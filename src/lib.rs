//! OpenTelemetry instrumentation for the Amazon Bedrock Runtime client of the AWS SDK for Rust,
//! following the OpenTelemetry [GenAI semantic conventions].
//!
//! Add [`BedrockInterceptor`] to a Bedrock Runtime client to record a span and metrics for each
//! `Converse` and `ConverseStream` call, with the model, request settings, token usage and finish
//! reason. Use [`BedrockInterceptor::tool_tracer`] to also record the tool calls that the app
//! runs.
//!
//! [GenAI semantic conventions]: https://github.com/open-telemetry/semantic-conventions-genai

mod attributes;
mod call;
mod content;
mod interceptor;
mod metrics;
mod stream;
mod tool;

pub use interceptor::BedrockInterceptor;
pub use tool::{ToolCall, ToolCallBuilder, ToolCallGuard, ToolTracer};
