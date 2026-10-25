//! Sends one `Converse` request to Amazon Bedrock and prints the recorded span and metrics to
//! stdout.
//!
//! Needs AWS credentials and access to the model in your region:
//!
//! ```sh
//! BEDROCK_MODEL_ID=amazon.nova-lite-v1:0 cargo run --example stdout
//! ```

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message,
};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(opentelemetry_stdout::SpanExporter::default())
        .build();
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(
            PeriodicReader::builder(opentelemetry_stdout::MetricExporter::default()).build(),
        )
        .build();

    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let config = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config)
        .interceptor(
            BedrockInterceptor::new()
                .tracer_provider(&tracer_provider)
                .meter_provider(&meter_provider),
        )
        .build();
    let client = aws_sdk_bedrockruntime::Client::from_conf(config);

    let model_id =
        std::env::var("BEDROCK_MODEL_ID").unwrap_or_else(|_| "amazon.nova-lite-v1:0".to_owned());
    let message = Message::builder()
        .role(ConversationRole::User)
        .content(ContentBlock::Text("Say hello in one sentence.".to_owned()))
        .build()?;

    let response = client
        .converse()
        .model_id(model_id)
        .messages(message)
        .inference_config(InferenceConfiguration::builder().max_tokens(100).build())
        .send()
        .await?;

    if let Some(message) = response
        .output()
        .and_then(|output| output.as_message().ok())
    {
        for block in message.content() {
            if let Ok(text) = block.as_text() {
                println!("Model: {text}");
            }
        }
    }

    tracer_provider.shutdown()?;
    meter_provider.shutdown()?;
    Ok(())
}
