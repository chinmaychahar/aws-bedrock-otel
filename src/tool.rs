//! `execute_tool` spans and metrics for tools that the app runs.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use aws_sdk_bedrockruntime::types::{ToolResultBlock, ToolResultStatus, ToolUseBlock};
use aws_smithy_types::Document;
use opentelemetry::global::BoxedTracer;
use opentelemetry::trace::{
    FutureExt, Span, SpanKind, Status, TraceContextExt, Tracer, WithContext,
};
use opentelemetry::{Context, ContextGuard, KeyValue};

use crate::attributes::*;
use crate::content::ContentCapture;
use crate::metrics::Metrics;

/// Records an `execute_tool` span and the `gen_ai.execute_tool.duration` metric for each tool
/// call that the app runs. Get one from [`BedrockInterceptor::tool_tracer`].
///
/// # Examples
///
/// ```no_run
/// use aws_bedrock_otel::BedrockInterceptor;
/// use aws_sdk_bedrockruntime::types::{ContentBlock, Message, ToolResultBlock};
///
/// # async fn run_tool(_: &aws_sdk_bedrockruntime::types::ToolUseBlock) -> ToolResultBlock { todo!() }
/// # async fn example(message: &Message) {
/// let interceptor = BedrockInterceptor::new();
/// let tools = interceptor.tool_tracer();
/// // Add `interceptor` to the Bedrock Runtime client, then for each tool call in a response:
/// for block in message.content() {
///     if let ContentBlock::ToolUse(tool_use) = block {
///         let call = tools.tool_use(tool_use).start();
///         let result = call.in_context(run_tool(tool_use)).await;
///         call.end_with_result(&result);
///     }
/// }
/// # }
/// ```
///
/// [`BedrockInterceptor::tool_tracer`]: crate::BedrockInterceptor::tool_tracer
#[derive(Debug, Clone)]
pub struct ToolTracer {
    tracer: Arc<BoxedTracer>,
    metrics: Option<Metrics>,
    spans: bool,
    capture: Option<ContentCapture>,
}

impl ToolTracer {
    pub(crate) fn new(
        tracer: Arc<BoxedTracer>,
        metrics: Option<Metrics>,
        spans: bool,
        capture: Option<ContentCapture>,
    ) -> Self {
        Self {
            tracer,
            metrics,
            spans,
            capture,
        }
    }

    /// Describes a call of the tool `name`.
    pub fn tool<'a>(&'a self, name: &'a str) -> ToolCallBuilder<'a> {
        ToolCallBuilder {
            tracer: self,
            name,
            call_id: None,
            tool_type: None,
            description: None,
            agent_name: None,
            conversation_id: None,
            arguments: None,
        }
    }

    /// Describes the call that the model asked for in `tool_use`, with its name, ID and
    /// arguments. The tool type is `function`, as the app runs it.
    pub fn tool_use<'a>(&'a self, tool_use: &'a ToolUseBlock) -> ToolCallBuilder<'a> {
        self.tool(tool_use.name())
            .call_id(tool_use.tool_use_id())
            .tool_type(TOOL_TYPE_FUNCTION)
            .arguments(tool_use.input())
    }
}

/// A tool call that hasn't started. Call [`start`](Self::start) just before running the tool.
#[derive(Debug)]
#[must_use = "a tool call is only recorded once it is started"]
pub struct ToolCallBuilder<'a> {
    tracer: &'a ToolTracer,
    name: &'a str,
    call_id: Option<&'a str>,
    tool_type: Option<&'a str>,
    description: Option<&'a str>,
    agent_name: Option<&'a str>,
    conversation_id: Option<&'a str>,
    arguments: Option<&'a Document>,
}

impl<'a> ToolCallBuilder<'a> {
    /// The tool call ID that the model sent, recorded as `gen_ai.tool.call.id`.
    pub fn call_id(mut self, id: &'a str) -> Self {
        self.call_id = Some(id);
        self
    }

