//! Attribute names from the OpenTelemetry GenAI semantic conventions.
//!
//! The `gen_ai.*` constants in `opentelemetry-semantic-conventions` are deprecated since the
//! conventions moved to <https://github.com/open-telemetry/semantic-conventions-genai>, so they
//! are defined here.

pub(crate) const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
pub(crate) const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
pub(crate) const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
pub(crate) const GEN_AI_REQUEST_MAX_TOKENS: &str = "gen_ai.request.max_tokens";
pub(crate) const GEN_AI_REQUEST_TEMPERATURE: &str = "gen_ai.request.temperature";
pub(crate) const GEN_AI_REQUEST_TOP_P: &str = "gen_ai.request.top_p";
pub(crate) const GEN_AI_REQUEST_STOP_SEQUENCES: &str = "gen_ai.request.stop_sequences";
pub(crate) const GEN_AI_RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
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
