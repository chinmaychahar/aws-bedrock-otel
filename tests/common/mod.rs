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
    let mut body = Vec::new();
    for message in &messages {
        write_message_to(message, &mut body).unwrap();
    }
    mock!(Client::converse_stream).then_http_response(move || {
        let mut response = Response::new(
            StatusCode::try_from(200).unwrap(),
            SdkBody::from(body.clone()),
        );
        response
            .headers_mut()
            .insert("content-type", "application/vnd.amazon.eventstream");
        response
    })
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
