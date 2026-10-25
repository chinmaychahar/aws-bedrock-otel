mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::SharedInterceptor;
use aws_sdk_bedrockruntime::config::interceptors::FinalizerInterceptorContextRef;
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::BoxError;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_sdk_bedrockruntime::types::{
    JsonSchemaDefinition, OutputConfig, OutputFormat, OutputFormatStructure, OutputFormatType,
};
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use opentelemetry::trace::Status;
use opentelemetry::{Array, StringValue, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_runtime_api::http::{Response, StatusCode};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use bytes::Bytes;
use common::{event, exception, full_stream, message, stream_rule, stream_rule_with_trailer};

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

    fn spans(&self) -> Vec<SpanData> {
        self.exporter.get_finished_spans().unwrap()
    }
}

fn strings(values: &[&'static str]) -> Value {
    Value::Array(Array::String(
        values.iter().map(|v| StringValue::from(*v)).collect(),
    ))
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

#[tokio::test]
async fn records_span_when_stream_is_read() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    assert!(setup.spans().is_empty(), "the span ends with the stream");

    let mut events = 0;
    while output.stream.recv().await.unwrap().is_some() {
        events += 1;
    }
    assert_eq!(events, 5);

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name, "chat amazon.nova-lite");
    assert_eq!(span.status, Status::Unset);
    assert_eq!(
        attribute(span, "gen_ai.request.stream"),
        Some(Value::Bool(true))
    );
    assert_eq!(
        attribute(span, "aws.request_id"),
        Some(Value::from("stream-request-id"))
    );
    assert_eq!(
        attribute(span, "server.address"),
        Some(Value::from("bedrock-runtime.us-east-1.amazonaws.com"))
    );
    assert_eq!(
        attribute(span, "gen_ai.response.finish_reasons"),
        Some(Value::Array(Array::String(vec![StringValue::from(
            "end_turn"
        )])))
    );
    assert_eq!(
        attribute(span, "gen_ai.usage.input_tokens"),
        Some(Value::I64(15))
    );
    assert_eq!(
        attribute(span, "gen_ai.usage.output_tokens"),
        Some(Value::I64(5))
    );
    assert_eq!(
        attribute(span, "gen_ai.usage.cache_read.input_tokens"),
        Some(Value::I64(3))
    );
    assert_eq!(
        attribute(span, "gen_ai.usage.cache_write.input_tokens"),
        Some(Value::I64(2))
    );
    assert!(matches!(
        attribute(span, "gen_ai.response.time_to_first_chunk"),
        Some(Value::F64(seconds)) if seconds >= 0.0
    ));
}

#[tokio::test]
async fn ends_span_when_stream_is_dropped() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    drop(output);

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(attribute(&spans[0], "error.type"), None);
    for key in ["aws.request_id", "gen_ai.response.time_to_first_chunk"] {
        let count = spans[0]
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == key)
            .count();
        assert_eq!(count, 1, "{key}");
    }
}

#[tokio::test]
async fn records_error_from_stream_exception() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        exception("modelStreamErrorException"),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    output.stream.recv().await.unwrap_err();
    drop(output);

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "error.type"),
        Some(Value::from("ModelStreamErrorException"))
    );
    assert_eq!(spans[0].status, Status::error("ModelStreamErrorException"));
    assert_eq!(
        attribute(&spans[0], "gen_ai.response.finish_reasons"),
        Some(strings(&["error"]))
    );
}

#[tokio::test]
async fn records_error_before_stream_opens() {
    let setup = Setup::new();
    let rule = mock!(Client::converse_stream).then_error(|| {
        ConverseStreamError::ValidationException(
            ValidationException::builder()
                .meta(ErrorMetadata::builder().code("ValidationException").build())
                .build(),
        )
    });
    let client = setup.client(&rule);

    client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap_err();

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "error.type"),
        Some(Value::from("ValidationException"))
    );
}

#[tokio::test]
async fn span_is_cancelled_when_stream_is_dropped_before_its_final_event() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    drop(output);

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "error.type"),
        Some(Value::from("cancelled"))
    );
    assert_eq!(spans[0].status, Status::error("cancelled"));
    assert_eq!(
        attribute(&spans[0], "gen_ai.response.finish_reasons"),
        Some(strings(&["error"]))
    );
}

/// Fails every call in `read_after_execution`, after the stream has been handed over.
#[derive(Debug)]
struct FailAfterExecution;

