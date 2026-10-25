mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseOutput};
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, ConverseOutput as ConverseResult, DocumentBlock,
    DocumentFormat, DocumentSource, ImageBlock, ImageFormat, ImageSource, Message,
    ReasoningContentBlock, ReasoningTextBlock, S3Location, StopReason, SystemContentBlock,
    SystemTool, Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
    ToolSpecification, ToolUseBlock,
};
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use aws_smithy_types::{Blob, Document};
use opentelemetry::Value;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::{Value as Json, json};

use common::{event, exception, full_stream, stream_rule};

const INPUT_MESSAGES: &str = "gen_ai.input.messages";
const OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
const SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
const TOOL_DEFINITIONS: &str = "gen_ai.tool.definitions";

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

    fn client(
        &self,
        rule: &Rule,
        configure: fn(BedrockInterceptor) -> BedrockInterceptor,
    ) -> Client {
        let provider = &self.provider;
        mock_client!(
            aws_sdk_bedrockruntime,
            RuleMode::Sequential,
            &[rule],
            move |config| config.interceptor(configure(BedrockInterceptor::with_tracer_provider(
                provider
            )))
        )
    }

    fn capturing_client(&self, rule: &Rule) -> Client {
        self.client(rule, |interceptor| interceptor.with_content_capture(true))
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

/// The attribute `key`, parsed as JSON.
fn json_attribute(span: &SpanData, key: &str) -> Json {
    match attribute(span, key) {
        Some(Value::String(value)) => serde_json::from_str(value.as_str()).unwrap(),
        other => panic!("{key} is not a string: {other:?}"),
    }
}

fn user(content: Vec<ContentBlock>) -> Message {
    Message::builder()
        .role(ConversationRole::User)
        .set_content(Some(content))
        .build()
        .unwrap()
}

fn assistant(content: Vec<ContentBlock>) -> Message {
    Message::builder()
        .role(ConversationRole::Assistant)
        .set_content(Some(content))
        .build()
        .unwrap()
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text(value.to_owned())
}

fn weather_tool_use() -> ContentBlock {
    ContentBlock::ToolUse(
        ToolUseBlock::builder()
            .tool_use_id("tool-1")
            .name("get_weather")
            .input(Document::Object(
                [("city".to_owned(), Document::String("Paris".to_owned()))].into(),
            ))
            .build()
            .unwrap(),
    )
}

fn converse_with(message: Message) -> ConverseOutput {
    ConverseOutput::builder()
        .output(ConverseResult::Message(message))
        .stop_reason(StopReason::EndTurn)
        .build()
        .unwrap()
}

#[tokio::test]
async fn content_is_not_captured_by_default() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.client(&rule, |interceptor| interceptor);

    client
        .converse()
        .model_id("model")
        .system(SystemContentBlock::Text("Be brief.".into()))
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    for key in [
        INPUT_MESSAGES,
        OUTPUT_MESSAGES,
        SYSTEM_INSTRUCTIONS,
        TOOL_DEFINITIONS,
    ] {
        assert_eq!(attribute(&span, key), None, "{key}");
    }
}

#[tokio::test]
async fn disabling_capture_records_no_content() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.client(&rule, |interceptor| {
        interceptor
            .with_content_capture(true)
            .with_content_capture(false)
    });

    client
        .converse()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(attribute(&span, INPUT_MESSAGES), None);
    assert_eq!(attribute(&span, OUTPUT_MESSAGES), None);
}

