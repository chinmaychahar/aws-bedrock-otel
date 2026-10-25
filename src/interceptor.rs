use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use aws_sdk_bedrockruntime::config::interceptors::{
    BeforeDeserializationInterceptorContextMut, BeforeDeserializationInterceptorContextRef,
    BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextRef,
    FinalizerInterceptorContextRef,
};
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::{BoxError, ProvideErrorMetadata};
use aws_sdk_bedrockruntime::operation::converse::{ConverseError, ConverseInput, ConverseOutput};
use aws_sdk_bedrockruntime::operation::converse_stream::{
    ConverseStreamError, ConverseStreamInput, ConverseStreamOutput,
};
use aws_sdk_bedrockruntime::types::{
    ConverseOutput as ConverseResult, InferenceConfiguration, Message, OutputConfig,
    SystemContentBlock, ToolConfiguration,
};
use aws_smithy_runtime_api::client::interceptors::context::Error;
use aws_smithy_runtime_api::client::orchestrator::{HttpResponse, OrchestratorError};
use aws_smithy_runtime_api::client::retries::RequestAttempts;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use aws_smithy_types::timeout::TimeoutConfig;
use opentelemetry::global::{self, BoxedTracer};
use opentelemetry::metrics::MeterProvider;
use opentelemetry::trace::{Span, SpanKind, TraceContextExt, Tracer, TracerProvider};
use opentelemetry::{Context, InstrumentationScope, KeyValue};

use crate::attributes::*;
use crate::call::Call;
use crate::content::{ContentCapture, server_tool_names, tool_definitions};
use crate::metrics::Metrics;
use crate::stream::TracedBody;
use crate::tool::ToolTracer;

const SCOPE_NAME: &str = env!("CARGO_PKG_NAME");
const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");
const CAPTURE_CONTENT_ENV: &str = "OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT";

/// Records an OpenTelemetry span and metrics for each `Converse` and `ConverseStream` call made
/// by a Bedrock Runtime client.
///
/// Spans and metrics follow the OpenTelemetry GenAI semantic conventions. Spans are named
/// `chat {model}`. A `ConverseStream` call ends when the response stream is read to the end or
/// dropped. A call that is dropped before it finishes, for example by `tokio::time::timeout`, is
/// recorded with `error.type` set to `cancelled`. Other operations are not traced yet.
///
/// Prompts and responses are not recorded unless
/// [`content_capture`](Self::content_capture) or the
/// `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` environment variable turns it on.
///
/// Use [`spans`](Self::spans) or [`metrics`](Self::metrics) to turn either off. Global providers
/// are read when the interceptor is created, so set them first. If a client has more than one
/// `BedrockInterceptor`, the first one to see a call traces it and the others ignore it.
/// Client-level interceptors see calls before ones added with `customize()`.
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
///
/// With explicit providers:
///
/// ```no_run
/// use aws_bedrock_otel::BedrockInterceptor;
///
/// # fn example(
/// #     tracer_provider: &opentelemetry_sdk::trace::SdkTracerProvider,
/// #     meter_provider: &opentelemetry_sdk::metrics::SdkMeterProvider,
/// #     sdk_config: &aws_config::SdkConfig,
/// # ) {
/// let interceptor = BedrockInterceptor::new()
///     .tracer_provider(tracer_provider)
///     .meter_provider(meter_provider);
/// let config = aws_sdk_bedrockruntime::config::Builder::from(sdk_config)
///     .interceptor(interceptor)
///     .build();
/// let client = aws_sdk_bedrockruntime::Client::from_conf(config);
/// # }
/// ```
#[derive(Debug)]
pub struct BedrockInterceptor {
    tracer: Arc<BoxedTracer>,
    metrics: Metrics,
    spans: bool,
    record_metrics: bool,
    capture_content: bool,
    max_content_length: Option<usize>,
}