    /// `gen_ai.tool.type`, such as `function`, `extension` or `datastore`.
    pub fn tool_type(mut self, tool_type: &'a str) -> Self {
        self.tool_type = Some(tool_type);
        self
    }

    /// `gen_ai.tool.description`. It may contain sensitive information.
    pub fn description(mut self, description: &'a str) -> Self {
        self.description = Some(description);
        self
    }

    /// `gen_ai.agent.name`, the name of the agent that runs the tool.
    pub fn agent_name(mut self, name: &'a str) -> Self {
        self.agent_name = Some(name);
        self
    }

    /// `gen_ai.conversation.id`, an ID the app already has for the conversation.
    pub fn conversation_id(mut self, id: &'a str) -> Self {
        self.conversation_id = Some(id);
        self
    }

    /// The arguments, recorded as `gen_ai.tool.call.arguments` when content capture is on.
    pub fn arguments(mut self, arguments: &'a Document) -> Self {
        self.arguments = Some(arguments);
        self
    }

    /// Starts the call. Its span is a child of the current context.
    pub fn start(self) -> ToolCall {
        let tracer = self.tracer;
        let mut metric_attributes = vec![KeyValue::new(GEN_AI_TOOL_NAME, self.name.to_owned())];
        if let Some(tool_type) = self.tool_type {
            metric_attributes.push(KeyValue::new(GEN_AI_TOOL_TYPE, tool_type.to_owned()));
        }
        if let Some(agent_name) = self.agent_name {
            metric_attributes.push(KeyValue::new(GEN_AI_AGENT_NAME, agent_name.to_owned()));
        }

        let parent = Context::current();
        let started_at = SystemTime::now();
        let started = Instant::now();
        let (context, capture) = if tracer.spans {
            let optional = [
                (GEN_AI_TOOL_CALL_ID, self.call_id),
                (GEN_AI_TOOL_DESCRIPTION, self.description),
                (GEN_AI_CONVERSATION_ID, self.conversation_id),
            ];
            let mut attributes = metric_attributes.clone();
            attributes.push(KeyValue::new(GEN_AI_OPERATION_NAME, OPERATION_EXECUTE_TOOL));
            attributes.extend(optional.into_iter().filter_map(|(key, value)| {
                value.map(|value| KeyValue::new(key, value.to_owned()))
            }));
            let mut span = tracer
                .tracer
                .span_builder(span_name(self.name))
                .with_kind(SpanKind::Internal)
                .with_start_time(started_at)
                .with_attributes(attributes)
                .start_with_context(tracer.tracer.as_ref(), &parent);
            // Content is only captured for spans that are recorded.
            let capture = tracer.capture.filter(|_| span.is_recording());
            if let Some(capture) = &capture
                && let Some(arguments) = self.arguments
            {
                span.set_attribute(KeyValue::new(
                    GEN_AI_TOOL_CALL_ARGUMENTS,
                    capture.tool_call_arguments(arguments),
                ));
            }
            (parent.with_span(span), capture)
        } else {
            (parent, None)
        };

        ToolCall {
            context,
            spans: tracer.spans,
            capture,
            metrics: tracer.metrics.clone(),
            metric_attributes,
            started_at,
            started,
            started_panicking: std::thread::panicking(),
            ended: false,
        }
    }
}

fn span_name(tool_name: &str) -> String {
    if tool_name.is_empty() {
        OPERATION_EXECUTE_TOOL.to_owned()
    } else {
        format!("{OPERATION_EXECUTE_TOOL} {tool_name}")
    }
}

/// A running tool call. End it with [`end_with_result`](Self::end_with_result),
/// [`end`](Self::end) or [`fail`](Self::fail). A call dropped before it ends is recorded with
/// `error.type` set to `cancelled`, or `_OTHER` if a panic in the tool drops it.
#[derive(Debug)]
#[must_use = "dropping a ToolCall records it as cancelled"]
pub struct ToolCall {
    context: Context,
    spans: bool,
    capture: Option<ContentCapture>,
    metrics: Option<Metrics>,
    metric_attributes: Vec<KeyValue>,
    started_at: SystemTime,
    started: Instant,
    /// Whether a panic was already unwinding at start, so only a later one counts.
    started_panicking: bool,
    ended: bool,
}

