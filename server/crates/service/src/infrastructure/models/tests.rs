use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionError, CompletionModel};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::instrument::WithSubscriber;

use super::probe::{PROBE_PNG_BASE64, structured_request, tools_request};
use super::rig_client::retry_after_ms;
use super::*;

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    delay: Duration,
}

impl Reply {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: body.to_string(),
            delay: Duration::ZERO,
        }
    }

    fn error(status: u16) -> Self {
        Self {
            status,
            // This is a synthetic leak sentinel, not a credential.
            body: json!({"error": {"message": "DO_NOT_LOG_PROVIDER_BODY", "code": "upstream_failure"}}).to_string(),
            ..Self::json(Value::Null)
        }
    }
}

struct Fixture {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    accepted: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Fixture {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let base_url = format!(
            "http://{}/v1",
            listener.local_addr().expect("fixture address")
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(Notify::new());
        let recorded = Arc::clone(&requests);
        let notified = Arc::clone(&accepted);
        let task = tokio::spawn(async move {
            let mut replies = VecDeque::from(replies);
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((stream, _)) = connection else { break };
                        let reply = replies.pop_front().unwrap_or_else(|| Reply::error(500));
                        connections.spawn(serve(stream, reply, Arc::clone(&recorded), Arc::clone(&notified)));
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            base_url,
            requests,
            accepted,
            task,
        }
    }

    fn client(&self) -> ModelClient {
        self.client_with_timeout(Duration::from_secs(2))
    }

    fn client_with_timeout(&self, timeout: Duration) -> ModelClient {
        ModelClient::for_fixture(ModelConfig {
            base_url: self.base_url.clone(),
            api_key: "synthetic-fixture-key".into(),
            model: "fixture-model".into(),
            timeout,
        })
        .unwrap_or_else(|error| panic!("fixture configuration: {error:?}"))
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("fixture records").clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut stream: TcpStream,
    reply: Reply,
    requests: Arc<Mutex<Vec<Value>>>,
    accepted: Arc<Notify>,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let Ok(count) = stream.read(&mut chunk).await else {
            return;
        };
        if count == 0 || bytes.len() > 64 * 1024 {
            return;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break offset + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, length)| length.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > 1024 * 1024 {
        return;
    }
    let correct_path = headers.starts_with("POST /v1/chat/completions HTTP/1.1");
    while bytes.len() < header_end + length {
        let Ok(count) = stream.read(&mut chunk).await else {
            return;
        };
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let mut body = serde_json::from_slice::<Value>(&bytes[header_end..header_end + length])
        .unwrap_or(Value::Null);
    body["fixture_correct_path"] = Value::Bool(correct_path);
    requests.lock().expect("fixture records").push(body);
    accepted.notify_one();
    tokio::time::sleep(reply.delay).await;
    let mut response = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in reply.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&reply.body);
    let _ = stream.write_all(response.as_bytes()).await;
}

fn response(content: &str) -> Value {
    json!({
        "id": "completion-fixture", "model": "fixture-model", "object": "chat.completion", "created": 1,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15, "prompt_tokens_details": {"cached_tokens": 4}}
    })
}

fn tool_response() -> Value {
    let mut result = response("");
    result["choices"][0]["finish_reason"] = json!("tool_calls");
    result["choices"][0]["message"]["content"] = Value::Null;
    result["choices"][0]["message"]["tool_calls"] = json!([
        {"id": "call-left", "type": "function", "function": {"name": "read_probe_value", "arguments": "{\"slot\":\"left\"}"}},
        {"id": "call-right", "type": "function", "function": {"name": "read_probe_value", "arguments": "{\"slot\":\"right\"}"}}
    ]);
    result
}

fn failure(result: Result<ModelOutput, ModelFailure>) -> ModelFailure {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected a classified failure"),
    }
}

async fn call(client: &ModelClient, request: ModelRequest) -> Result<ModelOutput, ModelFailure> {
    let (_cancel, cancellation) = watch::channel(false);
    client.complete(request, cancellation).await
}

#[tokio::test]
async fn text_request_uses_sdk_chat_route_and_preserves_usage() {
    let mut reply = Reply::json(response("R1_OK"));
    reply
        .headers
        .push(("x-request-id", "provider-fixture-id".into()));
    let fixture = Fixture::start(vec![reply]).await;
    let output = call(&fixture.client(), ModelRequest::text("test"))
        .await
        .expect("completion");
    assert_eq!(output.text, "R1_OK");
    assert_eq!(
        output.usage,
        Some(TokenUsage {
            input_tokens: 12,
            output_tokens: Some(3),
            cached_input_tokens: Some(4),
            total_tokens: 15
        })
    );
    assert!(output.provider_request_id_present);
    let requests = fixture.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "fixture-model");
    assert_eq!(requests[0]["fixture_correct_path"], true);
}

