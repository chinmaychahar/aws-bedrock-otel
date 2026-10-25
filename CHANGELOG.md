# Changelog

## Unreleased

## 0.3.0

### Added

- GenAI client metrics for `Converse` and `ConverseStream` calls: `gen_ai.client.inference.duration`, `gen_ai.client.inference.time_to_first_chunk`, `gen_ai.client.inference.time_per_output_chunk`, the `gen_ai.client.inference.operation.*_tokens` histograms and the `gen_ai.client.inference.usage.*` counters.
- `BedrockInterceptor::meter_provider` to record metrics with a given meter provider. The global meter provider is used otherwise.
- `BedrockInterceptor::spans` and `BedrockInterceptor::metrics` to turn spans or metrics off.
- Span attributes `server.address`, `server.port`, `aws.request_id`, `gen_ai.output.type` and `gen_ai.request.reasoning.level`. Metrics also have `server.address` and `server.port`.
- Opt-in content capture with `BedrockInterceptor::content_capture` or `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`: `gen_ai.system_instructions`, `gen_ai.input.messages`, `gen_ai.output.messages` and `gen_ai.tool.definitions`, including tool calls, tool results, tools that Bedrock runs and reasoning. Streamed responses are rebuilt from their events.
- `BedrockInterceptor::tool_tracer`, which returns a `ToolTracer` that records the app's tool calls as `execute_tool` spans and in the `gen_ai.execute_tool.duration` metric, with `gen_ai.tool.call.arguments` and `gen_ai.tool.call.result` when content capture is on.
- `BedrockInterceptor::max_content_length` to cut each captured text, tool call argument and JSON tool result to a number of bytes. Cut JSON values are recorded as strings.

### Changed

- **Breaking:** `BedrockInterceptor::with_tracer_provider(&provider)` is replaced by `BedrockInterceptor::new().tracer_provider(&provider)`, so all options are set the same way.
- **Prompts can be captured after upgrading:** if `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` is already set, for example for other OpenTelemetry instrumentations, prompts and responses are now recorded. Use `.content_capture(false)` to keep them off.
- `new()` now also records metrics with the global meter provider. Use `.metrics(false)` to turn them off.
- `gen_ai.usage.cache_read.input_tokens` and `gen_ai.usage.cache_write.input_tokens` are no longer recorded when they are `0`.
- Spans and the duration metric now have the same start and duration.
- A call with an empty model ID has a span named `chat` and no `gen_ai.request.model`.
- Calls dropped before they finish, for example by `tokio::time::timeout`, are now recorded with `error.type` set to `cancelled`, and the SDK's operation timeout with `timeout`. Previously their spans ended without an error.
- A `ConverseStream` dropped before `messageStop` is now recorded with `error.type` set to `cancelled`. Previously it ended without an error.
- A `ConverseStream` that ends before `messageStop`, has invalid framing, an unknown or missing message type, an event without a type, or a `messageStop` or `metadata` event that can't be parsed, is now recorded with `error.type` set to `_OTHER`. Previously it ended without an error.
- Streamed token usage with a missing input or output count now records that count as `0`, as the SDK does. Previously the usage was dropped.
- Failed calls now have `gen_ai.response.finish_reasons` set to `error` when no finish reason was received.
- A call is recorded once even when the interceptor is registered more than once on a client.
- `error.type` falls back to `timeout` and then the HTTP status code before `_OTHER`.
- Bump `opentelemetry` to 0.33.1, with its `metrics` feature.

## 0.2.0

### Added

- Spans for `ConverseStream` calls, ended when the response stream finishes or is dropped. They include `gen_ai.request.stream`, `gen_ai.response.time_to_first_chunk`, the stop reason, token usage, and `error.type` for errors raised mid-stream.

## 0.1.0

### Added

- `BedrockInterceptor`, which records a span for each `Converse` call following the OpenTelemetry GenAI semantic conventions.
