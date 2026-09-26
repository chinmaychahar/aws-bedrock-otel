use std::borrow::Cow;

use aws_sdk_bedrockruntime::config::interceptors::{
    BeforeSerializationInterceptorContextRef, FinalizerInterceptorContextRef,
};
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::{BoxError, ProvideErrorMetadata};
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseInput, ConverseOutput};
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Array, Context, InstrumentationScope, KeyValue, StringValue, Value};

use crate::attributes::*;

const SCOPE_NAME: &str = env!("CARGO_PKG_NAME");
const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Records an OpenTelemetry span for each `Converse` call made by a Bedrock Runtime client.
///
/// # Examples
///
/// ```no_run
/// use aws_bedrock_otel::BedrockInterceptor;
///
/// # async fn example() {
/// let sdk_config = aws_config::load_from_env().await;
/// let config = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config)
///     .interceptor(BedrockInterceptor::new())
///     .build();
/// let client = aws_sdk_bedrockruntime::Client::from_conf(config);
/// # }
/// ```
#[derive(Debug)]
pub struct BedrockInterceptor {
    tracer: BoxedTracer,
}

impl BedrockInterceptor {
    /// Creates an interceptor that uses the global tracer provider.
    pub fn new() -> Self {
        Self {
            tracer: global::tracer_with_scope(scope()),
        }
    }

    /// Creates an interceptor that uses the given tracer provider.
    pub fn with_tracer_provider<P>(provider: &P) -> Self
    where
        P: TracerProvider,
        P::Tracer: Send + Sync + 'static,
        <P::Tracer as Tracer>::Span: Send + Sync + 'static,
    {
        Self {
            tracer: BoxedTracer::new(Box::new(provider.tracer_with_scope(scope()))),
        }
    }
}

impl Default for BedrockInterceptor {
    fn default() -> Self {
        Self::new()
    }
}

fn scope() -> InstrumentationScope {
    InstrumentationScope::builder(SCOPE_NAME)
        .with_version(SCOPE_VERSION)
        .build()
}

/// The span of the current call, kept in the request's config bag.
#[derive(Debug)]
struct SpanContext(Context);

impl Storable for SpanContext {
    type Storer = StoreReplace<Self>;
}

impl Intercept for BedrockInterceptor {
    fn name(&self) -> &'static str {
        "BedrockInterceptor"
    }

    fn read_before_execution(
        &self,
        context: &BeforeSerializationInterceptorContextRef<'_>,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        // The typed input is only available before serialization.
        let Some(input) = context.input().downcast_ref::<ConverseInput>() else {
            return Ok(());
        };

        let model = input.model_id().unwrap_or_default();
        let span = self
            .tracer
            .span_builder(format!("{OPERATION_CHAT} {model}"))
            .with_kind(SpanKind::Client)
            .with_attributes(request_attributes(input))
            .start(&self.tracer);

        let span_context = Context::current().with_span(span);
        cfg.interceptor_state().store_put(SpanContext(span_context));
        Ok(())
    }

    fn read_after_execution(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(SpanContext(span_context)) = cfg.load::<SpanContext>() else {
            return Ok(());
        };
        let span = span_context.span();

        match context.output_or_error() {
            Some(Ok(output)) => {
                if let Some(output) = output.downcast_ref::<ConverseOutput>() {
                    span.set_attributes(response_attributes(output));
                }
            }
            Some(Err(error)) => {
                let error_type = error
                    .as_operation_error()
                    .and_then(|error| error.downcast_ref::<ConverseError>())
                    .and_then(|error| error.code())
                    .unwrap_or("_OTHER")
                    .to_owned();
                span.set_attribute(KeyValue::new(ERROR_TYPE, error_type.clone()));
                span.set_status(Status::error(error_type));
            }
            None => {}
        }
        span.end();
        Ok(())
    }
}

fn request_attributes(input: &ConverseInput) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(GEN_AI_OPERATION_NAME, OPERATION_CHAT),
        KeyValue::new(GEN_AI_PROVIDER_NAME, PROVIDER_AWS_BEDROCK),
    ];
    if let Some(model) = input.model_id() {
        attributes.push(KeyValue::new(GEN_AI_REQUEST_MODEL, model.to_owned()));
    }
    if let Some(config) = input.inference_config() {
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
    if let Some(guardrail) = input.guardrail_config() {
        attributes.push(KeyValue::new(
            AWS_BEDROCK_GUARDRAIL_ID,
            guardrail.guardrail_identifier().to_owned(),
        ));
    }
    attributes
}

fn response_attributes(output: &ConverseOutput) -> Vec<KeyValue> {
    let mut attributes = vec![KeyValue::new(
        GEN_AI_RESPONSE_FINISH_REASONS,
        string_array(&[output.stop_reason().as_str()]),
    )];
    if let Some(usage) = output.usage() {
        let cache_read = usage.cache_read_input_tokens().unwrap_or(0);
        let cache_write = usage.cache_write_input_tokens().unwrap_or(0);
        // Bedrock excludes cached tokens from `input_tokens`; the convention includes them.
        let input_tokens =
            i64::from(usage.input_tokens()) + i64::from(cache_read) + i64::from(cache_write);
        attributes.push(KeyValue::new(GEN_AI_USAGE_INPUT_TOKENS, input_tokens));
        attributes.push(KeyValue::new(
            GEN_AI_USAGE_OUTPUT_TOKENS,
            i64::from(usage.output_tokens()),
        ));
        if let Some(tokens) = usage.cache_read_input_tokens() {
            attributes.push(KeyValue::new(
                GEN_AI_USAGE_CACHE_READ_INPUT_TOKENS,
                i64::from(tokens),
            ));
        }
        if let Some(tokens) = usage.cache_write_input_tokens() {
            attributes.push(KeyValue::new(
                GEN_AI_USAGE_CACHE_WRITE_INPUT_TOKENS,
                i64::from(tokens),
            ));
        }
    }
    attributes
}

fn string_array<S: AsRef<str>>(values: &[S]) -> Value {
    let values = values
        .iter()
        .map(|value| StringValue::from(Cow::Owned(value.as_ref().to_owned())))
        .collect();
    Value::Array(Array::String(values))
}