#[tokio::test]
async fn missing_usage_stays_unknown_and_missing_output_count_is_not_inferred() {
    let mut absent = response("ok");
    absent.as_object_mut().unwrap().remove("usage");
    let mut partial = response("ok");
    partial["usage"]
        .as_object_mut()
        .unwrap()
        .remove("completion_tokens");
    partial["usage"]
        .as_object_mut()
        .unwrap()
        .remove("prompt_tokens_details");
    let fixture = Fixture::start(vec![Reply::json(absent), Reply::json(partial)]).await;
    let client = fixture.client();
    assert!(
        call(&client, ModelRequest::text("test"))
            .await
            .expect("completion")
            .usage
            .is_none()
    );
    let usage = call(&client, ModelRequest::text("test"))
        .await
        .expect("completion")
        .usage
        .unwrap();
    assert_eq!(usage.output_tokens, None);
    assert_eq!(usage.cached_input_tokens, None);
    assert_eq!(usage.total_tokens, 15);
}

#[tokio::test]
async fn malformed_usage_is_discarded_from_failures() {
    for usage in [
        json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 3}),
        json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 16}),
        json!({"prompt_tokens": 12, "total_tokens": 3}),
        json!({"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15,
            "prompt_tokens_details": {"cached_tokens": 13}}),
    ] {
        let mut body = response("ok");
        body["usage"] = usage;
        let mut reply = Reply::json(body);
        reply.headers.push(("x-request-id", "usage-fixture".into()));
        let fixture = Fixture::start(vec![reply]).await;
        let error = failure(call(&fixture.client(), ModelRequest::text("test")).await);
        assert_eq!(error.kind, FailureKind::InvalidResponse);
        assert!(error.usage.is_none());
        assert_eq!(error.provider_http_status, Some(200));
        assert_eq!(error.remote_execution, RemoteExecution::ResponseReceived);
        assert_eq!(error.sdk_call_count, 1);
        assert!(error.provider_request_id_present);
        assert!(!error.retryable);
        assert_eq!(fixture.requests().len(), 1);
    }
}

#[tokio::test]
async fn early_output_rejections_discard_invalid_usage() {
    let mut refusal = response("ordinary text beside refusal");
    refusal["choices"][0]["message"]["refusal"] = json!("refused fixture");
    let mut filtered = response("");
    filtered["choices"][0]["finish_reason"] = json!("content_filter");
    let mut truncated = response("partial");
    truncated["choices"][0]["finish_reason"] = json!("length");
    let mut empty = response("ok");
    empty["choices"] = json!([]);
    for (mut body, expected) in [
        (refusal, FailureKind::Refused),
        (filtered, FailureKind::Refused),
        (truncated, FailureKind::Truncated),
        (empty, FailureKind::InvalidResponse),
    ] {
        body["usage"]["total_tokens"] = json!(3);
        let fixture = Fixture::start(vec![Reply::json(body)]).await;
        let error = failure(call(&fixture.client(), ModelRequest::text("test")).await);
        assert_eq!(error.kind, expected);
        assert!(error.usage.is_none());
    }
}

#[tokio::test]
async fn cached_usage_preserves_nonzero_counts_and_leaves_zero_or_missing_unknown() {
    for (details, expected_cached) in [
        (None, None),
        (Some(json!({})), None),
        (Some(json!({"cached_tokens": 0})), None),
        (Some(json!({"cached_tokens": 4})), Some(4)),
    ] {
        let mut body = response("ok");
        if let Some(details) = details {
            body["usage"]["prompt_tokens_details"] = details;
        } else {
            body["usage"]
                .as_object_mut()
                .unwrap()
                .remove("prompt_tokens_details");
        }
        let fixture = Fixture::start(vec![Reply::json(body)]).await;
        let output = call(&fixture.client(), ModelRequest::text("test"))
            .await
            .expect("completion");
        assert_eq!(
            output.usage,
            Some(TokenUsage {
                input_tokens: 12,
                output_tokens: Some(3),
                cached_input_tokens: expected_cached,
                total_tokens: 15,
            })
        );
    }
}

