//! 有调用预算的真实模型验证，使用合成输入，仅输出脱敏后的结果元信息。

use std::io::{self, Write};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;
use tokio::sync::watch;

use super::{
    ModelClient, ModelConfig, ModelFailure, ModelOutput, ModelRequest, ModelTool, ModelToolResult,
    TokenUsage,
};

// Generated fixture: a 96x48 RGB PNG, solid red left half and solid blue right half.
pub(super) const PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAGAAAAAwCAIAAABhdOiYAAAAcUlEQVR4nO3QQQnAABDEwPNvuhVQAdnCQP6Bueduqvr/qf4DAgQI0FL1HxAgQICmqv+AAAECtFT9BwQIEKCp6j8gQIAALVX/AQECBGiq+g8IECBAS9V/QIAAAZqq/gMCBAjQUvUfECBAgKaq/4B+DvQC/ozvAMlEKtoAAAAASUVORK5CYII=";

#[derive(Serialize)]
struct ProbeRecord {
    case: &'static str,
    model_slot: &'static str,
    status: &'static str,
    elapsed_ms: u64,
    sdk_call_count: u8,
    usage: Option<TokenUsage>,
    usage_complete: bool,
    provider_request_id_present: bool,
    tool_call_count: usize,
    check: &'static str,
    error: Option<ModelFailure>,
}

impl ProbeRecord {
    fn result(
        case: &'static str,
        model_slot: &'static str,
        started: Instant,
        result: &Result<ModelOutput, ModelFailure>,
        check: &'static str,
        matches_expected: bool,
    ) -> Self {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match result {
            Ok(output) => Self {
                case,
                model_slot,
                status: if matches_expected { "passed" } else { "failed" },
                elapsed_ms,
                sdk_call_count: 1,
                usage: output.usage.clone(),
                usage_complete: output
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens.is_some()),
                provider_request_id_present: output.provider_request_id_present,
                tool_call_count: output.tool_calls.len(),
                check,
                error: None,
            },
            Err(error) => Self {
                case,
                model_slot,
                status: "failed",
                elapsed_ms,
                sdk_call_count: error.sdk_call_count,
                usage: error.usage.clone(),
                usage_complete: error
                    .usage
                    .as_ref()
                    .is_some_and(|usage| usage.output_tokens.is_some()),
                provider_request_id_present: error.provider_request_id_present,
                tool_call_count: 0,
                check,
                error: Some(error.clone()),
            },
        }
    }
}

fn emit(record: &impl Serialize) -> io::Result<()> {
    let encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&encoded)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

pub(super) fn structured_request() -> ModelRequest {
    let mut request = ModelRequest::text(
        "Return an object with marker exactly r1_sdk and count exactly 2. Do not include other fields.",
    );
    request.output_schema = Some(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "marker": {"type": "string", "enum": ["r1_sdk"]},
            "count": {"type": "integer", "enum": [2]}
        },
        "required": ["marker", "count"]
    }));
    request
}

pub(super) fn tools_request() -> ModelRequest {
    let mut request = ModelRequest::text(
        "In the same response, call read_probe_value twice: once with slot left and once with slot right. Do not guess the values. After both tool results arrive, return only the decimal sum, without any other text.",
    );
    request.require_tools = true;
    request.tools = vec![ModelTool {
        name: "read_probe_value".into(),
        description:
            "Read one immutable synthetic probe value. This local probe tool has no side effects."
                .into(),
        parameters: json!({
            "type": "object", "additionalProperties": false,
            "properties": {"slot": {"type": "string", "enum": ["left", "right"]}},
            "required": ["slot"]
        }),
    }];
    request
}

fn paired_calls(output: &ModelOutput) -> bool {
    output.tool_calls.len() == 2
        && ["left", "right"].iter().all(|slot| {
            output
                .tool_calls
                .iter()
                .filter(|call| call.arguments["slot"] == *slot)
                .count()
                == 1
        })
}

