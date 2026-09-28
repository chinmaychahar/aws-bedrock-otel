//! Tracing for `ConverseStream` responses.
//!
//! The span is ended by the response body, which reads the stop reason and token usage from the
//! stream events as they pass through.

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::body::{Error as BodyError, SdkBody};
use aws_smithy_types::event_stream::{HeaderValue, Message};
use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use opentelemetry::trace::{Status, TraceContextExt};
use opentelemetry::{Context, KeyValue};
use serde::Deserialize;

use crate::attributes::*;

/// Wraps a `ConverseStream` response body and ends the span when the stream finishes.
pub(crate) struct TracedBody {
    inner: SdkBody,
    recorder: Recorder,
}

impl TracedBody {
    pub(crate) fn new(inner: SdkBody, span_context: Context, started: Instant) -> Self {
        Self {
            inner,
            recorder: Recorder {
                span_context,
                started,
                decoder: MessageFrameDecoder::new(),
                buffer: BytesMut::new(),
                decoding: true,
                first_chunk_seen: false,
                ended: false,
            },
        }
    }
}

impl Body for TracedBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.recorder.observe(data);
                }
            }
            Poll::Ready(Some(Err(_))) => this.recorder.end_with_error(ERROR_TYPE_OTHER),
            Poll::Ready(None) => this.recorder.end(),
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        Body::size_hint(&self.inner)
    }
}

impl Drop for TracedBody {
    fn drop(&mut self) {
        // The stream may be dropped before it is read to the end.
        self.recorder.end();
    }
}

struct Recorder {
    span_context: Context,
    started: Instant,
    decoder: MessageFrameDecoder,
    buffer: BytesMut,
    decoding: bool,
    first_chunk_seen: bool,
    ended: bool,
}

impl Recorder {
    fn observe(&mut self, data: &Bytes) {
        if !self.decoding || self.ended {
            return;
        }
        self.buffer.extend_from_slice(data);
        loop {
            match self.decoder.decode_frame(&mut self.buffer) {
                Ok(DecodedFrame::Complete(message)) => self.handle(&message),
                Ok(DecodedFrame::Incomplete) => break,
                Err(_) => {
                    // Stop decoding malformed frames; the stream itself is unaffected.
                    self.decoding = false;
                    self.buffer = BytesMut::new();
                    break;
                }
            }
        }
    }

    fn handle(&mut self, message: &Message) {
        let span = self.span_context.span();
        match header(message, ":message-type") {
            Some("event") => match header(message, ":event-type") {
                Some("contentBlockDelta") if !self.first_chunk_seen => {
                    self.first_chunk_seen = true;
                    span.set_attribute(KeyValue::new(
                        GEN_AI_RESPONSE_TIME_TO_FIRST_CHUNK,
                        self.started.elapsed().as_secs_f64(),
                    ));
                }
                Some("messageStop") => {
                    if let Ok(MessageStop {
                        stop_reason: Some(reason),
                    }) = serde_json::from_slice(message.payload())
                    {
                        span.set_attribute(finish_reasons(&reason));
                    }
                }
                Some("metadata") => {
                    if let Ok(Metadata { usage: Some(usage) }) =
                        serde_json::from_slice(message.payload())
                    {
                        span.set_attributes(usage_attributes(
                            usage.input_tokens,
                            usage.output_tokens,
                            usage.cache_read_input_tokens,
                            usage.cache_write_input_tokens,
                        ));
                    }
                }
                _ => {}
            },
            Some("exception") => {
                let error_type = header(message, ":exception-type")
                    .map(capitalize)
                    .unwrap_or_else(|| ERROR_TYPE_OTHER.to_owned());
                self.end_with_error(&error_type);
            }
            _ => {}
        }
    }

    fn end_with_error(&mut self, error_type: &str) {
        if self.ended {
            return;
        }
        let span = self.span_context.span();
        span.set_attribute(KeyValue::new(ERROR_TYPE, error_type.to_owned()));
        span.set_status(Status::error(error_type.to_owned()));
        self.end();
    }

    fn end(&mut self) {
        if !self.ended {
            self.ended = true;
            self.span_context.span().end();
        }
    }
}

fn header<'a>(message: &'a Message, name: &str) -> Option<&'a str> {
    message
        .headers()
        .iter()
        .find(|header| header.name().as_str() == name)
        .and_then(|header| match header.value() {
            HeaderValue::String(value) => Some(value.as_str()),
            _ => None,
        })
}

/// Turns `modelStreamErrorException` into `ModelStreamErrorException`, like other error codes.
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStop {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct Metadata {
    usage: Option<Usage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::Header;
    use opentelemetry::trace::{Tracer, TracerProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

    fn event(name: &str, payload: &str) -> Vec<u8> {
        let message = Message::new(payload.as_bytes().to_vec())
            .add_header(Header::new(
                ":message-type",
                HeaderValue::String("event".into()),
            ))
            .add_header(Header::new(
                ":event-type",
                HeaderValue::String(name.to_owned().into()),
            ));
        let mut bytes = Vec::new();
        write_message_to(&message, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn decodes_frames_split_across_chunks() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let span = provider.tracer("test").start("chat");
        let mut body = TracedBody::new(
            SdkBody::empty(),
            Context::current_with_span(span),
            Instant::now(),
        );

        let mut bytes = event("messageStop", r#"{"stopReason":"max_tokens"}"#);
        bytes.extend(event(
            "metadata",
            r#"{"usage":{"inputTokens":7,"outputTokens":3}}"#,
        ));
        for byte in bytes {
            body.recorder.observe(&Bytes::from(vec![byte]));
        }
        drop(body);

        let spans = exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        let attributes = &spans[0].attributes;
        assert!(attributes.contains(&finish_reasons("max_tokens")));
        assert!(attributes.contains(&KeyValue::new(GEN_AI_USAGE_INPUT_TOKENS, 7_i64)));
        assert!(attributes.contains(&KeyValue::new(GEN_AI_USAGE_OUTPUT_TOKENS, 3_i64)));
    }
}
