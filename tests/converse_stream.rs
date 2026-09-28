use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_smithy_eventstream::frame::write_message_to;
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use aws_smithy_runtime_api::http::{Response, StatusCode};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::event_stream::{Header, HeaderValue, Message};
use opentelemetry::trace::Status;
use opentelemetry::{Array, StringValue, Value};
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

    fn spans(&self) -> Vec<SpanData> {
        self.exporter.get_finished_spans().unwrap()
    }
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn message(
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

fn event(name: &str, payload: &str) -> Message {
    message("event", ":event-type", name, payload)
}

fn stream_rule(messages: Vec<Message>) -> Rule {
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

fn full_stream() -> Vec<Message> {
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
}

#[tokio::test]
async fn records_error_from_stream_exception() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        message(
            "exception",
            ":exception-type",
            "modelStreamErrorException",
            r#"{"message":"model failed"}"#,
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
    output.stream.recv().await.unwrap_err();
    drop(output);

    let spans = setup.spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        attribute(&spans[0], "error.type"),
        Some(Value::from("ModelStreamErrorException"))
    );
    assert_eq!(spans[0].status, Status::error("ModelStreamErrorException"));
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
