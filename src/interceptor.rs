use std::time::Instant;

use aws_sdk_bedrockruntime::config::interceptors::{
    BeforeDeserializationInterceptorContextMut, BeforeSerializationInterceptorContextRef,
    FinalizerInterceptorContextRef,
};
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::{BoxError, ProvideErrorMetadata};
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseInput, ConverseOutput};
use aws_sdk_bedrockruntime::operation::converse_stream::{
    ConverseStreamError, ConverseStreamInput, ConverseStreamOutput,
};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, InstrumentationScope, KeyValue};

use crate::attributes::*;
use crate::stream::TracedBody;

const SCOPE_NAME: &str = env!("CARGO_PKG_NAME");
const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Records an OpenTelemetry span for each `Converse` and `ConverseStream` call made by a
/// Bedrock Runtime client.
///
/// Spans follow the OpenTelemetry GenAI semantic conventions and are named `chat {model}`.
/// A `ConverseStream` span ends when the response stream is read to the end or dropped.
/// Other operations are not traced yet.
///
/// # Examples
///
/// ```no_run
/// use aws_bedrock_otel::BedrockInterceptor;
///
/// # async fn example() {
/// let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
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
struct SpanState {
    context: Context,
    started: Instant,
    streaming: bool,
}

impl Storable for SpanState {
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
        let input = context.input();
        let (model, attributes, streaming) = if let Some(input) =
            input.downcast_ref::<ConverseInput>()
        {
            let guardrail = input.guardrail_config().map(|g| g.guardrail_identifier());
            let attributes =
                request_attributes(input.model_id(), input.inference_config(), guardrail, false);
            (input.model_id(), attributes, false)
        } else if let Some(input) = input.downcast_ref::<ConverseStreamInput>() {
            let guardrail = input.guardrail_config().map(|g| g.guardrail_identifier());
            let attributes =
                request_attributes(input.model_id(), input.inference_config(), guardrail, true);
            (input.model_id(), attributes, true)
        } else {
            return Ok(());
        };

        let span = self
            .tracer
            .span_builder(format!("{OPERATION_CHAT} {}", model.unwrap_or_default()))
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start(&self.tracer);

        cfg.interceptor_state().store_put(SpanState {
            context: Context::current().with_span(span),
            started: Instant::now(),
            streaming,
        });
        Ok(())
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(state) = cfg.load::<SpanState>() else {
            return Ok(());
        };
        // Error responses aren't event streams; `read_after_execution` handles them.
        if !state.streaming || !context.response().status().is_success() {
            return Ok(());
        }
        let body = std::mem::replace(context.response_mut().body_mut(), SdkBody::taken());
        let traced = TracedBody::new(body, state.context.clone(), state.started);
        *context.response_mut().body_mut() = SdkBody::from_body_1_x(traced);
        Ok(())
    }

    fn read_after_execution(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(state) = cfg.load::<SpanState>() else {
            return Ok(());
        };
        let span = state.context.span();

        match context.output_or_error() {
            Some(Ok(output)) => {
                if output.downcast_ref::<ConverseStreamOutput>().is_some() {
                    // The response stream ends the span.
                    return Ok(());
                }
                if let Some(output) = output.downcast_ref::<ConverseOutput>() {
                    span.set_attribute(finish_reasons(output.stop_reason().as_str()));
                    if let Some(usage) = output.usage() {
                        span.set_attributes(usage_attributes(
                            i64::from(usage.input_tokens()),
                            i64::from(usage.output_tokens()),
                            usage.cache_read_input_tokens().map(i64::from),
                            usage.cache_write_input_tokens().map(i64::from),
                        ));
                    }
                }
            }
            Some(Err(error)) => {
                let error = error.as_operation_error();
                let error_type = error
                    .and_then(|e| e.downcast_ref::<ConverseError>())
                    .and_then(|e| e.code())
                    .or_else(|| {
                        error
                            .and_then(|e| e.downcast_ref::<ConverseStreamError>())
                            .and_then(|e| e.code())
                    })
                    .unwrap_or(ERROR_TYPE_OTHER)
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