#[tokio::test]
async fn native_structured_output_is_validated_without_markdown_repair() {
    let fixture = Fixture::start(vec![
        Reply::json(response(r#"{"marker":"r1_sdk","count":2}"#)),
        Reply::json(response("```json\n{}\n```")),
        Reply::json(response(r#"{"marker":"r1_sdk","count":2,"extra":true}"#)),
    ])
    .await;
    let client = fixture.client();
    assert_eq!(
        call(&client, structured_request())
            .await
            .expect("structured")
            .structured,
        Some(json!({"marker":"r1_sdk","count":2}))
    );
    assert_eq!(
        failure(call(&client, structured_request()).await).kind,
        FailureKind::InvalidJson
    );
    assert_eq!(
        failure(call(&client, structured_request()).await).kind,
        FailureKind::SchemaMismatch
    );
    let requests = fixture.requests();
    assert_eq!(requests[0]["response_format"]["type"], "json_schema");
    assert_eq!(
        requests[0]["response_format"]["json_schema"]["strict"],
        true
    );
}

#[tokio::test]
async fn image_uses_sdk_multimodal_content() {
    let fixture = Fixture::start(vec![Reply::json(response("red,blue"))]).await;
    let mut request = ModelRequest::text("colors");
    request.png_base64 = Some(PROBE_PNG_BASE64.into());
    call(&fixture.client(), request)
        .await
        .expect("image completion");
    let requests = fixture.requests();
    assert_eq!(
        requests[0]["messages"][0]["content"][1]["type"],
        "image_url"
    );
    assert!(
        requests[0]["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[tokio::test]
async fn parallel_results_round_trip_ids_when_completion_order_changes() {
    let fixture = Fixture::start(vec![
        Reply::json(tool_response()),
        Reply::json(response("46")),
    ])
    .await;
    let client = fixture.client();
    let output = call(&client, tools_request()).await.expect("tool calls");
    assert_eq!(output.tool_calls.len(), 2);
    let results = vec![
        ModelToolResult {
            provider_tool_call_id: "call-right".into(),
            name: "read_probe_value".into(),
            content: "29".into(),
        },
        ModelToolResult {
            provider_tool_call_id: "call-left".into(),
            name: "read_probe_value".into(),
            content: "17".into(),
        },
    ];
    let (_cancel, cancellation) = watch::channel(false);
    let output = client
        .complete_tool_results(output, results, cancellation)
        .await
        .expect("follow-up");
    assert_eq!(output.text, "46");
    let requests = fixture.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["parallel_tool_calls"], true);
    let tools = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["tool_call_id"], "call-right");
    assert_eq!(tools[1]["tool_call_id"], "call-left");
    assert_eq!(tools[0]["content"], "29");
    assert_eq!(tools[1]["content"], "17");
}

#[tokio::test]
async fn tool_result_identity_mismatch_is_rejected_before_network() {
    let fixture = Fixture::start(vec![Reply::json(tool_response())]).await;
    let client = fixture.client();
    let output = call(&client, tools_request()).await.expect("tool calls");
    let (_cancel, cancellation) = watch::channel(false);
    let error = failure(
        client
            .complete_tool_results(output, vec![], cancellation)
            .await,
    );
    assert_eq!(error.kind, FailureKind::ToolResultMismatch);
    assert_eq!(error.sdk_call_count, 0);
    assert_eq!(fixture.requests().len(), 1);
}

#[tokio::test]
async fn duplicate_unknown_and_invalid_argument_tool_calls_are_not_executable() {
    for (field, value) in [
        ("id", json!("call-right")),
        ("name", json!("unknown_tool")),
        ("arguments", json!("{\"slot\":\"neither\"}")),
        ("id", json!("")),
    ] {
        let mut body = tool_response();
        let first = &mut body["choices"][0]["message"]["tool_calls"][0];
        if field == "id" {
            first[field] = value;
        } else {
            first["function"][field] = value;
        }
        let fixture = Fixture::start(vec![Reply::json(body)]).await;
        assert_eq!(
            failure(call(&fixture.client(), tools_request()).await).kind,
            FailureKind::InvalidToolCalls
        );
    }
}

#[tokio::test]
async fn rig_wraps_invalid_tool_argument_json_as_a_success_status_provider_error() {
    let mut body = tool_response();
    body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("{invalid");
    let mut reply = Reply::json(body);
    reply.headers.push(("x-request-id", "sdk-fixture".into()));
    let fixture = Fixture::start(vec![reply]).await;
    let http_client = reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .expect("fixture transport");
    let client = rig_core::providers::openai::CompletionsClient::builder()
        .api_key("synthetic-fixture-key")
        .base_url(&fixture.base_url)
        .http_client(http_client)
        .build()
        .expect("fixture SDK client");
    let model = client.completion_model("fixture-model");
    let request = model
        .completion_request("test")
        .record_content_telemetry(false)
        .build();
    let result = model
        .raw_completion_with_request_id(request)
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("expected SDK decoding failure"),
    };
    assert!(matches!(&error, CompletionError::ProviderResponse(_)));
    assert_eq!(
        error
            .provider_response_status()
            .map(|status| status.as_u16()),
        Some(200)
    );
    assert!(error.provider_request_id().is_some());
    let retained = error
        .provider_response_json()
        .expect("synthetic outer JSON")
        .expect("preserved synthetic payload");
    assert!(
        retained
            .pointer("/choices/0/message/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
            == Some("{invalid")
    );
    assert_eq!(fixture.requests().len(), 1);
}

#[tokio::test]
async fn broken_tool_argument_json_is_not_silently_dropped() {
    for (index, arguments) in ["{invalid", "{\"slot\":\"DO_NOT_LOG_TOOL_ARGUMENTS\""]
        .into_iter()
        .enumerate()
    {
        let mut body = tool_response();
        body["choices"][0]["message"]["tool_calls"][index]["function"]["arguments"] =
            json!(arguments);
        let mut reply = Reply::json(body);
        reply
            .headers
            .push(("x-request-id", "DO_NOT_LOG_REQUEST_ID".into()));
        reply.headers.push(("Retry-After", "7".into()));
        let fixture = Fixture::start(vec![reply]).await;
        let error = failure(call(&fixture.client(), tools_request()).await);
        assert_eq!(error.kind, FailureKind::InvalidJson);
        assert_eq!(error.provider_http_status, Some(200));
        assert_eq!(error.remote_execution, RemoteExecution::ResponseReceived);
        assert_eq!(error.sdk_call_count, 1);
        assert!(error.provider_request_id_present);
        assert_eq!(error.retry_after_ms, Some(7000));
        assert!(!error.retryable);
        assert!(error.usage.is_none());
        assert_eq!(fixture.requests().len(), 1);
        let serialized = serde_json::to_string(&error).expect("safe serialization");
        assert!(!serialized.contains("DO_NOT_LOG_TOOL_ARGUMENTS"));
        assert!(!serialized.contains("DO_NOT_LOG_REQUEST_ID"));
        assert!(!serialized.contains("synthetic-fixture-key"));
    }
}

#[tokio::test]
async fn explicit_provider_error_envelopes_keep_precedence_over_invalid_arguments() {
    for (key, value) in [
        ("error", json!({"message": "DO_NOT_LOG_PROVIDER_BODY"})),
        ("message", json!("DO_NOT_LOG_PROVIDER_BODY")),
    ] {
        let mut body = tool_response();
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("{invalid");
        body[key] = value;
        let fixture = Fixture::start(vec![Reply::json(body)]).await;
        let error = failure(call(&fixture.client(), tools_request()).await);
        assert_eq!(error.kind, FailureKind::ProviderRejected);
        assert_eq!(error.provider_http_status, Some(200));
        assert!(!error.retryable);
        assert_eq!(fixture.requests().len(), 1);
        assert!(
            !serde_json::to_string(&error)
                .expect("safe serialization")
                .contains("DO_NOT_LOG_PROVIDER_BODY")
        );
    }
}

#[tokio::test]
async fn non_success_status_keeps_precedence_over_invalid_arguments() {
    for (status, expected, retryable) in [
        (401, FailureKind::Authentication, false),
        (429, FailureKind::RateLimited, true),
        (503, FailureKind::ProviderUnavailable, true),
    ] {
        let mut body = tool_response();
        body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"] = json!("{invalid");
        let reply = Reply {
            status,
            ..Reply::json(body)
        };
        let fixture = Fixture::start(vec![reply]).await;
        let error = failure(call(&fixture.client(), tools_request()).await);
        assert_eq!(error.kind, expected);
        assert_eq!(error.provider_http_status, Some(status));
        assert_eq!(error.retryable, retryable);
        assert_eq!(fixture.requests().len(), 1);
    }
}

#[tokio::test]
async fn refusal_and_filtering_are_not_ordinary_text_success() {
    let mut refusal = response("ordinary text beside refusal");
    refusal["choices"][0]["message"]["refusal"] = json!("refused fixture");
    let mut filtered = response("");
    filtered["choices"][0]["finish_reason"] = json!("content_filter");
    for body in [refusal, filtered] {
        let fixture = Fixture::start(vec![Reply::json(body)]).await;
        let error = failure(call(&fixture.client(), ModelRequest::text("test")).await);
        assert_eq!(error.kind, FailureKind::Refused);
        assert_eq!(
            error.usage.as_ref().map(|usage| usage.total_tokens),
            Some(15)
        );
        assert!(!error.retryable);
    }
}

#[tokio::test]
async fn truncated_output_does_not_become_a_valid_score_candidate() {
    let mut body = response(r#"{"marker":"r1_sdk","count":2}"#);
    body["choices"][0]["finish_reason"] = json!("length");
    let fixture = Fixture::start(vec![Reply::json(body)]).await;
    let error = failure(call(&fixture.client(), structured_request()).await);
    assert_eq!(error.kind, FailureKind::Truncated);
    assert_eq!(
        error.usage.as_ref().map(|usage| usage.total_tokens),
        Some(15)
    );
}

#[tokio::test]
async fn http_errors_keep_classification_and_do_not_leak_or_retry() {
    for (status, expected, retryable) in [
        (401, FailureKind::Authentication, false),
        (403, FailureKind::PermissionDenied, false),
        (400, FailureKind::ProviderRejected, false),
        (429, FailureKind::RateLimited, true),
        (500, FailureKind::ProviderUnavailable, true),
        (503, FailureKind::ProviderUnavailable, true),
        (504, FailureKind::Timeout, true),
    ] {
        let mut reply = Reply::error(status);
        reply.headers.push(("Retry-After", "7".into()));
        reply
            .headers
            .push(("x-request-id", "request-fixture".into()));
        let fixture = Fixture::start(vec![reply]).await;
        let error = failure(call(&fixture.client(), ModelRequest::text("test")).await);
        assert_eq!(error.kind, expected);
        assert_eq!(error.provider_http_status, Some(status));
        assert_eq!(error.retryable, retryable);
        assert_eq!(error.retry_after_ms, Some(7000));
        assert_eq!(error.sdk_call_count, 1);
        assert!(error.provider_request_id_present);
        assert!(error.usage.is_none());
        assert_eq!(fixture.requests().len(), 1);
        let serialized = serde_json::to_string(&error).expect("safe serialization");
        assert!(!serialized.contains("DO_NOT_LOG_PROVIDER_BODY"));
        assert!(!serialized.contains("synthetic-fixture-key"));
    }
}

#[tokio::test]
async fn context_limit_and_success_status_error_envelope_are_failures() {
    let mut context = Reply::error(400);
    context.body =
        json!({"error":{"message":"too large","code":"context_length_exceeded"}}).to_string();
    let mut envelope = Reply::error(200);
    envelope.headers.push(("Retry-After", "9".into()));
    let fixture = Fixture::start(vec![context, envelope]).await;
    let client = fixture.client();
    assert_eq!(
        failure(call(&client, ModelRequest::text("test")).await).kind,
        FailureKind::ContextLimit
    );
    let error = failure(call(&client, ModelRequest::text("test")).await);
    assert_eq!(error.kind, FailureKind::ProviderRejected);
    assert_eq!(error.provider_http_status, Some(200));
    assert_eq!(error.retry_after_ms, Some(9000));
}

#[tokio::test]
async fn malformed_outer_json_and_empty_choices_fail_without_body_logging() {
    let mut invalid = Reply::json(Value::Null);
    invalid.body = "invalid DO_NOT_LOG_PROVIDER_BODY".into();
    let mut empty = response("ok");
    empty["choices"] = json!([]);
    let fixture = Fixture::start(vec![invalid, Reply::json(empty)]).await;
    let client = fixture.client();
    let error = failure(call(&client, ModelRequest::text("test")).await);
    assert_eq!(error.kind, FailureKind::InvalidJson);
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains("DO_NOT_LOG_PROVIDER_BODY")
    );
    assert_eq!(
        failure(call(&client, ModelRequest::text("test")).await).kind,
        FailureKind::InvalidResponse
    );
}

#[tokio::test]
async fn timeout_is_unknown_remote_execution_and_unknown_cost() {
    let mut reply = Reply::json(response("late"));
    reply.delay = Duration::from_secs(2);
    let fixture = Fixture::start(vec![reply]).await;
    let client = fixture.client_with_timeout(Duration::from_millis(75));
    let started = Instant::now();
    let error = failure(call(&client, ModelRequest::text("test")).await);
    assert_eq!(error.kind, FailureKind::Timeout);
    assert_eq!(error.remote_execution, RemoteExecution::Unknown);
    assert!(error.usage.is_none());
    assert_eq!(error.sdk_call_count, 1);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(fixture.requests().len(), 1);
}

#[tokio::test]
async fn cancellation_before_dispatch_sends_nothing() {
    let fixture = Fixture::start(vec![]).await;
    let (_cancel, cancellation) = watch::channel(true);
    let error = failure(
        fixture
            .client()
            .complete(ModelRequest::text("test"), cancellation)
            .await,
    );
    assert_eq!(error.kind, FailureKind::Cancelled);
    assert_eq!(error.remote_execution, RemoteExecution::NotStarted);
    assert_eq!(error.sdk_call_count, 0);
    assert!(fixture.requests().is_empty());
}

#[tokio::test]
async fn inflight_cancellation_stops_waiting_without_claiming_provider_stopped() {
    let mut reply = Reply::json(response("late"));
    reply.delay = Duration::from_secs(2);
    let fixture = Fixture::start(vec![reply]).await;
    let client = fixture.client();
    let (cancel, cancellation) = watch::channel(false);
    let request = tokio::spawn(async move {
        client
            .complete(ModelRequest::text("test"), cancellation)
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), fixture.accepted.notified())
        .await
        .expect("request received");
    cancel.send(true).expect("cancel sent");
    let error = failure(
        tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .expect("prompt cancellation")
            .expect("probe task"),
    );
    assert_eq!(error.kind, FailureKind::Cancelled);
    assert_eq!(error.remote_execution, RemoteExecution::Unknown);
    assert_eq!(error.sdk_call_count, 1);
    assert!(!error.retryable);
    assert!(error.usage.is_none());
    assert_eq!(fixture.requests().len(), 1);
}

#[tokio::test]
async fn disconnected_cancellation_owner_does_not_leave_a_request_running() {
    let fixture = Fixture::start(vec![]).await;
    let (cancel, cancellation) = watch::channel(false);
    drop(cancel);
    let error = failure(
        fixture
            .client()
            .complete(ModelRequest::text("test"), cancellation)
            .await,
    );
    assert_eq!(error.kind, FailureKind::Cancelled);
    assert_eq!(error.sdk_call_count, 0);
}

#[tokio::test]
async fn local_input_budget_fails_before_spending_provider_tokens() {
    let fixture = Fixture::start(vec![]).await;
    let error = failure(call(&fixture.client(), ModelRequest::text("x".repeat(65 * 1024))).await);
    assert_eq!(error.kind, FailureKind::InputTooLarge);
    assert_eq!(error.sdk_call_count, 0);
    assert_eq!(error.remote_execution, RemoteExecution::NotStarted);
    assert!(fixture.requests().is_empty());
}

#[test]
fn retry_after_supports_seconds_http_date_and_invalid_values() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    assert_eq!(retry_after_ms("7", now), Some(7000));
    assert_eq!(
        retry_after_ms(&httpdate::fmt_http_date(now + Duration::from_secs(17)), now),
        Some(17000)
    );
    assert_eq!(
        retry_after_ms(&httpdate::fmt_http_date(now - Duration::from_secs(1)), now),
        Some(0)
    );
    for invalid in ["-1", "0.5", "bad", "18446744073709551615"] {
        assert_eq!(retry_after_ms(invalid, now), None);
    }
}

#[test]
fn live_configuration_rejects_cleartext_and_url_credentials() {
    for base_url in [
        "http://provider.example/v1",
        "https://secret@provider.example/v1",
        "https://provider.example/v1?key=secret",
    ] {
        let client = ModelClient::new(ModelConfig {
            base_url: base_url.into(),
            api_key: "fixture-key".into(),
            model: "fixture".into(),
            timeout: Duration::from_secs(1),
        });
        assert!(matches!(
            client,
            Err(ModelFailure {
                kind: FailureKind::InvalidConfiguration,
                ..
            })
        ));
    }
}
