use std::sync::Arc;
use std::time::Duration;

use aws_sdk_bedrockruntime::config::interceptors::{
    BeforeDeserializationInterceptorContextMut, BeforeSerializationInterceptorContextRef,
    BeforeTransmitInterceptorContextRef, FinalizerInterceptorContextRef,
};
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::{BoxError, ProvideErrorMetadata};
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseInput, ConverseOutput};
use aws_sdk_bedrockruntime::operation::converse_stream::{
    ConverseStreamError, ConverseStreamInput, ConverseStreamOutput,
};
use aws_smithy_runtime_api::client::interceptors::context::Error;
use aws_smithy_runtime_api::client::orchestrator::{HttpResponse, OrchestratorError};
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use aws_smithy_types::timeout::TimeoutConfig;
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{SpanKind, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, InstrumentationScope};

use crate::attributes::*;
use crate::call::Call;
use crate::metrics::Metrics;
use crate::stream::TracedBody;

const SCOPE_NAME: &str = env!("CARGO_PKG_NAME");
const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Records an OpenTelemetry span and metrics for each `Converse` and `ConverseStream` call made
/// by a Bedrock Runtime client.
///
/// Spans and metrics follow the OpenTelemetry GenAI semantic conventions. Spans are named
/// `chat {model}`. A `ConverseStream` call ends when the response stream is read to the end or
/// dropped. A call that is dropped before it finishes, for example by `tokio::time::timeout`, is
/// recorded with `error.type` set to `cancelled`. Other operations are not traced yet.
///
/// Use [`without_spans`](Self::without_spans) or [`without_metrics`](Self::without_metrics) to
/// record only one of them. Global providers are read when the interceptor is created, so set
/// them first.
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
    metrics: Metrics,
    spans: bool,
    record_metrics: bool,
}

impl BedrockInterceptor {
    /// Creates an interceptor that uses the global tracer and meter providers.
    pub fn new() -> Self {
        Self {
            tracer: global::tracer_with_scope(scope()),
            metrics: Metrics::new(&global::meter_with_scope(scope())),
            spans: true,
            record_metrics: true,
        }
    }

    /// Creates an interceptor that uses the given tracer provider and the global meter provider.
    pub fn with_tracer_provider<P>(provider: &P) -> Self
    where
        P: TracerProvider,
        P::Tracer: Send + Sync + 'static,
        <P::Tracer as Tracer>::Span: Send + Sync + 'static,
    {
        Self {
            tracer: BoxedTracer::new(Box::new(provider.tracer_with_scope(scope()))),
            ..Self::new()
        }
    }

    /// Records metrics with the given meter provider instead of the global one.
    pub fn with_meter_provider<P: MeterProvider>(mut self, provider: &P) -> Self {
        self.metrics = Metrics::new(&provider.meter_with_scope(scope()));
        self
    }

    /// Records metrics only. Use this when another library, such as Rig, already creates spans
    /// for these calls.
    pub fn without_spans(mut self) -> Self {
        self.spans = false;
        self
    }

