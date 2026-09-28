# Changelog

## Unreleased

## 0.2.0

### Added

- Spans for `ConverseStream` calls, ended when the response stream finishes or is dropped. They include `gen_ai.request.stream`, `gen_ai.response.time_to_first_chunk`, the stop reason, token usage, and `error.type` for errors raised mid-stream.

## 0.1.0

### Added

- `BedrockInterceptor`, which records a span for each `Converse` call following the OpenTelemetry GenAI semantic conventions.
