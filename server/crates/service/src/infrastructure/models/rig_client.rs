use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionError, CompletionModel, ToolDefinition};
use rig_core::message::{ImageMediaType, Message, ToolChoice, ToolResultContent, UserContent};
use rig_core::providers::openai;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;
use tracing::instrument::WithSubscriber;

/// 凭据不实现 Debug 或 Serialize，也不从 dotenv 文件补充配置。
pub struct ModelConfig {
    pub(super) base_url: String,
    pub(super) api_key: String,
    pub(super) model: String,
    pub(super) timeout: Duration,
}

impl ModelConfig {
    /// 读取隔离验证程序注入的 R1_TEXT_* 或 R1_VISION_* 配置。
    pub fn from_env(prefix: &str) -> Result<Self, ModelFailure> {
        if !matches!(prefix, "R1_TEXT" | "R1_VISION") {
            return Err(ModelFailure::not_started(FailureKind::InvalidConfiguration));
        }
        let required = |suffix| {
            std::env::var(format!("{prefix}_{suffix}"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| ModelFailure::not_started(FailureKind::InvalidConfiguration))
        };
        let timeout_seconds = match std::env::var("R1_MODEL_TIMEOUT_SECONDS") {
            Ok(value) => value.parse::<u64>().ok().filter(|n| (1..=180).contains(n)),
            Err(std::env::VarError::NotPresent) => Some(60),
            Err(_) => None,
        }
        .ok_or_else(|| ModelFailure::not_started(FailureKind::InvalidConfiguration))?;
        Ok(Self {
            base_url: required("BASE_URL")?,
            api_key: required("API_KEY")?,
            model: required("MODEL")?,
            timeout: Duration::from_secs(timeout_seconds),
        })
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    InvalidConfiguration,
    InvalidRequest,
    InputTooLarge,
    ContextLimit,
    Authentication,
    PermissionDenied,
    RateLimited,
    ProviderUnavailable,
    ProviderRejected,
    Transport,
    Timeout,
    Cancelled,
    InvalidResponse,
    InvalidJson,
    SchemaMismatch,
    Refused,
    Truncated,
    InvalidToolCalls,
    ToolResultMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteExecution {
    NotStarted,
    ResponseReceived,
    /// 丢弃本地异步任务不代表远端已经取消，也不代表没有发生计费。
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: Option<u64>,
    /// 缓存输入属于 input_tokens，不再额外累加到 total_tokens。
    /// Rig 0.42 无法区分缺失与零值，R1 将两者都保留为未知。
    pub cached_input_tokens: Option<u64>,
    pub total_tokens: u64,
}

/// 可记录到日志的错误，不包含 SDK Display、供应商原始正文、URL 或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelFailure {
    pub kind: FailureKind,
    pub provider_http_status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    pub retryable: bool,
    pub sdk_call_count: u8,
    pub remote_execution: RemoteExecution,
    pub usage: Option<TokenUsage>,
    pub provider_request_id_present: bool,
}

impl ModelFailure {
    pub(super) fn not_started(kind: FailureKind) -> Self {
        Self {
            kind,
            provider_http_status: None,
            retry_after_ms: None,
            retryable: false,
            sdk_call_count: 0,
            remote_execution: RemoteExecution::NotStarted,
            usage: None,
            provider_request_id_present: false,
        }
    }

    fn waiting(kind: FailureKind) -> Self {
        Self {
            retryable: kind == FailureKind::Timeout,
            sdk_call_count: 1,
            remote_execution: RemoteExecution::Unknown,
            ..Self::not_started(kind)
        }
    }
}

#[derive(Clone)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone)]
pub struct ModelToolCall {
    /// 供应商关联 ID，不作为 Runtime 执行 ID 或授权凭据。
    pub provider_tool_call_id: String,
    pub name: String,
    pub arguments: Value,
}

pub struct ModelToolResult {
    pub provider_tool_call_id: String,
    pub name: String,
    pub content: String,
}

pub struct ModelRequest {
    pub prompt: String,
    pub png_base64: Option<String>,
    pub output_schema: Option<Value>,
    pub tools: Vec<ModelTool>,
    pub require_tools: bool,
    pub max_output_tokens: u64,
}

impl ModelRequest {
    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            png_base64: None,
            output_schema: None,
            tools: Vec::new(),
            require_tools: false,
            max_output_tokens: 256,
        }
    }
}