    /// Records spans only. This takes precedence over
    /// [`with_meter_provider`](Self::with_meter_provider).
    pub fn without_metrics(mut self) -> Self {
        self.record_metrics = false;
        self
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

/// The current call, kept in the request's config bag. If the request is dropped before the
/// call finishes, the call is recorded as timed out or cancelled.
#[derive(Debug)]
struct CallState {
    call: Arc<Call>,
    streaming: bool,
    operation_timeout: Option<Duration>,
}

impl Storable for CallState {
    type Storer = StoreReplace<Self>;
}

impl Drop for CallState {
    fn drop(&mut self) {
        self.call.complete_request();
        if self.call.is_stream_delivered() {
            return;
        }
        // The SDK's operation timeout drops the request without telling the interceptors.
        // Its timer starts after this call does, so the elapsed time is at least the timeout.
        let timed_out = self
            .operation_timeout
            .is_some_and(|timeout| self.call.started().elapsed() >= timeout);
        let error_type = if timed_out {
            ERROR_TYPE_TIMEOUT
        } else {
            ERROR_TYPE_CANCELLED
        };
        self.call.finish(None, None, Some(error_type));
    }
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
        // Another `BedrockInterceptor` on the same client already traces this call.
        if (!self.spans && !self.record_metrics) || cfg.load::<CallState>().is_some() {
            return Ok(());
        }
        // The typed input is only available before serialization.
        let input = context.input();
        let (model, config, guardrail, streaming) =
            if let Some(input) = input.downcast_ref::<ConverseInput>() {
                let guardrail = input.guardrail_config().map(|g| g.guardrail_identifier());
                (input.model_id(), input.inference_config(), guardrail, false)
            } else if let Some(input) = input.downcast_ref::<ConverseStreamInput>() {
                let guardrail = input.guardrail_config().map(|g| g.guardrail_identifier());
                (input.model_id(), input.inference_config(), guardrail, true)
            } else {
                return Ok(());
            };

        // Without spans, an empty context makes every span operation a no-op, so the caller's
        // current span is never touched.
        let span_context = if self.spans {
            let attributes = request_attributes(model, config, guardrail, streaming);
            let span = self
                .tracer
                .span_builder(format!("{OPERATION_CHAT} {}", model.unwrap_or_default()))
                .with_kind(SpanKind::Client)
                .with_attributes(attributes)
                .start(&self.tracer);
            Context::current().with_span(span)
        } else {
            Context::new()
        };

        let metrics = self.record_metrics.then(|| self.metrics.clone());
        let call = Call::new(span_context, metrics, metric_attributes(model));
        cfg.interceptor_state().store_put(CallState {
            call: Arc::new(call),
            streaming,
            operation_timeout: None,
        });
        Ok(())
    }

    fn read_before_attempt(
        &self,
        _context: &BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let timeout = cfg
            .load::<TimeoutConfig>()
            .and_then(TimeoutConfig::operation_timeout);
        if let Some(state) = cfg.get_mut_from_interceptor_state::<CallState>() {
            state.operation_timeout = timeout;
        }
        Ok(())
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(state) = cfg.load::<CallState>() else {
            return Ok(());
        };
        // Error responses aren't event streams; `read_after_execution` handles them.
        if !state.streaming || !context.response().status().is_success() {
            return Ok(());
        }
        let body = std::mem::replace(context.response_mut().body_mut(), SdkBody::taken());
        let traced = TracedBody::new(body, state.call.clone());
        *context.response_mut().body_mut() = SdkBody::from_body_1_x(traced);
        Ok(())
    }

    fn read_after_execution(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let Some(state) = cfg.load::<CallState>() else {
            return Ok(());
        };
        let call = &state.call;

        match context.output_or_error() {
            Some(Ok(output)) => {
                if output.downcast_ref::<ConverseStreamOutput>().is_some() {
                    call.deliver_stream();
                } else if let Some(output) = output.downcast_ref::<ConverseOutput>() {
                    let usage = output.usage().map(|usage| TokenUsage {
                        input: i64::from(usage.input_tokens()),
                        output: i64::from(usage.output_tokens()),
                        cache_read: usage.cache_read_input_tokens().map(i64::from),
                        cache_write: usage.cache_write_input_tokens().map(i64::from),
                    });
                    call.finish(usage.as_ref(), Some(output.stop_reason().as_str()), None);
                }
            }
            Some(Err(error)) => {
                call.finish(None, None, Some(&error_type(error, context.response())));
            }
            None => {}
        }
        Ok(())
    }
}

/// The Bedrock error code, `timeout`, the HTTP status code, or `_OTHER`, in that order.
fn error_type(error: &OrchestratorError<Error>, response: Option<&HttpResponse>) -> String {
    let operation_error = error.as_operation_error();
    let code = operation_error
        .and_then(|e| e.downcast_ref::<ConverseError>())
        .and_then(|e| e.code())
        .or_else(|| {
            operation_error
                .and_then(|e| e.downcast_ref::<ConverseStreamError>())
                .and_then(|e| e.code())
        });
    if let Some(code) = code {
        return code.to_owned();
    }
    let timed_out = error.is_timeout_error()
        || error
            .as_connector_error()
            .is_some_and(|error| error.is_timeout());
    if timed_out {
        return ERROR_TYPE_TIMEOUT.to_owned();
    }
    match response.map(|response| response.status()) {
        Some(status) if !status.is_success() => status.as_u16().to_string(),
        _ => ERROR_TYPE_OTHER.to_owned(),
    }
}