#[tokio::test]
async fn records_converse_messages() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| {
        converse_with(assistant(vec![
            ContentBlock::ReasoningContent(ReasoningContentBlock::ReasoningText(
                ReasoningTextBlock::builder()
                    .text("Need weather.")
                    .build()
                    .unwrap(),
            )),
            text("Checking."),
            weather_tool_use(),
        ]))
    });
    let client = setup.capturing_client(&rule);

    client
        .converse()
        .model_id("model")
        .system(SystemContentBlock::Text("Be brief.".into()))
        .system(SystemContentBlock::Text("Use tools.".into()))
        .messages(user(vec![text("Weather in Paris?")]))
        .messages(assistant(vec![weather_tool_use()]))
        .messages(user(vec![ContentBlock::ToolResult(
            ToolResultBlock::builder()
                .tool_use_id("tool-1")
                .content(ToolResultContentBlock::Json(Document::Object(
                    [("celsius".to_owned(), Document::from(18u64))].into(),
                )))
                .build()
                .unwrap(),
        )]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, SYSTEM_INSTRUCTIONS),
        json!([
            { "type": "text", "content": "Be brief." },
            { "type": "text", "content": "Use tools." },
        ])
    );
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES),
        json!([
            { "role": "user", "parts": [{ "type": "text", "content": "Weather in Paris?" }] },
            {
                "role": "assistant",
                "parts": [{
                    "type": "tool_call",
                    "id": "tool-1",
                    "name": "get_weather",
                    "arguments": { "city": "Paris" },
                }],
            },
            {
                "role": "user",
                "parts": [{
                    "type": "tool_call_response",
                    "id": "tool-1",
                    "response": { "celsius": 18 },
                }],
            },
        ])
    );
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES),
        json!([{
            "role": "assistant",
            "parts": [
                { "type": "reasoning", "content": "Need weather." },
                { "type": "text", "content": "Checking." },
                {
                    "type": "tool_call",
                    "id": "tool-1",
                    "name": "get_weather",
                    "arguments": { "city": "Paris" },
                },
            ],
        }])
    );
    assert_eq!(attribute(&span, TOOL_DEFINITIONS), None);
}

#[tokio::test]
async fn tool_results_with_several_items_are_an_array() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.capturing_client(&rule);

    client
        .converse()
        .model_id("model")
        .messages(user(vec![ContentBlock::ToolResult(
            ToolResultBlock::builder()
                .tool_use_id("tool-1")
                .content(ToolResultContentBlock::Text("18 degrees".into()))
                .content(ToolResultContentBlock::Json(Document::Bool(true)))
                .build()
                .unwrap(),
        )]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES)[0]["parts"][0]["response"],
        json!(["18 degrees", true])
    );
}

#[tokio::test]
async fn media_is_recorded_without_its_bytes() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.capturing_client(&rule);

    client
        .converse()
        .model_id("model")
        .messages(user(vec![
            ContentBlock::Image(
                ImageBlock::builder()
                    .format(ImageFormat::Png)
                    .source(ImageSource::Bytes(Blob::new(
                        b"secret-image-bytes".to_vec(),
                    )))
                    .build()
                    .unwrap(),
            ),
            ContentBlock::Image(
                ImageBlock::builder()
                    .format(ImageFormat::Jpeg)
                    .source(ImageSource::S3Location(
                        S3Location::builder()
                            .uri("s3://bucket/cat.jpeg")
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ),
            ContentBlock::Document(
                DocumentBlock::builder()
                    .format(DocumentFormat::Pdf)
                    .name("report")
                    .source(DocumentSource::Bytes(Blob::new(
                        b"secret-pdf-bytes".to_vec(),
                    )))
                    .build()
                    .unwrap(),
            ),
            ContentBlock::Document(
                DocumentBlock::builder()
                    .format(DocumentFormat::Txt)
                    .name("notes")
                    .source(DocumentSource::Text("plain notes".into()))
                    .build()
                    .unwrap(),
            ),
        ]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    let Some(Value::String(raw)) = attribute(&span, INPUT_MESSAGES) else {
        panic!("no input messages");
    };
    assert!(!raw.as_str().contains("secret"), "{raw}");
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES),
        json!([{
            "role": "user",
            "parts": [
                { "type": "image", "mime_type": "image/png" },
                {
                    "type": "uri",
                    "modality": "image",
                    "uri": "s3://bucket/cat.jpeg",
                    "mime_type": "image/jpeg",
                },
                { "type": "document", "mime_type": "application/pdf" },
                { "type": "text", "content": "plain notes" },
            ],
        }])
    );
}

#[tokio::test]
async fn tool_definitions_have_only_type_and_name() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.capturing_client(&rule);

    client
        .converse()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .tool_config(
            ToolConfiguration::builder()
                .tools(Tool::ToolSpec(
                    ToolSpecification::builder()
                        .name("get_weather")
                        .description("Looks up the weather")
                        .input_schema(ToolInputSchema::Json(Document::Object(
                            [("type".to_owned(), Document::String("object".into()))].into(),
                        )))
                        .build()
                        .unwrap(),
                ))
                .tools(Tool::SystemTool(
                    SystemTool::builder()
                        .name("nova_grounding")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, TOOL_DEFINITIONS),
        json!([
            { "type": "function", "name": "get_weather" },
            { "type": "system", "name": "nova_grounding" },
        ])
    );
}

#[tokio::test]
async fn long_text_is_cut_at_a_character_boundary() {
    let setup = Setup::new();
    let rule =
        mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("abcdef")])));
    let client = setup.client(&rule, |interceptor| {
        interceptor
            .with_content_capture(true)
            .with_max_content_length(4)
    });

    client
        .converse()
        .model_id("model")
        .system(SystemContentBlock::Text("abcd".into()))
        .messages(user(vec![text("日本語")]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, SYSTEM_INSTRUCTIONS)[0]["content"],
        json!("abcd")
    );
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES)[0]["parts"][0]["content"],
        json!("日")
    );
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES)[0]["parts"][0]["content"],
        json!("abcd")
    );
}