impl BedrockInterceptor {
    /// Creates an interceptor that uses the global tracer and meter providers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tracer: Arc::new(global::tracer_with_scope(scope())),
            metrics: Metrics::new(&global::meter_with_scope(scope())),
            spans: true,
            record_metrics: true,
            capture_content: std::env::var(CAPTURE_CONTENT_ENV)
                .is_ok_and(|value| value.eq_ignore_ascii_case("true")),
            max_content_length: None,
        }
    }

    /// Records spans with the given tracer provider instead of the global one.
    #[must_use]
    pub fn tracer_provider<P>(mut self, provider: &P) -> Self
    where
        P: TracerProvider,
        P::Tracer: Send + Sync + 'static,
        <P::Tracer as Tracer>::Span: Send + Sync + 'static,
    {
        self.tracer = Arc::new(BoxedTracer::new(Box::new(
            provider.tracer_with_scope(scope()),
        )));
        self
    }

    /// Records metrics with the given meter provider instead of the global one.
    #[must_use]
    pub fn meter_provider<P: MeterProvider>(mut self, provider: &P) -> Self {
        self.metrics = Metrics::new(&provider.meter_with_scope(scope()));
        self
    }

    /// Whether to record spans. On by default. Turn them off when another library, such as
    /// Rig, already creates spans for these calls.
    #[must_use]
    pub fn spans(mut self, enabled: bool) -> Self {
        self.spans = enabled;
        self
    }

    /// Whether to record metrics. On by default.
    #[must_use]
    pub fn metrics(mut self, enabled: bool) -> Self {
        self.record_metrics = enabled;
        self
    }

    /// Whether to record prompts, responses, system instructions and tool definitions on chat
    /// spans, and tool call arguments and results on [`ToolTracer`] spans. This overrides the
    /// `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT` environment variable.
    ///
    /// Content may include personal or sensitive data. Media sent as bytes is recorded
    /// without the bytes.
    #[must_use]
    pub fn content_capture(mut self, enabled: bool) -> Self {
        self.capture_content = enabled;
        self
    }

    /// Cuts each captured text, tool call argument and JSON tool result to at most `bytes` bytes.
    /// Cut JSON values are recorded as strings. The X-Ray OTLP endpoint rejects spans larger
    /// than 200 KB.
    #[must_use]
    pub fn max_content_length(mut self, bytes: usize) -> Self {
        self.max_content_length = Some(bytes);
        self
    }

    /// Returns a [`ToolTracer`] that records the app's tool calls with this interceptor's
    /// providers and options. Set the options first.
    #[must_use]
    pub fn tool_tracer(&self) -> ToolTracer {
        ToolTracer::new(
            self.tracer.clone(),
            self.record_metrics.then(|| self.metrics.clone()),
            self.spans,
            self.capture_content.then_some(ContentCapture {
                max_length: self.max_content_length,
            }),
        )
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

/// The parts of a `Converse` or `ConverseStream` input that are recorded.
struct Request<'a> {
    model: Option<&'a str>,
    config: Option<&'a InferenceConfiguration>,
    output_config: Option<&'a OutputConfig>,
    guardrail: Option<&'a str>,
    tool_config: Option<&'a ToolConfiguration>,
    system: &'a [SystemContentBlock],
    messages: &'a [Message],
    streaming: bool,
}

impl<'a> Request<'a> {
    fn from_converse(input: &'a ConverseInput) -> Self {
        Self {
            model: input.model_id(),
            config: input.inference_config(),
            output_config: input.output_config(),
            guardrail: input.guardrail_config().map(|g| g.guardrail_identifier()),
            tool_config: input.tool_config(),
            system: input.system(),
            messages: input.messages(),
            streaming: false,
        }
    }

    fn from_converse_stream(input: &'a ConverseStreamInput) -> Self {
        Self {
            model: input.model_id(),
            config: input.inference_config(),
            output_config: input.output_config(),
            guardrail: input.guardrail_config().map(|g| g.guardrail_identifier()),
            tool_config: input.tool_config(),
            system: input.system(),
            messages: input.messages(),
            streaming: true,
        }
    }

    fn content_attributes(&self, capture: &ContentCapture) -> Vec<KeyValue> {
        [
            (
                GEN_AI_SYSTEM_INSTRUCTIONS,
                capture.system_instructions(self.system),
            ),
            (GEN_AI_INPUT_MESSAGES, capture.input_messages(self.messages)),
            (GEN_AI_TOOL_DEFINITIONS, tool_definitions(self.tool_config)),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| KeyValue::new(key, value)))
        .collect()
    }
}

