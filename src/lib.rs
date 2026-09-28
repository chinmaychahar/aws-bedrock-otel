//! OpenTelemetry instrumentation for the Amazon Bedrock Runtime client of the AWS SDK for Rust,
//! following the OpenTelemetry [GenAI semantic conventions].
//!
//! Add [`BedrockInterceptor`] to a Bedrock Runtime client to record a span for each `Converse`
//! and `ConverseStream` call, with the model, request settings, token usage and finish reason.
//!
//! [GenAI semantic conventions]: https://github.com/open-telemetry/semantic-conventions-genai

mod attributes;
mod interceptor;
mod stream;

pub use interceptor::BedrockInterceptor;
