//! Helpers shared by the integration tests.

#![allow(dead_code)]

use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::operation::converse::ConverseOutput;
use aws_sdk_bedrockruntime::types::{ConverseMetrics, StopReason, TokenUsage};
use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_mocks::{Rule, mock};
use aws_smithy_runtime_api::http::{Response, StatusCode};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};

/// A `Converse` response with 10 uncached, 3 cache-read and 2 cache-write input tokens, and 5
/// output tokens.
pub fn converse_output() -> ConverseOutput {
    ConverseOutput::builder()
        .stop_reason(StopReason::EndTurn)
        .usage(
            TokenUsage::builder()
                .input_tokens(10)
                .output_tokens(5)
                .total_tokens(20)
                .cache_read_input_tokens(3)
                .cache_write_input_tokens(2)
                .build()
                .unwrap(),
        )
        .metrics(ConverseMetrics::builder().latency_ms(1).build().unwrap())
        .build()
        .unwrap()
}

pub fn message(
    message_type: &'static str,
    type_header: &'static str,
    name: &str,
    payload: &str,
) -> Message {
    Message::new(payload.as_bytes().to_vec())
        .add_header(Header::new(
            ":message-type",
            HeaderValue::String(message_type.into()),
        ))
        .add_header(Header::new(
            type_header,
            HeaderValue::String(name.to_owned().into()),
        ))
        .add_header(Header::new(
            ":content-type",
            HeaderValue::String("application/json".into()),
        ))
}

pub fn event(name: &str, payload: &str) -> Message {
    message("event", ":event-type", name, payload)
}

pub fn exception(name: &str) -> Message {
    message(
        "exception",
        ":exception-type",
        name,
        r#"{"message":"model failed"}"#,
    )
}

/// A `ConverseStream` rule that answers with the given events.
pub fn stream_rule(messages: Vec<Message>) -> Rule {
    stream_rule_with_trailer(messages, &[])
}

/// Like [`stream_rule`], with raw bytes after the events.
pub fn stream_rule_with_trailer(messages: Vec<Message>, trailer: &[u8]) -> Rule {
    mock!(Client::converse_stream).then_http_response(stream_response(messages, trailer))
}

/// Like [`stream_rule`], answering every request.
pub fn repeated_stream_rule(messages: Vec<Message>) -> Rule {
    mock!(Client::converse_stream)
        .sequence()
        .http_response(stream_response(messages, &[]))
        .repeatedly()
        .build()
}

/// A `ConverseStream` HTTP response with the given events and raw bytes after them.
pub fn stream_response(
    messages: Vec<Message>,
    trailer: &[u8],
) -> impl Fn() -> Response<SdkBody> + Send + Sync + 'static {
    let mut body = Vec::new();
    for message in &messages {
        write_message_to(message, &mut body).unwrap();
    }
    body.extend_from_slice(trailer);
    move || {
        let mut response = Response::new(
            StatusCode::try_from(200).unwrap(),
            SdkBody::from(body.clone()),
        );
        response
            .headers_mut()
            .insert("content-type", "application/vnd.amazon.eventstream");
        response
            .headers_mut()
            .insert("x-amzn-requestid", "stream-request-id");
        response
    }
}

/// A complete stream with the same token usage as [`converse_output`].
pub fn full_stream() -> Vec<Message> {
    vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hello"}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
        event(
            "metadata",
            r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":20,"cacheReadInputTokens":3,"cacheWriteInputTokens":2},"metrics":{"latencyMs":100}}"#,
        ),
    ]
}

/// One exported metric data point.
#[derive(Debug)]
pub struct Point {
    pub attributes: Vec<KeyValue>,
    pub count: u64,
    pub sum: f64,
    pub bounds: Vec<f64>,
}

