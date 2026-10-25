# aws-bedrock-otel

[![Crates.io](https://img.shields.io/crates/v/aws-bedrock-otel.svg)](https://crates.io/crates/aws-bedrock-otel)
[![Docs.rs](https://docs.rs/aws-bedrock-otel/badge.svg)](https://docs.rs/aws-bedrock-otel)
[![CI](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml/badge.svg)](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/aws-bedrock-otel.svg)](LICENSE)

OpenTelemetry tracing for Rust apps that use the official [`aws-sdk-bedrockruntime`](https://docs.rs/aws-sdk-bedrockruntime) client. Add one interceptor and your Bedrock calls are traced as spans following the OpenTelemetry [GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai).

> **Status:** early development. `Converse` and `ConverseStream` calls are traced, `InvokeModel` is WIP

## Usage

Add `BedrockInterceptor` to your Bedrock Runtime client:

```rust
use aws_bedrock_otel::BedrockInterceptor;

let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
let config = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config)
    .interceptor(BedrockInterceptor::new())
    .build();
let client = aws_sdk_bedrockruntime::Client::from_conf(config);
```

Notes:
- `BedrockInterceptor::new()` uses the global tracer and meter providers. They are read when the interceptor is created, so set them first. Use `BedrockInterceptor::with_tracer_provider(&tracer_provider)` and `.with_meter_provider(&meter_provider)` to pass them explicitly
- `.without_spans()` records metrics only, for apps where another library (such as Rig) already creates spans for these calls. `.without_metrics()` records spans only
- A `ConverseStream` call ends when the response stream is read to the end or dropped. A stream dropped before `messageStop`, or a call dropped before it finishes (for example by `tokio::time::timeout`), is recorded with `error.type` set to `cancelled`
- Bedrock sends token usage in the last stream event, after `messageStop`. Read the stream to the end to record it
- Streams are timed as the caller reads them, so a slow reader adds to the durations. Chunks that arrive together are recorded close together
- Each `Converse` and `ConverseStream` call records a `chat {model}` span with the attributes below

### With `tracing`

With [`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry) 0.34 (the release for `opentelemetry` 0.33), spans join the trace of the current `tracing` span. This relies on the layer's `with_context_activation`, which is on by default. If the SDK's own debug spans are enabled, the `chat` span appears under the SDK's `apply_configuration` span.

### Spans


| Attribute | Source |
|---|---|
| `gen_ai.operation.name` | `chat` |
| `gen_ai.provider.name` | `aws.bedrock` |
| `gen_ai.request.model` | model ID |
| `gen_ai.request.max_tokens`, `gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.stop_sequences` | inference configuration, when set |
| `aws.bedrock.guardrail.id` | guardrail configuration, when set |
| `gen_ai.request.stream` | `true` for `ConverseStream` |
| `gen_ai.response.finish_reasons` | stop reason, or `error` when the call fails before one is received |
| `gen_ai.response.time_to_first_chunk` | seconds until the first content chunk, for `ConverseStream` |
| `gen_ai.usage.input_tokens` | input tokens, including cache reads and writes |
| `gen_ai.usage.output_tokens` | output tokens |
| `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens` | cached tokens, when reported |
| `error.type` | see below, when the call or the stream fails |

### Metrics

| Metric | Type | Recorded |
|---|---|---|
| `gen_ai.client.inference.duration` | histogram, seconds | for every call, including failed and cancelled calls |
| `gen_ai.client.inference.time_to_first_chunk` | histogram, seconds | for `ConverseStream`, when the first content chunk arrives |
| `gen_ai.client.inference.time_per_output_chunk` | histogram, seconds | for `ConverseStream`, for each content chunk after the first |
| `gen_ai.client.inference.operation.input_tokens`, `gen_ai.client.inference.operation.output_tokens` | histograms, tokens | for every call; `0` when there is no usage |
| `gen_ai.client.inference.usage.input_tokens`, `.output_tokens`, `.cache_read.input_tokens`, `.cache_write.input_tokens` | counters, tokens | when the count is above `0` |

Metrics have `gen_ai.operation.name`, `gen_ai.provider.name` and `gen_ai.request.model`. Durations and token histograms also have `error.type` when the call fails, and the usage counters have `gen_ai.token.modality` set to `unknown`.

### Errors

When the SDK returns an error, `error.type` is the first of these that applies:

1. the Bedrock error code, such as `ThrottlingException` or `ValidationException`
2. `timeout`, for SDK attempt timeouts and connection timeouts
3. the HTTP status code, such as `400`, when the error has no code
4. `_OTHER`

Otherwise:

- `timeout` when the SDK's operation timeout ends the call
- `cancelled` when the call is dropped before it returns, or its stream is dropped before `messageStop`
- the exception name, such as `ModelStreamErrorException`, when Bedrock sends an exception in the stream
- `_OTHER` when reading the stream fails, or another interceptor fails the call after the stream opened

## Examples

[`examples/stdout.rs`](examples/stdout.rs) sends one `Converse` request and prints the span and metrics to your terminal. It needs AWS credentials and access to the model:

```sh
BEDROCK_MODEL_ID=amazon.nova-lite-v1:0 cargo run --example stdout
```

## Minimum supported Rust version

Rust 1.94.1, the same as the AWS SDK for Rust

## License

Licensed under the [Apache License, Version 2.0](LICENSE)
