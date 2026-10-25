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
- Add one `BedrockInterceptor` per client. If there are several, the first one to see a call traces it, and each call is still recorded once. Client-level interceptors see calls before ones added with `customize()`, so a `customize()` interceptor is ignored when the client already has one
- A `ConverseStream` call ends when the response stream is read to the end or dropped. A stream dropped before `messageStop`, or a call dropped before it finishes (for example by `tokio::time::timeout`), is recorded with `error.type` set to `cancelled`
- Bedrock sends token usage in the last stream event, after `messageStop`. Read the stream to the end to record it
- Streams are timed as the caller reads them, so a slow reader adds to the durations. Chunks that arrive together are recorded close together
- Each `Converse` and `ConverseStream` call records a `chat {model}` span with the attributes below

### With `tracing`

With [`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry) 0.34 (the release for `opentelemetry` 0.33), spans join the trace of the current `tracing` span. This relies on the layer's `with_context_activation`, which is on by default. If the SDK's own debug spans are enabled, the `chat` span appears under one of them instead of directly under your span.

### Spans


| Attribute | Source |
|---|---|
| `gen_ai.operation.name` | `chat` |
| `gen_ai.provider.name` | `aws.bedrock` |
| `gen_ai.request.model` | model ID |
| `gen_ai.request.max_tokens`, `gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.stop_sequences` | inference configuration, when set |
| `aws.bedrock.guardrail.id` | guardrail configuration, when set |
| `gen_ai.output.type` | `json` when a JSON schema output format is requested |
| `gen_ai.request.reasoning.level` | output configuration `effort`, when set. Reasoning set in `additionalModelRequestFields` is not recorded |
| `server.address`, `server.port` | endpoint the request is sent to. Added after the span starts, so samplers don't see them |
| `aws.request_id` | AWS request ID of the latest attempt that got a response (an AWS SDK attribute, not a GenAI one) |
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

Metrics have `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `server.address` and `server.port`. Durations and token histograms also have `error.type` when the call fails, and the usage counters have `gen_ai.token.modality` set to `unknown`.

### Errors

When the interceptor sees the SDK's error, `error.type` is the first of these that applies:

1. the Bedrock error code, such as `ThrottlingException` or `ValidationException`
2. `timeout`, for SDK attempt timeouts and connection timeouts
3. the HTTP status code, such as `400`, when the error has no code
4. `_OTHER`

Otherwise:

- `timeout` when the SDK's operation timeout ends the call
- `cancelled` when the call is dropped before it returns, or its stream is dropped before `messageStop`
- the exception name, such as `ModelStreamErrorException`, when Bedrock sends an exception in the stream
- `_OTHER` when reading the stream fails, its framing, message type or event type is invalid, the `messageStop` or `metadata` event can't be parsed, it ends before `messageStop`, or another interceptor fails the call after the stream opened. Other events the SDK can't parse show up as `cancelled` when the caller drops the stream

### Content capture

Prompts and responses aren't recorded by default, because they can contain personal or sensitive data. Turn capture on with `.with_content_capture(true)`, or set `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`. The builder option overrides the environment variable. Content is recorded on spans, so it needs spans on.

| Attribute | Content |
|---|---|
| `gen_ai.system_instructions` | system prompt |
| `gen_ai.input.messages` | request messages, including tool calls and tool results |
| `gen_ai.output.messages` | response message. For `ConverseStream`, rebuilt from the stream, including the part received before a stream fails or is dropped |
| `gen_ai.tool.definitions` | the type and name of each tool |

Values are JSON strings that follow the GenAI [message schemas](https://github.com/open-telemetry/semantic-conventions-genai). Images, documents, audio and video sent as bytes are recorded as their type and MIME type, without the bytes. Media in S3 is recorded with its S3 URI.

Content isn't cut by default. The [X-Ray OTLP endpoint](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/CloudWatch-OTLPEndpoint.html) rejects spans larger than 200 KB. Use `.with_max_content_length(bytes)` to cut each text, tool call argument and JSON tool result to that many bytes. Cut JSON values are recorded as strings. The limit applies to each value, not to the whole span, so a long conversation can still go over:

```rust
use aws_bedrock_otel::BedrockInterceptor;

let interceptor = BedrockInterceptor::new()
    .with_content_capture(true)
    .with_max_content_length(16 * 1024);
```

## Examples

[`examples/stdout.rs`](examples/stdout.rs) sends one `Converse` request and prints the span and metrics to your terminal. It needs AWS credentials and access to the model:

```sh
BEDROCK_MODEL_ID=amazon.nova-lite-v1:0 cargo run --example stdout
```

## Minimum supported Rust version

Rust 1.94.1, the same as the AWS SDK for Rust

## License

Licensed under the [Apache License, Version 2.0](LICENSE)
