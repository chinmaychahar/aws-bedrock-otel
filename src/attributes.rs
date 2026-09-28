//! GenAI semantic convention attributes and helpers to build them.
//!
//! Defined here because the `gen_ai.*` constants in `opentelemetry-semantic-conventions` are
//! deprecated.

use aws_sdk_bedrockruntime::types::InferenceConfiguration;
use opentelemetry::{Array, KeyValue, StringValue, Value};

pub(crate) const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
pub(crate) const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
pub(crate) const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
pub(crate) const GEN_AI_REQUEST_MAX_TOKENS: &str = "gen_ai.request.max_tokens";
pub(crate) const GEN_AI_REQUEST_TEMPERATURE: &str = "gen_ai.request.temperature";
pub(crate) const GEN_AI_REQUEST_TOP_P: &str = "gen_ai.request.top_p";
pub(crate) const GEN_AI_REQUEST_STOP_SEQUENCES: &str = "gen_ai.request.stop_sequences";
pub(crate) const GEN_AI_REQUEST_STREAM: &str = "gen_ai.request.stream";
pub(crate) const GEN_AI_RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
pub(crate) const GEN_AI_RESPONSE_TIME_TO_FIRST_CHUNK: &str = "gen_ai.response.time_to_first_chunk";
pub(crate) const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub(crate) const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub(crate) const GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS: &str =
    "gen_ai.usage.cache_read.input_tokens";
pub(crate) const GEN_AI_USAGE_CACHE_WRITE_INPUT_TOKENS: &str =
    "gen_ai.usage.cache_write.input_tokens";
pub(crate) const AWS_BEDROCK_GUARDRAIL_ID: &str = "aws.bedrock.guardrail.id";
pub(crate) const ERROR_TYPE: &str = "error.type";

pub(crate) const PROVIDER_AWS_BEDROCK: &str = "aws.bedrock";
pub(crate) const OPERATION_CHAT: &str = "chat";
pub(crate) const ERROR_TYPE_OTHER: &str = "_OTHER";

/// Attributes known when a request starts.
pub(crate) fn request_attributes(
    model: Option<&str>,
    config: Option<&InferenceConfiguration>,
    guardrail_id: Option<&str>,
    stream: bool,
) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(GEN_AI_OPERATION_NAME, OPERATION_CHAT),
        KeyValue::new(GEN_AI_PROVIDER_NAME, PROVIDER_AWS_BEDROCK),
    ];
    if let Some(model) = model {
        attributes.push(KeyValue::new(GEN_AI_REQUEST_MODEL, model.to_owned()));
    }
    if let Some(config) = config {
        if let Some(max_tokens) = config.max_tokens() {
            attributes.push(KeyValue::new(
                GEN_AI_REQUEST_MAX_TOKENS,
                i64::from(max_tokens),
            ));
        }
        if let Some(temperature) = config.temperature() {
            attributes.push(KeyValue::new(
                GEN_AI_REQUEST_TEMPERATURE,
                f64::from(temperature),
            ));
        }
        if let Some(top_p) = config.top_p() {
            attributes.push(KeyValue::new(GEN_AI_REQUEST_TOP_P, f64::from(top_p)));
        }
        if !config.stop_sequences().is_empty() {
            attributes.push(KeyValue::new(
                GEN_AI_REQUEST_STOP_SEQUENCES,
                string_array(config.stop_sequences()),
            ));
        }
    }
    if let Some(guardrail_id) = guardrail_id {
        attributes.push(KeyValue::new(
            AWS_BEDROCK_GUARDRAIL_ID,
            guardrail_id.to_owned(),
        ));
    }
    if stream {
        attributes.push(KeyValue::new(GEN_AI_REQUEST_STREAM, true));
    }
    attributes
}

/// Token usage attributes.
pub(crate) fn usage_attributes(
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
) -> Vec<KeyValue> {
    // Bedrock excludes cached tokens from `input_tokens`; the convention includes them.
    let total_input =
        input_tokens + cache_read_tokens.unwrap_or(0) + cache_write_tokens.unwrap_or(0);
    let mut attributes = vec![
        KeyValue::new(GEN_AI_USAGE_INPUT_TOKENS, total_input),
        KeyValue::new(GEN_AI_USAGE_OUTPUT_TOKENS, output_tokens),
    ];
    if let Some(tokens) = cache_read_tokens {
        attributes.push(KeyValue::new(GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS, tokens));
    }
    if let Some(tokens) = cache_write_tokens {
        attributes.push(KeyValue::new(GEN_AI_USAGE_CACHE_WRITE_INPUT_TOKENS, tokens));
    }
    attributes
}

pub(crate) fn finish_reasons(reason: &str) -> KeyValue {
    KeyValue::new(GEN_AI_RESPONSE_FINISH_REASONS, string_array(&[reason]))
}

fn string_array<S: AsRef<str>>(values: &[S]) -> Value {
    let values = values
        .iter()
        .map(|value| StringValue::from(value.as_ref().to_owned()))
        .collect();
    Value::Array(Array::String(values))
}
