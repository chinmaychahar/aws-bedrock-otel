use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseOutput};
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_sdk_bedrockruntime::types::{
    ConverseMetrics, GuardrailConfiguration, InferenceConfiguration, StopReason, TokenUsage,
};
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use opentelemetry::trace::{
    FutureExt, SpanKind, Status, TraceContextExt, Tracer, TracerProvider as _,
};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

struct Setup {
    provider: SdkTracerProvider,
    exporter: InMemorySpanExporter,
}

impl Setup {
    fn new() -> Self {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        Self { provider, exporter }
    }

    fn client(&self, rule: &Rule) -> Client {
        let provider = &self.provider;
        mock_client!(
            aws_sdk_bedrockruntime,
            RuleMode::Sequential,
            &[rule],
            move |config| config.interceptor(BedrockInterceptor::with_tracer_provider(provider))
        )
    }

    fn only_span(&self) -> SpanData {
        let mut spans = self.exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1, "expected exactly one span");
        spans.remove(0)
    }
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn strings(values: &[&'static str]) -> Value {
    Value::Array(Array::String(
        values.iter().map(|v| StringValue::from(*v)).collect(),
    ))
}

fn converse_output() -> ConverseOutput {
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

#[tokio::test]
async fn records_converse_span() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("anthropic.claude-3-haiku")
        .inference_config(
            InferenceConfiguration::builder()
                .max_tokens(256)
                .temperature(0.5)
                .top_p(0.25)
                .stop_sequences("END")
                .build(),
        )
        .guardrail_config(
            GuardrailConfiguration::builder()
                .guardrail_identifier("gr-123")
                .guardrail_version("1")
                .build(),
        )
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(span.name, "chat anthropic.claude-3-haiku");
    assert_eq!(span.span_kind, SpanKind::Client);
    assert_eq!(span.status, Status::Unset);

    let expected = [
        KeyValue::new("gen_ai.operation.name", "chat"),
        KeyValue::new("gen_ai.provider.name", "aws.bedrock"),
        KeyValue::new("gen_ai.request.model", "anthropic.claude-3-haiku"),
        KeyValue::new("gen_ai.request.max_tokens", 256),
        KeyValue::new("gen_ai.request.temperature", 0.5),
        KeyValue::new("gen_ai.request.top_p", 0.25),
        KeyValue::new("gen_ai.request.stop_sequences", strings(&["END"])),
        KeyValue::new("aws.bedrock.guardrail.id", "gr-123"),
        KeyValue::new("gen_ai.response.finish_reasons", strings(&["end_turn"])),
        // 10 uncached + 3 read from cache + 2 written to cache
        KeyValue::new("gen_ai.usage.input_tokens", 15),
        KeyValue::new("gen_ai.usage.output_tokens", 5),
        KeyValue::new("gen_ai.usage.cache_read.input_tokens", 3),
        KeyValue::new("gen_ai.usage.cache_write.input_tokens", 2),
    ];
    for kv in expected {
        assert_eq!(
            attribute(&span, kv.key.as_str()),
            Some(kv.value.clone()),
            "{}",
            kv.key
        );
    }
    assert_eq!(attribute(&span, "error.type"), None);
}

#[tokio::test]
async fn omits_unset_request_settings() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    for key in [
        "gen_ai.request.max_tokens",
        "gen_ai.request.temperature",
        "gen_ai.request.top_p",
        "gen_ai.request.stop_sequences",
        "aws.bedrock.guardrail.id",
    ] {
        assert_eq!(attribute(&span, key), None, "{key}");
    }
}

#[tokio::test]
async fn records_error_type() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_error(|| {
        ConverseError::ValidationException(
            ValidationException::builder()
                .message("bad request")
                .meta(ErrorMetadata::builder().code("ValidationException").build())
                .build(),
        )
    });
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap_err();

    let span = setup.only_span();
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("ValidationException"))
    );
    assert_eq!(span.status, Status::error("ValidationException"));
    assert_eq!(attribute(&span, "gen_ai.usage.input_tokens"), None);
}

#[tokio::test]
async fn uses_current_span_as_parent() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule);

    let tracer = setup.provider.tracer("test");
    let parent = tracer.start("parent");
    let parent_context = Context::current_with_span(parent);
    let parent_span_id = parent_context.span().span_context().span_id();

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .with_context(parent_context)
        .await
        .unwrap();

    let spans = setup.exporter.get_finished_spans().unwrap();
    let span = spans
        .iter()
        .find(|span| span.name == "chat amazon.nova-lite")
        .unwrap();
    assert_eq!(span.parent_span_id, parent_span_id);
}
