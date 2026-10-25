//! Spans join the caller's trace in apps that use `tracing` with `tracing-opentelemetry`.

mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use tracing::Instrument;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

use common::converse_output;

async fn spans(level: LevelFilter) -> Vec<SpanData> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("app")))
        .with(level);
    let _guard = tracing::subscriber::set_default(subscriber);

    let rule = mock!(Client::converse).then_output(converse_output);
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        |config| config.interceptor(BedrockInterceptor::with_tracer_provider(&provider))
    );
    async { client.converse().model_id("m").send().await.unwrap() }
        .instrument(tracing::info_span!("request"))
        .await;

    exporter.get_finished_spans().unwrap()
}

fn find<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name == name)
        .unwrap_or_else(|| panic!("no {name} span"))
}

#[tokio::test]
async fn chat_span_is_a_child_of_the_current_tracing_span() {
    let spans = spans(LevelFilter::INFO).await;
    let request = find(&spans, "request");
    let chat = find(&spans, "chat m");
    assert_eq!(chat.parent_span_id, request.span_context.span_id());
    assert_eq!(
        chat.span_context.trace_id(),
        request.span_context.trace_id()
    );
}

#[tokio::test]
async fn chat_span_stays_in_the_trace_with_sdk_debug_spans() {
    // With debug spans enabled, the SDK's own spans sit between `request` and `chat`.
    let spans = spans(LevelFilter::DEBUG).await;
    let request = find(&spans, "request");
    let sdk = find(&spans, "Bedrock Runtime.Converse");
    let chat = find(&spans, "chat m");
    assert_eq!(sdk.parent_span_id, request.span_context.span_id());
    assert_ne!(chat.parent_span_id, request.span_context.span_id());
    assert_eq!(
        chat.span_context.trace_id(),
        request.span_context.trace_id()
    );
}
