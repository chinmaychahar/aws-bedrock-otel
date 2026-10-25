# Changelog

## Unreleased

## 0.3.0

### Added

- GenAI client metrics for `Converse` and `ConverseStream` calls: `gen_ai.client.inference.duration`, `gen_ai.client.inference.time_to_first_chunk`, `gen_ai.client.inference.time_per_output_chunk`, the `gen_ai.client.inference.operation.*_tokens` histograms and the `gen_ai.client.inference.usage.*` counters.
- `BedrockInterceptor::with_meter_provider` to record metrics with a given meter provider. The global meter provider is used otherwise.
- `BedrockInterceptor::without_spans` and `BedrockInterceptor::without_metrics` to record only metrics or only spans.

### Changed

- `new()` and `with_tracer_provider()` now also record metrics with the global meter provider. Use `without_metrics()` to turn them off.
- Calls dropped before they finish, for example by `tokio::time::timeout`, are now recorded with `error.type` set to `cancelled`, and the SDK's operation timeout with `timeout`. Previously their spans ended without an error.
- A `ConverseStream` dropped before `messageStop` is now recorded with `error.type` set to `cancelled`. Previously it ended without an error.
- Failed calls now have `gen_ai.response.finish_reasons` set to `error` when no finish reason was received.
- `error.type` falls back to `timeout` and then the HTTP status code before `_OTHER`.
- Bump `opentelemetry` to 0.33.1.

## 0.2.0

### Added

- Spans for `ConverseStream` calls, ended when the response stream finishes or is dropped. They include `gen_ai.request.stream`, `gen_ai.response.time_to_first_chunk`, the stop reason, token usage, and `error.type` for errors raised mid-stream.

## 0.1.0

### Added

- `BedrockInterceptor`, which records a span for each `Converse` call following the OpenTelemetry GenAI semantic conventions.