/// SDK 类型及隐藏推理仅保留在私有内存中，用于工具结果回传后的继续生成。
pub struct ModelOutput {
    pub text: String,
    pub structured: Option<Value>,
    pub tool_calls: Vec<ModelToolCall>,
    pub usage: Option<TokenUsage>,
    pub provider_request_id_present: bool,
    history: Vec<Message>,
    tools: Vec<ModelTool>,
}

pub struct ModelClient {
    model: openai::CompletionModel,
    timeout: Duration,
}

impl ModelClient {
    pub fn new(config: ModelConfig) -> Result<Self, ModelFailure> {
        Self::build(config, false)
    }

    #[cfg(test)]
    pub(super) fn for_fixture(config: ModelConfig) -> Result<Self, ModelFailure> {
        Self::build(config, true)
    }

    fn build(config: ModelConfig, loopback_fixture: bool) -> Result<Self, ModelFailure> {
        let invalid = || ModelFailure::not_started(FailureKind::InvalidConfiguration);
        let url = reqwest::Url::parse(&config.base_url).map_err(|_| invalid())?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if !(url.scheme() == "https" || loopback_fixture && loopback && url.scheme() == "http")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
            || config.api_key.trim().is_empty()
            || config.model.trim().is_empty()
            || config.model.len() > 128
            || config.model.chars().any(char::is_control)
            || config.timeout.is_zero()
            || config.timeout > Duration::from_secs(180)
        {
            return Err(invalid());
        }
        // No reqwest protocol retries and no Rig RetryClient/agent loop.
        let http_client = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10).min(config.timeout))
            .timeout(config.timeout)
            .build()
            .map_err(|_| invalid())?;
        let client = openai::CompletionsClient::builder()
            .api_key(config.api_key)
            .base_url(config.base_url.trim_end_matches('/'))
            .http_client(http_client)
            .build()
            .map_err(|_| invalid())?;
        Ok(Self {
            model: client.completion_model(config.model),
            timeout: config.timeout,
        })
    }

    /// 只调用 SDK 一次，不执行工具、重试、评分或持久化。
    pub async fn complete(
        &self,
        request: ModelRequest,
        cancellation: watch::Receiver<bool>,
    ) -> Result<ModelOutput, ModelFailure> {
        validate_request(&request)?;
        let mut content = vec![UserContent::text(request.prompt.clone())];
        if let Some(image) = &request.png_base64 {
            content.push(UserContent::image_base64(
                image.clone(),
                Some(ImageMediaType::PNG),
                None,
            ));
        }
        let history = vec![Message::User { content }];
        self.send(request, history, cancellation).await
    }

    /// 并行工具完成顺序变化时按 ID 匹配，不依赖数组位置。
    pub async fn complete_tool_results(
        &self,
        previous: ModelOutput,
        results: Vec<ModelToolResult>,
        cancellation: watch::Receiver<bool>,
    ) -> Result<ModelOutput, ModelFailure> {
        let mismatch = || ModelFailure::not_started(FailureKind::ToolResultMismatch);
        if previous.tool_calls.is_empty() || results.len() != previous.tool_calls.len() {
            return Err(mismatch());
        }
        let mut seen = HashSet::new();
        let mut content = Vec::new();
        for result in results {
            if !seen.insert(result.provider_tool_call_id.clone())
                || !previous.tool_calls.iter().any(|call| {
                    call.provider_tool_call_id == result.provider_tool_call_id
                        && call.name == result.name
                })
                || result.content.len() > 16 * 1024
            {
                return Err(mismatch());
            }
            content.push(UserContent::tool_result_from_wire(
                result.provider_tool_call_id,
                result.name,
                vec![ToolResultContent::text(result.content)],
            ));
        }
        let mut history = previous.history;
        history.push(Message::User { content });
        let mut request = ModelRequest::text("tool continuation");
        request.tools = previous.tools;
        self.send(request, history, cancellation).await
    }

    async fn send(
        &self,
        request: ModelRequest,
        history: Vec<Message>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<ModelOutput, ModelFailure> {
        if *cancellation.borrow() || cancellation.has_changed().is_err() {
            return Err(ModelFailure::not_started(FailureKind::Cancelled));
        }
        let Some((prompt, earlier)) = history.split_last() else {
            return Err(ModelFailure::not_started(FailureKind::InvalidRequest));
        };
        let mut builder = self
            .model
            .completion_request(prompt.clone())
            .messages(earlier.iter().cloned())
            .max_tokens(request.max_output_tokens)
            .record_content_telemetry(false)
            .tools(
                request
                    .tools
                    .iter()
                    .map(|tool| ToolDefinition {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        parameters: tool.parameters.clone(),
                    })
                    .collect(),
            );
        if request.require_tools {
            builder = builder
                .tool_choice(ToolChoice::Required)
                .additional_params(serde_json::json!({"parallel_tool_calls": true}));
        }
        if let Some(schema) = &request.output_schema {
            let schema = serde_json::from_value(schema.clone())
                .map_err(|_| ModelFailure::not_started(FailureKind::InvalidRequest))?;
            builder = builder.output_schema(schema);
        }
        let sdk_request = builder.build();
        // Some Rig error/trace paths log full bodies. Disable those events for this future.
        let operation = self
            .model
            .raw_completion_with_request_id(sdk_request)
            .with_subscriber(tracing::subscriber::NoSubscriber::default());
        let raw = tokio::select! {
            biased;
            _ = wait_for_cancel(&mut cancellation) => {
                return Err(ModelFailure::waiting(FailureKind::Cancelled));
            }
            result = tokio::time::timeout(self.timeout, operation) => {
                match result {
                    Ok(result) => result.map_err(classify_sdk_error)?,
                    Err(_) => return Err(ModelFailure::waiting(FailureKind::Timeout)),
                }
            }
        };
        map_response(raw.0, raw.1.is_some(), request, history)
    }
}

