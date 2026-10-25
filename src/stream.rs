//! Tracing for `ConverseStream` responses.
//!
//! The response body ends the call. It reads the stop reason, token usage and chunk timings from
//! the stream events as they pass through.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use aws_smithy_eventstream::frame::{DecodedFrame, MessageFrameDecoder};
use aws_smithy_types::body::{Error as BodyError, SdkBody};
use aws_smithy_types::event_stream::{HeaderValue, Message};
use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use opentelemetry::KeyValue;
use serde::Deserialize;

use crate::attributes::*;
use crate::call::Call;
use crate::content::StreamedMessage;

/// Wraps a `ConverseStream` response body and ends the call when the stream finishes.
pub(crate) struct TracedBody {
    inner: SdkBody,
    recorder: Recorder,
}

impl TracedBody {
    pub(crate) fn new(inner: SdkBody, call: Arc<Call>) -> Self {
        let streamed = call
            .capture()
            .map(|capture| StreamedMessage::new(capture.max_length));
        Self {
            inner,
            recorder: Recorder {
                call,
                streamed,
                usage: None,
                stop_reason: None,
                message_stopped: false,
                last_chunk: None,
                decoder: MessageFrameDecoder::new(),
                buffer: BytesMut::new(),
                decoding: true,
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
            Poll::Ready(Some(Err(_))) => this.recorder.finish(Some(ERROR_TYPE_OTHER)),
            // A stream that ends before `messageStop`, or inside a frame, was cut short.
            Poll::Ready(None) => {
                let cut_short = !this.recorder.message_stopped || !this.recorder.buffer.is_empty();
                this.recorder.finish(cut_short.then_some(ERROR_TYPE_OTHER));
            }
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
        let error_type = if self.recorder.message_stopped {
            None
        } else if self.recorder.call.is_request_done() {
            // The caller dropped the stream before the message ended.
            Some(ERROR_TYPE_CANCELLED)
        } else {
            // The request failed after the stream was opened, e.g. in another interceptor.
            Some(ERROR_TYPE_OTHER)
        };
        self.recorder.finish(error_type);
    }
}

struct Recorder {
    call: Arc<Call>,
    /// The response message, when content capture is on.
    streamed: Option<StreamedMessage>,
    usage: Option<TokenUsage>,
    stop_reason: Option<String>,
    message_stopped: bool,
    last_chunk: Option<Instant>,
    decoder: MessageFrameDecoder,
    buffer: BytesMut,
    decoding: bool,
}

impl Recorder {
    fn observe(&mut self, data: &Bytes) {
        if !self.decoding {
            return;
        }
        self.buffer.extend_from_slice(data);
        loop {
            match self.decoder.decode_frame(&mut self.buffer) {
                Ok(DecodedFrame::Complete(message)) => {
                    self.handle(&message);
                    if !self.decoding {
                        break;
                    }
                }
                Ok(DecodedFrame::Incomplete) => break,
                Err(_) => {
                    // The SDK fails to decode the same frame, so the stream has failed.
                    self.buffer = BytesMut::new();
                    self.finish(Some(ERROR_TYPE_OTHER));
                    break;
                }
            }
        }
    }

    fn handle(&mut self, message: &Message) {
        match header(message, ":message-type") {
            Some("event") => match header(message, ":event-type") {
                Some("contentBlockStart") => {
                    if let Some(streamed) = &mut self.streamed {
                        streamed.start(message.payload());
                    }
                }
                Some("contentBlockDelta") => {
                    self.record_chunk();
                    if let Some(streamed) = &mut self.streamed {
                        streamed.delta(message.payload());
                    }
                }
                Some("messageStop") => {
                    self.message_stopped = true;
                    match serde_json::from_slice::<MessageStop>(message.payload()) {
                        Ok(MessageStop { stop_reason }) => self.stop_reason = stop_reason,
                        // The SDK fails on the same payload.
                        Err(_) => self.finish(Some(ERROR_TYPE_OTHER)),
                    }
                }
                Some("metadata") => match serde_json::from_slice::<Metadata>(message.payload()) {
                    Ok(Metadata { usage }) => {
                        // Like the SDK, missing counts are 0.
                        self.usage = usage.map(|usage| TokenUsage {
                            input: usage.input_tokens.unwrap_or_default(),
                            output: usage.output_tokens.unwrap_or_default(),
                            cache_read: usage.cache_read_input_tokens,
                            cache_write: usage.cache_write_input_tokens,
                        });
                    }
                    Err(_) => self.finish(Some(ERROR_TYPE_OTHER)),
                },
                Some(_) => {}
                // The SDK fails on events without a type.
                None => self.finish(Some(ERROR_TYPE_OTHER)),
            },
            Some("exception") => {
                let error_type = header(message, ":exception-type")
                    .map(capitalize)
                    .unwrap_or_else(|| ERROR_TYPE_OTHER.to_owned());
                self.finish(Some(&error_type));
            }
            // The SDK fails on any other message type.
            _ => self.finish(Some(ERROR_TYPE_OTHER)),
        }
    }

    fn record_chunk(&mut self) {
        let now = Instant::now();
        match self.last_chunk {
            None => {
                let elapsed = now.duration_since(self.call.started());
                self.call.span().set_attribute(KeyValue::new(
                    GEN_AI_RESPONSE_TIME_TO_FIRST_CHUNK,
                    elapsed.as_secs_f64(),
                ));
                self.call.record_time_to_first_chunk(elapsed);
            }
            Some(previous) => self
                .call
                .record_time_per_output_chunk(now.duration_since(previous)),
        }
        self.last_chunk = Some(now);
    }

    /// Ends the call, unless the stream never reached the caller. In that case the request
    /// failed and the interceptor ends the call.
    fn finish(&mut self, error_type: Option<&str>) {
        self.decoding = false;
        if self.call.is_stream_delivered() {
            // Output that was received is recorded, even if the stream failed.
            if let Some(streamed) = self.streamed.take() {
                self.call
                    .set_output(|capture| streamed.output_messages(capture));
            }
            self.call
                .finish(self.usage.as_ref(), self.stop_reason.as_deref(), error_type);
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
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_eventstream::frame::write_message_to;
    use aws_smithy_types::event_stream::Header;
    use opentelemetry::Context;
    use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};
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
        let call = Arc::new(Call::new(
            Context::current_with_span(span),
            None,
            None,
            Vec::new(),
        ));
        call.deliver_stream();
        let mut body = TracedBody::new(SdkBody::empty(), call);

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
