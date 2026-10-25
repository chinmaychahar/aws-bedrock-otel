//! A single traced call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use opentelemetry::trace::{SpanRef, Status, TraceContextExt};
use opentelemetry::{Context, KeyValue};

use crate::attributes::{
    ERROR_TYPE, FINISH_REASON_ERROR, TokenUsage, finish_reasons, usage_attributes,
};
use crate::metrics::Metrics;

/// A `Converse` or `ConverseStream` call. It is finished exactly once, by whichever part sees
/// its end first.
#[derive(Debug)]
pub(crate) struct Call {
    context: Context,
    started: Instant,
    metrics: Option<Metrics>,
    metric_attributes: Vec<KeyValue>,
    stream_delivered: AtomicBool,
    request_done: AtomicBool,
    finished: AtomicBool,
}

impl Call {
    pub(crate) fn new(
        context: Context,
        metrics: Option<Metrics>,
        metric_attributes: Vec<KeyValue>,
    ) -> Self {
        Self {
            context,
            started: Instant::now(),
            metrics,
            metric_attributes,
            stream_delivered: AtomicBool::new(false),
            request_done: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        }
    }

    pub(crate) fn span(&self) -> SpanRef<'_> {
        self.context.span()
    }

    pub(crate) fn started(&self) -> Instant {
        self.started
    }

    /// Marks the response stream as handed to the caller. From then on the stream ends the call.
    pub(crate) fn deliver_stream(&self) {
        self.stream_delivered.store(true, Ordering::Release);
    }

    pub(crate) fn is_stream_delivered(&self) -> bool {
        self.stream_delivered.load(Ordering::Acquire)
    }

    /// Marks the SDK request as returned to the caller.
    pub(crate) fn complete_request(&self) {
        self.request_done.store(true, Ordering::Release);
    }

    pub(crate) fn is_request_done(&self) -> bool {
        self.request_done.load(Ordering::Acquire)
    }

    pub(crate) fn record_time_to_first_chunk(&self, elapsed: Duration) {
        if let Some(metrics) = &self.metrics {
            metrics.record_time_to_first_chunk(&self.metric_attributes, elapsed);
        }
    }

    pub(crate) fn record_time_per_output_chunk(&self, elapsed: Duration) {
        if let Some(metrics) = &self.metrics {
            metrics.record_time_per_output_chunk(&self.metric_attributes, elapsed);
        }
    }

    /// Ends the span and records the metrics. Calls after the first are ignored.
    ///
    /// Failed calls without a finish reason get `error`, as the conventions recommend.
    pub(crate) fn finish(
        &self,
        usage: Option<&TokenUsage>,
        finish_reason: Option<&str>,
        error_type: Option<&str>,
    ) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let span = self.context.span();
        let finish_reason = finish_reason.or(error_type.map(|_| FINISH_REASON_ERROR));
        if let Some(reason) = finish_reason {
            span.set_attribute(finish_reasons(reason));
        }
        if let Some(usage) = usage {
            span.set_attributes(usage_attributes(usage));
        }
        if let Some(error_type) = error_type {
            span.set_attribute(KeyValue::new(ERROR_TYPE, error_type.to_owned()));
            span.set_status(Status::error(error_type.to_owned()));
        }
        if let Some(metrics) = &self.metrics {
            metrics.record_call(
                &self.metric_attributes,
                self.started.elapsed(),
                usage,
                error_type,
            );
        }
        span.end();
    }
}