async fn wait_for_cancel(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow_and_update() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn validate_request(request: &ModelRequest) -> Result<(), ModelFailure> {
    let invalid = || ModelFailure::not_started(FailureKind::InvalidRequest);
    if request.prompt.trim().is_empty()
        || !(1..=4096).contains(&request.max_output_tokens)
        || request.tools.len() > 8
        || request.require_tools && request.tools.is_empty()
        || request.output_schema.is_some() && !request.tools.is_empty()
    {
        return Err(invalid());
    }
    // A probe input budget, not an assertion about the model's token context window.
    if request.prompt.len() > 64 * 1024
        || request
            .png_base64
            .as_ref()
            .is_some_and(|image| image.len() > 1024 * 1024)
    {
        return Err(ModelFailure::not_started(FailureKind::InputTooLarge));
    }
    if let Some(schema) = &request.output_schema {
        jsonschema::validator_for(schema).map_err(|_| invalid())?;
    }
    let mut names = HashSet::new();
    for tool in &request.tools {
        if tool.name.trim().is_empty() || !names.insert(&tool.name) {
            return Err(invalid());
        }
        jsonschema::validator_for(&tool.parameters).map_err(|_| invalid())?;
    }
    Ok(())
}

fn map_response(
    raw: openai::CompletionResponse,
    provider_request_id_present: bool,
    request: ModelRequest,
    mut history: Vec<Message>,
) -> Result<ModelOutput, ModelFailure> {
    let usage = raw.usage.as_ref().map(|usage| TokenUsage {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.completion_tokens.map(|count| count as u64),
        total_tokens: usage.total_tokens as u64,
        cached_input_tokens: usage
            .prompt_tokens_details
            .as_ref()
            .map(|detail| detail.cached_tokens as u64)
            .filter(|count| *count > 0),
    });
    let invalid_usage = usage.as_ref().is_some_and(|usage| {
        usage
            .cached_input_tokens
            .is_some_and(|cached| cached > usage.input_tokens)
            || usage.total_tokens < usage.input_tokens
            || usage.output_tokens.is_some_and(|output| {
                usage.input_tokens.checked_add(output) != Some(usage.total_tokens)
            })
    });
    // Early output rejections must also exclude invalid usage from probe totals.
    let usage = usage.filter(|_| !invalid_usage);
    let fail = |kind| ModelFailure {
        provider_http_status: Some(200),
        sdk_call_count: 1,
        remote_execution: RemoteExecution::ResponseReceived,
        usage: usage.clone(),
        provider_request_id_present,
        ..ModelFailure::not_started(kind)
    };
    if raw.choices.len() != 1 {
        return Err(fail(FailureKind::InvalidResponse));
    }
    let choice = raw
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| fail(FailureKind::InvalidResponse))?;
    match choice.finish_reason.as_str() {
        "length" => return Err(fail(FailureKind::Truncated)),
        "content_filter" => return Err(fail(FailureKind::Refused)),
        "stop" | "tool_calls" => {}
        _ => return Err(fail(FailureKind::InvalidResponse)),
    }
    let openai::Message::Assistant {
        content,
        refusal,
        tool_calls,
        ..
    } = &choice.message
    else {
        return Err(fail(FailureKind::InvalidResponse));
    };
    if refusal.as_ref().is_some_and(|text| !text.is_empty())
        || content
            .iter()
            .any(|item| matches!(item, openai::AssistantContent::Refusal { .. }))
    {
        return Err(fail(FailureKind::Refused));
    }
    if invalid_usage {
        return Err(fail(FailureKind::InvalidResponse));
    }
    let text = content
        .iter()
        .filter_map(|part| match part {
            openai::AssistantContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut ids = HashSet::new();
    let mut calls = Vec::new();
    for call in tool_calls {
        let definition = request
            .tools
            .iter()
            .find(|tool| tool.name == call.function.name);
        if call.id.trim().is_empty() || !ids.insert(call.id.clone()) || definition.is_none() {
            return Err(fail(FailureKind::InvalidToolCalls));
        }
        if let Some(definition) = definition {
            let validator = jsonschema::validator_for(&definition.parameters)
                .map_err(|_| fail(FailureKind::InvalidToolCalls))?;
            if !validator.is_valid(&call.function.arguments) {
                return Err(fail(FailureKind::InvalidToolCalls));
            }
        }
        calls.push(ModelToolCall {
            provider_tool_call_id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        });
    }
    if request.require_tools && calls.is_empty()
        || choice.finish_reason == "tool_calls" && calls.is_empty()
        || !calls.is_empty() && choice.finish_reason != "tool_calls"
    {
        return Err(fail(FailureKind::InvalidToolCalls));
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Err(fail(FailureKind::InvalidResponse));
    }
    let structured = if let Some(schema) = &request.output_schema {
        let value: Value =
            serde_json::from_str(&text).map_err(|_| fail(FailureKind::InvalidJson))?;
        let validator =
            jsonschema::validator_for(schema).map_err(|_| fail(FailureKind::SchemaMismatch))?;
        if !validator.is_valid(&value) {
            return Err(fail(FailureKind::SchemaMismatch));
        }
        Some(value)
    } else {
        None
    };
    history.push(
        choice
            .message
            .try_into()
            .map_err(|_| fail(FailureKind::InvalidResponse))?,
    );
    Ok(ModelOutput {
        text,
        structured,
        tool_calls: calls,
        usage,
        provider_request_id_present,
        history,
        tools: request.tools,
    })
}

fn has_invalid_tool_argument_json(body: &Value) -> bool {
    if ["error", "message"]
        .iter()
        .any(|key| body.get(*key).is_some_and(|value| !value.is_null()))
    {
        return false;
    }
    body.get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                choice
                    .pointer("/message/tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| {
                        calls.iter().any(|call| {
                            call.pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .is_some_and(|arguments| {
                                    !arguments.trim().is_empty()
                                        && serde_json::from_str::<Value>(arguments).is_err()
                                })
                        })
                    })
            })
        })
}