/// Flushes `provider` and returns the data points of the metric `name`.
pub fn metric_points(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
    name: &str,
) -> Vec<Point> {
    provider.force_flush().unwrap();
    let exported = exporter.get_finished_metrics().unwrap();
    let Some(latest) = exported.last() else {
        return Vec::new();
    };
    let Some(metric) = latest
        .scope_metrics()
        .flat_map(|scope| scope.metrics())
        .find(|metric| metric.name() == name)
    else {
        return Vec::new();
    };
    match metric.data() {
        AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
            .data_points()
            .map(|point| Point {
                attributes: point.attributes().cloned().collect(),
                count: point.count(),
                sum: point.sum(),
                bounds: point.bounds().collect(),
            })
            .collect(),
        AggregatedMetrics::U64(MetricData::Histogram(histogram)) => histogram
            .data_points()
            .map(|point| Point {
                attributes: point.attributes().cloned().collect(),
                count: point.count(),
                sum: point.sum() as f64,
                bounds: point.bounds().collect(),
            })
            .collect(),
        AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
            .data_points()
            .map(|point| Point {
                attributes: point.attributes().cloned().collect(),
                count: 1,
                sum: point.value() as f64,
                bounds: Vec::new(),
            })
            .collect(),
        other => panic!("unexpected data for {name}: {other:?}"),
    }
}

pub const CONVERSE_BODY: &str = r#"{"output":{"message":{"role":"assistant","content":[{"text":"Hi"}]}},"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15},"metrics":{"latencyMs":1}}"#;

/// An HTTP response with a JSON body and a request ID header.
pub fn http_response(
    status: u16,
    body: &'static str,
    request_id_header: &'static str,
    request_id: &'static str,
) -> Response<SdkBody> {
    let mut response = Response::new(StatusCode::try_from(status).unwrap(), SdkBody::from(body));
    response
        .headers_mut()
        .insert("content-type", "application/json");
    response.headers_mut().insert(request_id_header, request_id);
    response
}

/// A `Converse` rule that answers with a real HTTP response, so the SDK deserializes it.
pub fn converse_http_rule() -> Rule {
    mock!(Client::converse).then_http_response(|| {
        http_response(
            200,
            CONVERSE_BODY,
            "x-amzn-requestid",
            "converse-request-id",
        )
    })
}

/// The JSON schema for a content attribute, from the GenAI semantic conventions.
fn schema(attribute: &str) -> Option<&'static str> {
    Some(match attribute {
        "gen_ai.input.messages" => include_str!("../schemas/gen-ai-input-messages.json"),
        "gen_ai.output.messages" => include_str!("../schemas/gen-ai-output-messages.json"),
        "gen_ai.system_instructions" => include_str!("../schemas/gen-ai-system-instructions.json"),
        "gen_ai.tool.definitions" => include_str!("../schemas/gen-ai-tool-definitions.json"),
        "gen_ai.tool.call.arguments" => include_str!("../schemas/gen-ai-tool-call-arguments.json"),
        "gen_ai.tool.call.result" => include_str!("../schemas/gen-ai-tool-call-result.json"),
        _ => return None,
    })
}

/// Panics if `value` doesn't match the schema of `attribute`.
///
/// The schemas accept any object with a `type` as a part, so each part with a known `type` is
/// also checked against its own definition.
pub fn assert_matches_schema(attribute: &str, value: &serde_json::Value) {
    let schema = schema(attribute).unwrap_or_else(|| panic!("no schema for {attribute}"));
    let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
    assert_valid(attribute, &schema, value);

    let Some(definitions) = schema["$defs"].as_object() else {
        return;
    };
    let parts: Vec<&serde_json::Value> = match attribute {
        "gen_ai.input.messages" | "gen_ai.output.messages" => value
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["parts"].as_array().unwrap())
            .collect(),
        "gen_ai.system_instructions" | "gen_ai.tool.definitions" => {
            value.as_array().unwrap().iter().collect()
        }
        _ => Vec::new(),
    };
    for part in parts {
        let definition = definitions
            .iter()
            .find(|(_, definition)| definition["properties"]["type"]["const"] == part["type"]);
        if let Some((name, _)) = definition {
            let part_schema = serde_json::json!({
                "$ref": format!("#/$defs/{name}"),
                "$defs": definitions,
            });
            assert_valid(attribute, &part_schema, part);
        }
    }
}

fn assert_valid(attribute: &str, schema: &serde_json::Value, value: &serde_json::Value) {
    // Tool definitions refer to the JSON Schema draft-07 meta-schema for tool parameters,
    // which are never recorded, so any schema is accepted there.
    let registry = jsonschema::Registry::new()
        .add(
            "http://json-schema.org/draft-07/schema",
            serde_json::json!({}),
        )
        .unwrap()
        .prepare()
        .unwrap();
    let validator = jsonschema::options()
        .with_registry(&registry)
        .build(schema)
        .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{attribute} doesn't match its schema: {errors:?}\n{value}"
    );
}
