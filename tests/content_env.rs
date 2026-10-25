//! Kept in its own test binary, because it changes the process environment. Add no other tests
//! here.

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::types::{ContentBlock, ConversationRole, Message};
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

const ENV: &str = "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT";

/// Whether a call made with the configured interceptor records `gen_ai.input.messages`.
async fn captures(configure: fn(BedrockInterceptor) -> BedrockInterceptor) -> bool {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let rule = mock!(Client::converse).then_output(|| {
        aws_sdk_bedrockruntime::operation::converse::ConverseOutput::builder()
            .stop_reason(aws_sdk_bedrockruntime::types::StopReason::EndTurn)
            .build()
            .unwrap()
    });
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        |config| config.interceptor(configure(
            BedrockInterceptor::new().tracer_provider(&provider)
        ))
    );
    client
        .converse()
        .model_id("model")
        .messages(
            Message::builder()
                .role(ConversationRole::User)
                .content(ContentBlock::Text("Hello".into()))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    spans[0]
        .attributes
        .iter()
        .any(|kv| kv.key.as_str() == "gen_ai.input.messages")
}

/// Whether a tool call records `gen_ai.tool.call.arguments`.
fn tool_tracer_captures() -> bool {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tools = BedrockInterceptor::new()
        .tracer_provider(&provider)
        .tool_tracer();
    let arguments = aws_smithy_types::Document::Object(Default::default());
    tools.tool("lookup").arguments(&arguments).start().end();
    let spans = exporter.get_finished_spans().unwrap();
    spans[0]
        .attributes
        .iter()
        .any(|kv| kv.key.as_str() == "gen_ai.tool.call.arguments")
}

fn set_env(value: Option<&str>) {
    // SAFETY: this binary has a single test, so no other thread reads the environment.
    unsafe {
        match value {
            Some(value) => std::env::set_var(ENV, value),
            None => std::env::remove_var(ENV),
        }
    }
}

#[tokio::test]
async fn environment_variable_turns_capture_on() {
    set_env(None);
    assert!(!captures(|i| i).await);

    set_env(Some("TRUE"));
    assert!(captures(|i| i).await);
    assert!(tool_tracer_captures());
    assert!(!captures(|i| i.content_capture(false)).await);

    set_env(Some("1"));
    assert!(!captures(|i| i).await);
    assert!(!tool_tracer_captures());
    assert!(captures(|i| i.content_capture(true)).await);

    set_env(None);
}