/// The current call, kept in the request's config bag. If the request is dropped before the
/// call finishes, the call is recorded as timed out or cancelled.
#[derive(Debug)]
struct CallState {
    call: Arc<Call>,
    streaming: bool,
    operation_timeout: Option<Duration>,
    /// The attempt whose response body was wrapped, so it is wrapped once even if the
    /// interceptor is registered twice.
    wrapped_attempt: Option<u32>,
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
        // A caller that drops the request after that long is also counted as a timeout.
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
        // Nothing to record, or another `BedrockInterceptor` on the client already traces this
        // call.
        if (!self.spans && !self.record_metrics) || cfg.load::<CallState>().is_some() {
            return Ok(());
        }
        // The typed input is only available before serialization.
        let input = context.input();
        let request = if let Some(input) = input.downcast_ref::<ConverseInput>() {
            Request::from_converse(input)
        } else if let Some(input) = input.downcast_ref::<ConverseStreamInput>() {
            Request::from_converse_stream(input)
        } else {
            return Ok(());
        };
        let mut capture = None;

        // Without spans, an empty context makes every span operation a no-op, so the caller's
        // current span is never touched.
        let started = (Instant::now(), SystemTime::now());
        let span_context = if self.spans {
            let attributes = request_attributes(
                request.model,
                request.config,
                request.output_config,
                request.guardrail,
                request.streaming,
            );
            let mut span = self
                .tracer
                .span_builder(span_name(request.model))
                .with_kind(SpanKind::Client)
                .with_start_time(started.1)
                .with_attributes(attributes)
                .start(self.tracer.as_ref());
            // Content is only captured for spans that are recorded.
            if self.capture_content && span.is_recording() {
                let content = ContentCapture {
                    max_length: self.max_content_length,
                };
                span.set_attributes(request.content_attributes(&content));
                capture = Some((content, server_tool_names(request.messages)));
            }
            Context::current().with_span(span)
        } else {
            Context::new()
        };

        let metrics = self.record_metrics.then(|| self.metrics.clone());
        let call = Call::new(
            span_context,
            started,
            metrics,
            capture,
            metric_attributes(request.model),
        );
        cfg.interceptor_state().store_put(CallState {
            call: Arc::new(call),
            streaming: request.streaming,
            operation_timeout: None,
            wrapped_attempt: None,
        });
        Ok(())
    }

    fn read_before_serialization(
        &self,
        _context: &BeforeSerializationInterceptorContextRef<'_>,
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

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        // The endpoint is resolved for each attempt, so these are added after the span starts.
        if let Some(state) = cfg.load::<CallState>() {
            state
                .call
                .set_server(server_attributes(context.request().uri()));
        }
        Ok(())
    }

    // The first hook after each response. It runs for every interceptor, so another
    // interceptor failing doesn't skip it.
    fn read_after_transmit(
        &self,
        context: &BeforeDeserializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        if let Some(state) = cfg.load::<CallState>()
            && let Some(request_id) = request_id(context.response())
        {
            state.call.set_request_id(request_id.to_owned());
        }
        Ok(())
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let attempt = cfg
            .load::<RequestAttempts>()
            .map_or(0, RequestAttempts::attempts);
        let Some(state) = cfg.get_mut_from_interceptor_state::<CallState>() else {
            return Ok(());
        };
        // Error responses aren't event streams; `read_after_execution` handles them. Streams
        // of calls that record nothing aren't decoded.
        let success = context.response().status().is_success();
        if !state.streaming
            || !success
            || state.wrapped_attempt == Some(attempt)
            || !state.call.is_recorded()
        {
            return Ok(());
        }
        state.wrapped_attempt = Some(attempt);
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
                    if let Some(ConverseResult::Message(message)) = output.output() {
                        call.set_output(|capture, earlier| {
                            Some(capture.output_messages(message, earlier))
                        });
                    }
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

fn span_name(model: Option<&str>) -> String {
    match model {
        Some(model) if !model.is_empty() => format!("{OPERATION_CHAT} {model}"),
        _ => OPERATION_CHAT.to_owned(),
    }
}

/// The request ID, from the headers listed for `aws.request_id`.
fn request_id(response: &HttpResponse) -> Option<&str> {
    let headers = response.headers();
    headers
        .get("x-amzn-requestid")
        .or_else(|| headers.get("x-amzn-request-id"))
        .or_else(|| headers.get("x-amz-request-id"))
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