impl ToolCall {
    /// The context of the call's span. Spans started in this context are children of the
    /// call. Without spans, this is the context that was current when the call started.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Runs `future` in the call's [`context`](Self::context), so spans it starts are
    /// children of the call. End the call after the future completes.
    pub fn in_context<F>(&self, future: F) -> WithContext<F> {
        future.with_context(self.context.clone())
    }

    /// Makes the call's [`context`](Self::context) current until the guard is dropped. The
    /// call can't end while the guard exists. In async code, use
    /// [`in_context`](Self::in_context) instead.
    pub fn attach(&self) -> ToolCallGuard<'_> {
        ToolCallGuard {
            _guard: self.context.clone().attach(),
            _call: PhantomData,
        }
    }

    /// Ends the call with the result that is sent back to the model. With content capture on,
    /// the result is recorded as `gen_ai.tool.call.result`, unless it is empty. A result with
    /// the status `error` is recorded with `error.type` set to `_OTHER`; use
    /// [`fail`](Self::fail) to record another error type.
    pub fn end_with_result(mut self, result: &ToolResultBlock) {
        if result.status() == Some(&ToolResultStatus::Error) {
            self.finish(Some(ERROR_TYPE_OTHER));
            return;
        }
        if let Some(output) = self
            .capture
            .as_ref()
            .and_then(|capture| capture.tool_call_result(result))
        {
            self.context
                .span()
                .set_attribute(KeyValue::new(GEN_AI_TOOL_CALL_RESULT, output));
        }
        self.finish(None);
    }

    /// Ends the call as successful, without recording a result.
    pub fn end(mut self) {
        self.finish(None);
    }

    /// Ends the call as failed. `error_type` should be a short, low-cardinality name for the
    /// error, such as `timeout` or an error type name.
    pub fn fail(mut self, error_type: &str) {
        self.finish(Some(error_type));
    }

    fn finish(&mut self, error_type: Option<&str>) {
        self.ended = true;
        // The span and the metric get the same duration. The end time follows the monotonic
        // clock, so a wall clock change during the call doesn't move it.
        let duration = self.started.elapsed();
        let mut attributes = std::mem::take(&mut self.metric_attributes);
        if let Some(error_type) = error_type {
            attributes.push(KeyValue::new(ERROR_TYPE, error_type.to_owned()));
        }
        if self.spans {
            let span = self.context.span();
            if let Some(error_type) = error_type {
                span.set_attribute(KeyValue::new(ERROR_TYPE, error_type.to_owned()));
                span.set_status(Status::error(error_type.to_owned()));
            }
            span.end_with_timestamp(self.started_at + duration);
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_tool_call(&attributes, duration);
        }
    }
}

impl Drop for ToolCall {
    fn drop(&mut self) {
        if !self.ended {
            let error_type = if std::thread::panicking() && !self.started_panicking {
                ERROR_TYPE_OTHER
            } else {
                ERROR_TYPE_CANCELLED
            };
            self.finish(Some(error_type));
        }
    }
}

/// Keeps a tool call's context current. Returned by [`ToolCall::attach`].
///
/// ```compile_fail,E0505
/// # fn example(tools: &aws_bedrock_otel::ToolTracer) {
/// let call = tools.tool("lookup").start();
/// let guard = call.attach();
/// call.end(); // The guard still borrows the call.
/// drop(guard);
/// # }
/// ```
#[derive(Debug)]
#[must_use = "the context is only current while the guard exists"]
pub struct ToolCallGuard<'a> {
    _guard: ContextGuard,
    _call: PhantomData<&'a ToolCall>,
}