fn classify_sdk_error(error: CompletionError) -> ModelFailure {
    let status = error
        .provider_response_status()
        .map(|status| status.as_u16());
    let provider_body = error.provider_response_json().ok().flatten();
    let provider_code = provider_body
        .as_ref()
        .and_then(|body| body.pointer("/error/code"))
        .and_then(Value::as_str);
    let kind = match status {
        Some(401) => FailureKind::Authentication,
        Some(403) => FailureKind::PermissionDenied,
        Some(429) => FailureKind::RateLimited,
        Some(408 | 504) => FailureKind::Timeout,
        Some(500..=599) => FailureKind::ProviderUnavailable,
        Some(400 | 413)
            if matches!(
                provider_code,
                Some("context_length_exceeded" | "context_window_exceeded")
            ) =>
        {
            FailureKind::ContextLimit
        }
        // Rig's untagged envelope wraps failed completion decoding as ProviderResponse.
        Some(200..=299)
            if matches!(&error, CompletionError::ProviderResponse(_))
                && provider_body
                    .as_ref()
                    .is_some_and(has_invalid_tool_argument_json) =>
        {
            FailureKind::InvalidJson
        }
        Some(_) => FailureKind::ProviderRejected,
        None => match &error {
            CompletionError::JsonError(_) => FailureKind::InvalidJson,
            CompletionError::ResponseError(_) => FailureKind::InvalidResponse,
            CompletionError::HttpError(rig_core::http_client::Error::Instance(source)) => {
                if source
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(reqwest::Error::is_timeout)
                {
                    FailureKind::Timeout
                } else {
                    FailureKind::Transport
                }
            }
            CompletionError::HttpError(_) => FailureKind::Transport,
            CompletionError::RequestError(_) | CompletionError::UrlError(_) => {
                FailureKind::InvalidRequest
            }
            _ => FailureKind::ProviderRejected,
        },
    };
    ModelFailure {
        provider_http_status: status,
        retry_after_ms: error
            .provider_response_headers()
            .and_then(|headers| headers.get("retry-after"))
            .and_then(|header| header.to_str().ok())
            .and_then(|header| retry_after_ms(header, SystemTime::now())),
        retryable: matches!(
            kind,
            FailureKind::RateLimited
                | FailureKind::ProviderUnavailable
                | FailureKind::Timeout
                | FailureKind::Transport
        ),
        sdk_call_count: 1,
        remote_execution: if status.is_some() {
            RemoteExecution::ResponseReceived
        } else {
            RemoteExecution::Unknown
        },
        provider_request_id_present: error.provider_request_id().is_some(),
        ..ModelFailure::not_started(kind)
    }
}

pub(super) fn retry_after_ms(header: &str, now: SystemTime) -> Option<u64> {
    let header = header.trim();
    if !header.is_empty() && header.bytes().all(|byte| byte.is_ascii_digit()) {
        return header.parse::<u64>().ok()?.checked_mul(1000);
    }
    let deadline = httpdate::parse_http_date(header).ok()?;
    let remaining = deadline.duration_since(now).unwrap_or_default();
    u64::try_from(remaining.as_millis()).ok()
}