#[tokio::test]
async fn failed_calls_record_input_but_no_output() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_error(|| {
        ConverseError::ValidationException(
            ValidationException::builder()
                .meta(ErrorMetadata::builder().code("ValidationException").build())
                .build(),
        )
    });
    let client = setup.capturing_client(&rule);

    let _ = client
        .converse()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await;

    let span = setup.only_span();
    assert!(attribute(&span, INPUT_MESSAGES).is_some());
    assert_eq!(attribute(&span, OUTPUT_MESSAGES), None);
}

#[tokio::test]
async fn records_streamed_output() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"Need "}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"weather."}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"sig"}}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":0}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"text":"Check"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"text":"ing."}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"tool-1","name":"get_weather"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"city\":"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":2,"delta":{"toolUse":{"input":"\"Paris\"}"}}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":2}"#),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":3,"start":{"toolResult":{"toolUseId":"tool-2"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":3,"delta":{"toolResult":[{"text":"18 "},{"text":"degrees"}]}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":3}"#),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":4,"start":{"image":{"format":"png"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":4,"delta":{"image":{"source":{"bytes":"c2VjcmV0"}}}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":4}"#),
        event("messageStop", r#"{"stopReason":"tool_use"}"#),
        event(
            "metadata",
            r#"{"usage":{"inputTokens":1,"outputTokens":1,"totalTokens":2},"metrics":{"latencyMs":1}}"#,
        ),
    ]);
    let client = setup.capturing_client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Weather in Paris?")]))
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES),
        json!([{ "role": "user", "parts": [{ "type": "text", "content": "Weather in Paris?" }] }])
    );
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES),
        json!([{
            "role": "assistant",
            "parts": [
                { "type": "reasoning", "content": "Need weather." },
                { "type": "text", "content": "Checking." },
                {
                    "type": "tool_call",
                    "id": "tool-1",
                    "name": "get_weather",
                    "arguments": { "city": "Paris" },
                },
                { "type": "tool_call_response", "id": "tool-2", "response": "18 degrees" },
                { "type": "image", "mime_type": "image/png" },
            ],
        }])
    );
}

#[tokio::test]
async fn dropped_streams_record_partial_output() {
    let setup = Setup::new();
    // Without `messageStop`, dropping the stream cancels the call.
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"tool-1","name":"get_weather"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"toolUse":{"input":"{\"city\":"}}}"#,
        ),
    ]);
    let client = setup.capturing_client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();
    for _ in 0..3 {
        output.stream.recv().await.unwrap();
    }
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES),
        json!([{
            "role": "assistant",
            "parts": [
                { "type": "text", "content": "Hel" },
                {
                    "type": "tool_call",
                    "id": "tool-1",
                    "name": "get_weather",
                    "arguments": "{\"city\":",
                },
            ],
        }])
    );
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("cancelled"))
    );
}

