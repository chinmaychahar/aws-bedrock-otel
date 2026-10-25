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
pub(crate) const GEN_AI_TOKEN_MODALITY: &str = "gen_ai.token.modality";
pub(crate) const AWS_BEDROCK_GUARDRAIL_ID: &str = "aws.bedrock.guardrail.id";
pub(crate) const ERROR_TYPE: &str = "error.type";

pub(crate) const PROVIDER_AWS_BEDROCK: &str = "aws.bedrock";
pub(crate) const OPERATION_CHAT: &str = "chat";
pub(crate) const ERROR_TYPE_OTHER: &str = "_OTHER";
pub(crate) const ERROR_TYPE_TIMEOUT: &str = "timeout";
pub(crate) const ERROR_TYPE_CANCELLED: &str = "cancelled";
pub(crate) const FINISH_REASON_ERROR: &str = "error";
pub(crate) const TOKEN_MODALITY_UNKNOWN: &str = "unknown";

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

/// Token counts reported by Bedrock.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TokenUsage {
    pub(crate) input: i64,
    pub(crate) output: i64,
    pub(crate) cache_read: Option<i64>,
    pub(crate) cache_write: Option<i64>,
}

impl TokenUsage {
    /// Bedrock excludes cached tokens from `input`; the convention includes them.
    pub(crate) fn total_input(&self) -> i64 {
        self.input
            .saturating_add(self.cache_read.unwrap_or(0))
            .saturating_add(self.cache_write.unwrap_or(0))
    }
}

/// Token usage attributes.
pub(crate) fn usage_attributes(usage: &TokenUsage) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(GEN_AI_USAGE_INPUT_TOKENS, usage.total_input()),
        KeyValue::new(GEN_AI_USAGE_OUTPUT_TOKENS, usage.output),
    ];
    if let Some(tokens) = usage.cache_read {
        attributes.push(KeyValue::new(GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS, tokens));
    }
    if let Some(tokens) = usage.cache_write {
        attributes.push(KeyValue::new(GEN_AI_USAGE_CACHE_WRITE_INPUT_TOKENS, tokens));
    }
    attributes
}

/// Attributes shared by all metrics.
pub(crate) fn metric_attributes(model: Option<&str>) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(GEN_AI_OPERATION_NAME, OPERATION_CHAT),
        KeyValue::new(GEN_AI_PROVIDER_NAME, PROVIDER_AWS_BEDROCK),
    ];
    if let Some(model) = model {
        attributes.push(KeyValue::new(GEN_AI_REQUEST_MODEL, model.to_owned()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_input_includes_cached_tokens() {
        let usage = TokenUsage {
            input: 10,
            output: 5,
            cache_read: Some(3),
            cache_write: Some(2),
        };
        assert_eq!(usage.total_input(), 15);

        let uncached = TokenUsage {
            input: 10,
            ..TokenUsage::default()
        };
        assert_eq!(uncached.total_input(), 10);
    }

    #[test]
    fn usage_attributes_skip_missing_cache_counts() {
        let usage = TokenUsage {
            input: 10,
            output: 5,
            cache_read: None,
            cache_write: Some(0),
        };
        let keys: Vec<_> = usage_attributes(&usage)
            .into_iter()
            .map(|kv| kv.key.as_str().to_owned())
            .collect();
        assert_eq!(
            keys,
            [
                GEN_AI_USAGE_INPUT_TOKENS,
                GEN_AI_USAGE_OUTPUT_TOKENS,
                GEN_AI_USAGE_CACHE_WRITE_INPUT_TOKENS
            ]
        );
    }

    #[test]
    fn metric_attributes_omit_missing_model() {
        assert_eq!(metric_attributes(None).len(), 2);
        assert!(metric_attributes(Some("m")).contains(&KeyValue::new(GEN_AI_REQUEST_MODEL, "m")));
    }
}
