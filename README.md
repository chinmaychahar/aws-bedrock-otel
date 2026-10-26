# aws-bedrock-otel

[![Crates.io](https://img.shields.io/crates/v/aws-bedrock-otel.svg)](https://crates.io/crates/aws-bedrock-otel)
[![Docs.rs](https://docs.rs/aws-bedrock-otel/badge.svg)](https://docs.rs/aws-bedrock-otel)
[![CI](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml/badge.svg)](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/aws-bedrock-otel.svg)](LICENSE)

OpenTelemetry traces and metrics for Amazon Bedrock calls made with the official [`aws-sdk-bedrockruntime`](https://docs.rs/aws-sdk-bedrockruntime) Rust client.

Add one interceptor to your client. Every `Converse` and `ConverseStream` call is then recorded with its model, settings, token usage, latency and errors, following the OpenTelemetry [GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai).

> **Status:** early development. `Converse` and `ConverseStream` are supported. `InvokeModel` is not yet.

## Quick start

```rust
use aws_bedrock_otel::BedrockInterceptor;

let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
let config = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config)
    .interceptor(BedrockInterceptor::new())
    .build();
let client = aws_sdk_bedrockruntime::Client::from_conf(config);
```

That's it. Each call now creates a `chat {model}` span and records metrics. Set up your OpenTelemetry tracer and meter providers before creating the interceptor.

## What you get

- **Spans** with the model, request settings, token usage, finish reason, endpoint and AWS request ID
- **Metrics** for call duration, token usage and, for streams, time to first chunk and time per chunk
- **Errors** recorded with a clear `error.type`, including timeouts and cancelled calls
- **Streaming** handled correctly: the span ends when you finish reading the stream
- **Prompts and responses**, if you turn content capture on
- **Tool calls** your app runs, with `ToolTracer`

## Options

```rust
let interceptor = BedrockInterceptor::new()
    .tracer_provider(&tracer_provider) // default: the global tracer provider
    .meter_provider(&meter_provider)   // default: the global meter provider
    .spans(false)                      // turn spans off, keep metrics
    .metrics(false)                    // turn metrics off, keep spans
    .content_capture(true)             // record prompts and responses (off by default)
    .max_content_length(16 * 1024);    // cut each captured text to 16 KB
```

Use `.spans(false)` if another library, such as Rig, already creates spans for these calls.

## Streaming

- The call ends when you read the stream to the end, or drop it.
- Read the stream to the end to get token usage. Bedrock sends it in the last event.
- If you drop the stream early, the call is recorded as `cancelled`.

## Content capture

Prompts and responses can contain personal data, so they aren't recorded unless you turn this on:

```rust
let interceptor = BedrockInterceptor::new().content_capture(true);
```

You can also set `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true`. The builder option wins over the environment variable.

When on, spans get:

| Attribute | Contains |
|---|---|
| `gen_ai.system_instructions` | the system prompt |
| `gen_ai.input.messages` | the request messages |
| `gen_ai.output.messages` | the response (for streams, everything received) |
| `gen_ai.tool.definitions` | the type and name of each tool |

Good to know:

- Images, documents, audio and video are never recorded as bytes. Only their type is kept, or their S3 URI.
- The X-Ray OTLP endpoint [rejects spans over 200 KB](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/CloudWatch-OTLPEndpoint.html). Use `.max_content_length(bytes)` to cut long texts. The limit applies to each text, not the whole span.

## Tool calls

When the model asks for a tool, your app runs it. `ToolTracer` records each of those runs as an `execute_tool {tool}` span:

```rust
let interceptor = BedrockInterceptor::new();
let tools = interceptor.tool_tracer(); // after setting the interceptor's options

for block in message.content() {
    if let ContentBlock::ToolUse(tool_use) = block {
        let call = tools.tool_use(tool_use).start();
        let result = call.in_context(run_tool(tool_use)).await;
        call.end_with_result(&result);
    }
}
```

- End a call with `end_with_result(&result)`, `end()` or `fail("error type")`.
- A call dropped without ending is recorded as `cancelled`, or `_OTHER` if the tool panicked.
- `in_context` makes spans created by the tool children of its call.
- For tools that don't come from a `ToolUseBlock`, use `tools.tool("name")`.

## Using `tracing`

With [`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry) 0.34, Bedrock spans appear under your current `tracing` span. No extra setup is needed.

## Errors

`error.type` is set when a call fails:

| Value | When |
|---|---|
| Bedrock error code, such as `ThrottlingException` | Bedrock returned an error |
| `timeout` | an SDK timeout ended the call |
| HTTP status, such as `503` | the error had no code |
| exception name, such as `ModelStreamErrorException` | Bedrock sent an error inside the stream |
| `cancelled` | the call or its stream was dropped before it finished |
| `_OTHER` | anything else, such as a broken stream |

## Reference

<details>
<summary>Span attributes</summary>

| Attribute | Value |
|---|---|
| `gen_ai.operation.name` | `chat` |
| `gen_ai.provider.name` | `aws.bedrock` |
| `gen_ai.request.model` | model ID |
| `gen_ai.request.max_tokens`, `.temperature`, `.top_p`, `.stop_sequences` | inference settings, when set |
| `gen_ai.request.reasoning.level` | output configuration `effort`, when set |
| `gen_ai.request.stream` | `true` for `ConverseStream` |
| `gen_ai.output.type` | `json` when a JSON schema output is requested |
| `aws.bedrock.guardrail.id` | guardrail, when set |
| `server.address`, `server.port` | the endpoint |
| `aws.request_id` | AWS request ID of the last attempt |
| `gen_ai.response.finish_reasons` | stop reason, or `error` |
| `gen_ai.response.time_to_first_chunk` | seconds to the first chunk, for streams |
| `gen_ai.usage.input_tokens` | input tokens, including cached tokens |
| `gen_ai.usage.output_tokens` | output tokens |
| `gen_ai.usage.cache_read.input_tokens`, `.cache_write.input_tokens` | cached tokens, when not `0` |
| `error.type` | see [Errors](#errors) |

</details>

<details>
<summary>Metrics</summary>

| Metric | Type |
|---|---|
| `gen_ai.client.inference.duration` | histogram, seconds |
| `gen_ai.client.inference.time_to_first_chunk` | histogram, seconds (streams) |
| `gen_ai.client.inference.time_per_output_chunk` | histogram, seconds (streams) |
| `gen_ai.client.inference.operation.input_tokens`, `.output_tokens` | histograms, tokens |
| `gen_ai.client.inference.usage.input_tokens`, `.output_tokens`, `.cache_read.input_tokens`, `.cache_write.input_tokens` | counters, tokens |
| `gen_ai.execute_tool.duration` | histogram, seconds (`ToolTracer`) |

- Chat metrics have the operation, provider, model and endpoint. Durations and token histograms also have `error.type` when a call fails.
- The tool metric has the tool name and type, and `error.type` when the tool fails.

</details>

<details>
<summary>Details and edge cases</summary>

- Durations include request signing, retries and the waits between them.
- Streams are timed as you read them, so a slow reader makes durations longer.
- Add one `BedrockInterceptor` per client. If you add more, each call is still recorded once.
- Captured values are JSON strings that follow the GenAI message schemas. Tools that Bedrock runs itself are recorded as `server_tool_call` parts.
- Cut JSON values are recorded as strings.
- Tool results are recorded as `gen_ai.tool.call.result`. JSON results stay objects; plain text is recorded as a string.

</details>

## Compatibility

| `aws-bedrock-otel` | `opentelemetry` | GenAI semantic conventions |
|---|---|---|
| 0.3 | 0.33 | [`9ffc7e0`](https://github.com/open-telemetry/semantic-conventions-genai/tree/9ffc7e00c639be7712e544e3e54331e885dccf84) (2026-10-21) |
| 0.1, 0.2 | 0.33 | |

- Use the same `opentelemetry` version as your app. With a different version, nothing is recorded.
- The GenAI conventions are still changing. Name changes come in minor releases and are listed in the [CHANGELOG](CHANGELOG.md).

## Example

[`examples/stdout.rs`](examples/stdout.rs) sends one request and prints the span and metrics. It needs AWS credentials:

```sh
BEDROCK_MODEL_ID=amazon.nova-lite-v1:0 cargo run --example stdout
```

## Minimum supported Rust version

Rust 1.94.1, the same as the AWS SDK for Rust.

## License

[Apache License, Version 2.0](LICENSE)