impl Intercept for FailAfterExecution {
    fn name(&self) -> &'static str {
        "FailAfterExecution"
    }

    fn read_after_execution(
        &self,
        _context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        Err("failed".into())
    }
}

#[tokio::test]
async fn stream_failed_by_another_interceptor_is_an_error() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let provider = &setup.provider;
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(BedrockInterceptor::with_tracer_provider(provider))
            .interceptor(FailAfterExecution)
    );

    client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap_err();

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "error.type"),
        Some(Value::from("_OTHER"))
    );
}

#[tokio::test]
async fn the_same_instance_twice_has_no_duplicate_attributes() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let shared = SharedInterceptor::new(BedrockInterceptor::with_tracer_provider(&setup.provider));
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(shared.clone())
            .interceptor(shared.clone())
    );

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    for key in [
        "aws.request_id",
        "server.address",
        "gen_ai.response.finish_reasons",
        "gen_ai.response.time_to_first_chunk",
        "gen_ai.usage.input_tokens",
    ] {
        let count = spans[0]
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == key)
            .count();
        assert_eq!(count, 1, "{key}");
    }
}

#[tokio::test]
async fn registering_the_interceptor_twice_records_one_span() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let provider = &setup.provider;
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(BedrockInterceptor::with_tracer_provider(provider))
            .interceptor(BedrockInterceptor::with_tracer_provider(provider))
    );

    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(attribute(&spans[0], "error.type"), None);
}

#[tokio::test]
async fn records_structured_output_and_reasoning_level() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
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
        .effort("low")
        .build();
    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .output_config(output_config)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    let spans = setup.spans();
    assert_eq!(
        attribute(&spans[0], "gen_ai.output.type"),
        Some(Value::from("json"))
    );
    assert_eq!(
        attribute(&spans[0], "gen_ai.request.reasoning.level"),
        Some(Value::from("low"))
    );
}

/// A body that sends `bytes` and then fails.
struct FailingBody {
    bytes: Option<Bytes>,
}

impl http_body::Body for FailingBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, std::io::Error>>> {
        std::task::Poll::Ready(Some(match self.bytes.take() {
            Some(bytes) => Ok(http_body::Frame::data(bytes)),
            None => Err(std::io::Error::other("connection reset")),
        }))
    }
}

async fn error_type_after_reading(rule: Rule) -> Option<Value> {
    let setup = Setup::new();
    let client = setup.client(&rule);
    let mut output = client
        .converse_stream()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();
    while let Ok(Some(_)) = output.stream.recv().await {}
    drop(output);
    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    attribute(&spans[0], "error.type")
}

#[tokio::test]
async fn body_errors_are_recorded_as_other() {
    let mut bytes = Vec::new();
    write_message_to(
        &event("messageStart", r#"{"role":"assistant"}"#),
        &mut bytes,
    )
    .unwrap();
    let rule = mock!(Client::converse_stream).then_http_response(move || {
        let body = FailingBody {
            bytes: Some(Bytes::from(bytes.clone())),
        };
        Response::new(
            StatusCode::try_from(200).unwrap(),
            SdkBody::from_body_1_x(body),
        )
    });
    assert_eq!(
        error_type_after_reading(rule).await,
        Some(Value::from("_OTHER"))
    );
}

#[tokio::test]
async fn unknown_message_types_are_recorded_as_other() {
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        message("error", ":error-code", "InternalFailure", "{}"),
    ]);
    assert_eq!(
        error_type_after_reading(rule).await,
        Some(Value::from("_OTHER"))
    );
}

#[tokio::test]
async fn partial_frames_at_the_end_are_recorded_as_other() {
    let rule = stream_rule_with_trailer(full_stream(), &[0, 0, 0, 40, 0, 0]);
    assert_eq!(
        error_type_after_reading(rule).await,
        Some(Value::from("_OTHER"))
    );
}

#[tokio::test]
async fn events_without_a_type_are_recorded_as_other() {
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        Message::new(b"{}".to_vec()).add_header(Header::new(
            ":message-type",
            HeaderValue::String("event".into()),
        )),
    ]);
    assert_eq!(
        error_type_after_reading(rule).await,
        Some(Value::from("_OTHER"))
    );
}

#[tokio::test]
async fn unparseable_message_stop_is_recorded_as_other() {
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event("messageStop", "not json"),
    ]);
    assert_eq!(
        error_type_after_reading(rule).await,
        Some(Value::from("_OTHER"))
    );
}