#[tokio::test]
async fn streams_without_content_record_no_output() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ]);
    let client = setup.capturing_client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}
    drop(output);

    let span = setup.only_span();
    assert!(attribute(&span, INPUT_MESSAGES).is_some());
    assert_eq!(attribute(&span, OUTPUT_MESSAGES), None);
}

#[tokio::test]
async fn streamed_text_is_cut_to_the_maximum_length() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule, |interceptor| {
        interceptor
            .with_content_capture(true)
            .with_max_content_length(3)
    });

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES)[0]["parts"][0]["content"],
        json!("Hel")
    );
}

#[tokio::test]
async fn long_tool_arguments_and_results_are_cut() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.client(&rule, |interceptor| {
        interceptor
            .with_content_capture(true)
            .with_max_content_length(10)
    });

    client
        .converse()
        .model_id("model")
        .messages(user(vec![text("Weather in Paris?")]))
        .messages(assistant(vec![weather_tool_use()]))
        .messages(user(vec![ContentBlock::ToolResult(
            ToolResultBlock::builder()
                .tool_use_id("tool-1")
                .content(ToolResultContentBlock::Json(Document::Bool(true)))
                .content(ToolResultContentBlock::Json(Document::String(
                    "a long tool result".into(),
                )))
                .build()
                .unwrap(),
        )]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    let messages = json_attribute(&span, INPUT_MESSAGES);
    assert_eq!(messages[1]["parts"][0]["arguments"], json!(r#"{"city":"P"#));
    assert_eq!(
        messages[2]["parts"][0]["response"],
        json!([true, r#""a long to"#])
    );
}

#[tokio::test]
async fn documents_without_text_are_placeholders() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(|| converse_with(assistant(vec![text("Hi")])));
    let client = setup.capturing_client(&rule);

    client
        .converse()
        .model_id("model")
        .messages(user(vec![ContentBlock::Document(
            DocumentBlock::builder()
                .format(DocumentFormat::Csv)
                .name("table")
                .source(DocumentSource::Content(Vec::new()))
                .build()
                .unwrap(),
        )]))
        .send()
        .await
        .unwrap();

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, INPUT_MESSAGES)[0]["parts"],
        json!([{ "type": "document", "mime_type": "text/csv" }])
    );
}

#[tokio::test]
async fn stream_exceptions_keep_the_output_received() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ),
        exception("modelStreamErrorException"),
    ]);
    let client = setup.capturing_client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Hello")]))
        .send()
        .await
        .unwrap();
    while let Ok(Some(_)) = output.stream.recv().await {}
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES),
        json!([{ "role": "assistant", "parts": [{ "type": "text", "content": "Hel" }] }])
    );
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("ModelStreamErrorException"))
    );
}

#[tokio::test]
async fn streamed_tool_calls_need_a_start_event() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{}"}}}"#,
        ),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"tool-1","name":"get_time"}}}"#,
        ),
        event("contentBlockStop", r#"{"contentBlockIndex":1}"#),
        event("messageStop", r#"{"stopReason":"tool_use"}"#),
    ]);
    let client = setup.capturing_client(&rule);

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Time?")]))
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES),
        json!([{
            "role": "assistant",
            "parts": [{ "type": "tool_call", "id": "tool-1", "name": "get_time", "arguments": {} }],
        }])
    );
}

#[tokio::test]
async fn streamed_tool_input_cut_to_nothing_is_an_empty_string() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockStart",
            r#"{"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"tool-1","name":"get_weather"}}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"city\":\"Paris\"}"}}}"#,
        ),
        event("messageStop", r#"{"stopReason":"tool_use"}"#),
    ]);
    let client = setup.client(&rule, |interceptor| {
        interceptor
            .with_content_capture(true)
            .with_max_content_length(0)
    });

    let mut output = client
        .converse_stream()
        .model_id("model")
        .messages(user(vec![text("Weather?")]))
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}
    drop(output);

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, OUTPUT_MESSAGES)[0]["parts"][0]["arguments"],
        json!("")
    );
}
