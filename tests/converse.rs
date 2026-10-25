mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse::ConverseError;
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_sdk_bedrockruntime::types::{
    GuardrailConfiguration, InferenceConfiguration, JsonSchemaDefinition, OutputConfig,
    OutputFormat, OutputFormatStructure, OutputFormatType,
};
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use opentelemetry::trace::{
    FutureExt, SpanKind, Status, TraceContextExt, Tracer, TracerProvider as _,
};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

use aws_sdk_bedrockruntime::config::interceptors::BeforeDeserializationInterceptorContextRef;
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::BoxError;
use aws_smithy_runtime_api::client::retries::RequestAttempts;
use common::{CONVERSE_BODY, converse_http_rule, converse_output, http_response};

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
        "gen_ai.output.type",
        "gen_ai.request.reasoning.level",
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
    assert_eq!(
        attribute(&span, "gen_ai.response.finish_reasons"),
        Some(strings(&["error"]))
    );
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

#[tokio::test]
async fn records_server_and_request_id_from_http() {
    let setup = Setup::new();
    let rule = converse_http_rule();
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        attribute(&span, "server.address"),
        Some(Value::from("bedrock-runtime.us-east-1.amazonaws.com"))
    );
    assert_eq!(attribute(&span, "server.port"), Some(Value::I64(443)));
    assert_eq!(
        attribute(&span, "aws.request_id"),
        Some(Value::from("converse-request-id"))
    );
    assert_eq!(
        attribute(&span, "gen_ai.response.finish_reasons"),
        Some(strings(&["end_turn"]))
    );
    assert_eq!(
        attribute(&span, "gen_ai.usage.input_tokens"),
        Some(Value::I64(10))
    );
}

#[tokio::test]
async fn records_structured_output_and_reasoning_level() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule);

    let schema = JsonSchemaDefinition::builder()
        .schema(r#"{"type":"object"}"#)
        .build()
        .unwrap();
    let output_config = OutputConfig::builder()
        .text_format(
            OutputFormat::builder()
                .r#type(OutputFormatType::JsonSchema)
                .structure(OutputFormatStructure::JsonSchema(schema))
                .build()
                .unwrap(),
        )
        .effort("high")
        .build();
    client
        .converse()
        .model_id("amazon.nova-lite")
        .output_config(output_config)
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        attribute(&span, "gen_ai.output.type"),
        Some(Value::from("json"))
    );
    assert_eq!(
        attribute(&span, "gen_ai.request.reasoning.level"),
        Some(Value::from("high"))
    );
}

fn count(span: &SpanData, key: &str) -> usize {
    span.attributes
        .iter()
        .filter(|kv| kv.key.as_str() == key)
        .count()
}

#[tokio::test]
async fn retries_record_the_last_request_id_once() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .sequence()
        .http_response(|| http_response(503, "{}", "x-amzn-requestid", "first"))
        .times(2)
        .http_response(|| http_response(200, CONVERSE_BODY, "x-amzn-requestid", "second"))
        .build();
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(rule.num_calls(), 3);
    assert_eq!(count(&span, "aws.request_id"), 1);
    assert_eq!(
        attribute(&span, "aws.request_id"),
        Some(Value::from("second"))
    );
    assert_eq!(count(&span, "server.address"), 1);
}

#[tokio::test]
async fn request_id_falls_back_to_x_amz_request_id() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .then_http_response(|| http_response(200, CONVERSE_BODY, "x-amz-request-id", "fallback"));
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    assert_eq!(
        attribute(&setup.only_span(), "aws.request_id"),
        Some(Value::from("fallback"))
    );
}

#[tokio::test]
async fn error_responses_record_the_request_id() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_http_response(|| {
        let mut response = http_response(
            400,
            r#"{"message":"bad"}"#,
            "x-amzn-requestid",
            "error-request-id",
        );
        response
            .headers_mut()
            .insert("x-amzn-errortype", "ValidationException");
        response
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
        attribute(&span, "aws.request_id"),
        Some(Value::from("error-request-id"))
    );
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("ValidationException"))
    );
}

#[tokio::test]
async fn request_id_reads_x_amzn_request_id() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .then_http_response(|| http_response(200, CONVERSE_BODY, "x-amzn-request-id", "dashed"));
    let client = setup.client(&rule);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    assert_eq!(
        attribute(&setup.only_span(), "aws.request_id"),
        Some(Value::from("dashed"))
    );
}

/// Fails the second attempt in `read_after_transmit`.
#[derive(Debug)]
struct FailSecondTransmit;

impl Intercept for FailSecondTransmit {
    fn name(&self) -> &'static str {
        "FailSecondTransmit"
    }

    fn read_after_transmit(
        &self,
        _context: &BeforeDeserializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        match cfg.load::<RequestAttempts>().map(RequestAttempts::attempts) {
            Some(2) => Err("failed".into()),
            _ => Ok(()),
        }
    }
}

#[tokio::test]
async fn request_id_is_from_the_latest_response_when_another_interceptor_fails() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .sequence()
        .http_response(|| http_response(503, "{}", "x-amzn-requestid", "first"))
        .http_response(|| http_response(200, CONVERSE_BODY, "x-amzn-requestid", "second"))
        .build();
    let provider = &setup.provider;
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(BedrockInterceptor::with_tracer_provider(provider))
            .interceptor(FailSecondTransmit)
    );

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap_err();

    let span = setup.only_span();
    assert_eq!(count(&span, "aws.request_id"), 1);
    assert_eq!(
        attribute(&span, "aws.request_id"),
        Some(Value::from("second"))
    );
}