async fn run_slot(
    model_slot: &'static str,
    cancellation: watch::Receiver<bool>,
    records: &mut Vec<ProbeRecord>,
) -> io::Result<()> {
    let prefix = if model_slot == "text" {
        "R1_TEXT"
    } else {
        "R1_VISION"
    };
    let config = match ModelConfig::from_env(prefix) {
        Ok(config) => config,
        Err(error) => {
            let record = ProbeRecord::result(
                "configuration",
                model_slot,
                Instant::now(),
                &Err(error),
                "explicit_environment_configuration",
                false,
            );
            emit(&record)?;
            records.push(record);
            return Ok(());
        }
    };
    // Model names are configuration, never provider-supplied raw response strings.
    emit(
        &json!({"case": "model_configuration", "model_slot": model_slot, "model_name": config.model_name()}),
    )?;
    let client = match ModelClient::new(config) {
        Ok(client) => client,
        Err(error) => {
            let record = ProbeRecord::result(
                "configuration",
                model_slot,
                Instant::now(),
                &Err(error),
                "valid_transport_configuration",
                false,
            );
            emit(&record)?;
            records.push(record);
            return Ok(());
        }
    };

    let started = Instant::now();
    let mut request = if model_slot == "text" {
        ModelRequest::text("Return exactly R1_OK, no explanation or punctuation.")
    } else {
        ModelRequest::text(
            "Describe the left half and right half colors in this image. Reply with only two lowercase English color names separated by a comma, left first.",
        )
    };
    if model_slot == "vision" {
        request.png_base64 = Some(PROBE_PNG_BASE64.into());
    }
    let result = client.complete(request, cancellation.clone()).await;
    let matched = result.as_ref().is_ok_and(|output| {
        if model_slot == "text" {
            output.text.trim() == "R1_OK"
        } else {
            output
                .text
                .to_lowercase()
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>()
                == ["red", "blue"]
        }
    });
    let record = ProbeRecord::result(
        "text_or_image",
        model_slot,
        started,
        &result,
        "known_synthetic_answer",
        matched,
    );
    emit(&record)?;
    records.push(record);

    let started = Instant::now();
    let mut request = structured_request();
    if model_slot == "vision" {
        request.prompt = "Read this image and return left_color and right_color as lowercase English color names. No other fields.".into();
        request.png_base64 = Some(PROBE_PNG_BASE64.into());
        request.output_schema = Some(json!({
            "type": "object", "additionalProperties": false,
            "properties": {"left_color": {"type": "string"}, "right_color": {"type": "string"}},
            "required": ["left_color", "right_color"]
        }));
    }
    let result = client.complete(request, cancellation.clone()).await;
    let expected = if model_slot == "text" {
        json!({"marker": "r1_sdk", "count": 2})
    } else {
        json!({"left_color": "red", "right_color": "blue"})
    };
    let matched = result
        .as_ref()
        .is_ok_and(|output| output.structured.as_ref() == Some(&expected));
    let record = ProbeRecord::result(
        "structured_output",
        model_slot,
        started,
        &result,
        "native_schema_and_local_validation",
        matched,
    );
    emit(&record)?;
    records.push(record);

    let started = Instant::now();
    let mut request = tools_request();
    if model_slot == "vision" {
        request.png_base64 = Some(PROBE_PNG_BASE64.into());
    }
    let result = client.complete(request, cancellation.clone()).await;
    let matched = result.as_ref().is_ok_and(paired_calls);
    let record = ProbeRecord::result(
        "parallel_tool_calls",
        model_slot,
        started,
        &result,
        "two_unique_ids_and_distinct_arguments",
        matched,
    );
    emit(&record)?;
    records.push(record);
    if !matched {
        let record = ProbeRecord {
            case: "tool_results",
            model_slot,
            status: "blocked",
            elapsed_ms: 0,
            sdk_call_count: 0,
            usage: None,
            usage_complete: false,
            provider_request_id_present: false,
            tool_call_count: 0,
            check: "parallel_tool_calls_required",
            error: None,
        };
        emit(&record)?;
        records.push(record);
        return Ok(());
    }
    if let Ok(output) = result {
        let mut calls = output.tool_calls.iter();
        if let (Some(first), Some(second)) = (calls.next(), calls.next()) {
            let tool_result = |call: super::ModelToolCall, delay_ms| async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                ModelToolResult {
                    provider_tool_call_id: call.provider_tool_call_id,
                    name: call.name,
                    content: json!({"value": if call.arguments["slot"] == "left" {17} else {29}})
                        .to_string(),
                }
            };
            let (first_result, second_result) = tokio::join!(
                tool_result(first.clone(), 5),
                tool_result(second.clone(), 1)
            );
            let started = Instant::now();
            let result = client
                .complete_tool_results(output, vec![second_result, first_result], cancellation)
                .await;
            let matched = result
                .as_ref()
                .is_ok_and(|output| output.text.trim() == "46" && output.tool_calls.is_empty());
            let record = ProbeRecord::result(
                "tool_results",
                model_slot,
                started,
                &result,
                "reverse_completion_order_preserves_ids_and_sum",
                matched,
            );
            emit(&record)?;
            records.push(record);
        }
    }
    Ok(())
}

/// 最多调用 SDK 八次，不自动重试。故障样例另行验证。
pub async fn run_from_env(cancellation: watch::Receiver<bool>) -> io::Result<bool> {
    emit(&json!({
        "case": "probe_start", "sdk": "rig-core", "sdk_version": "0.42.0",
        "transport_version": "0.13.5", "automatic_retries": 0,
        "scope": "r1_compatibility", "maximum_sdk_calls": 8
    }))?;
    let mut records = Vec::new();
    run_slot("text", cancellation.clone(), &mut records).await?;
    run_slot("vision", cancellation, &mut records).await?;
    let passed = records.len() == 8 && records.iter().all(|record| record.status == "passed");
    let total_known_tokens: u64 = records
        .iter()
        .filter_map(|record| record.usage.as_ref())
        .map(|usage| usage.total_tokens)
        .sum();
    emit(&json!({
        "case": "probe_summary", "passed": passed,
        "case_count": records.len(),
        "sdk_call_count": records.iter().map(|record| u64::from(record.sdk_call_count)).sum::<u64>(),
        "cases_with_unknown_usage": records.iter().filter(|record| record.sdk_call_count > 0 && !record.usage_complete).count(),
        "known_total_tokens": total_known_tokens,
        "billing_scope": "review_service_probe_only",
        "cancellation": "local_wait_only_remote_execution_and_billing_may_continue",
        "not_verified": ["provider_context_window_size", "capacity", "streaming", "full_runtime"]
    }))?;
    Ok(passed)
}
