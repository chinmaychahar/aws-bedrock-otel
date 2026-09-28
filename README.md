# aws-bedrock-otel

[![Crates.io](https://img.shields.io/crates/v/aws-bedrock-otel.svg)](https://crates.io/crates/aws-bedrock-otel)
[![Docs.rs](https://docs.rs/aws-bedrock-otel/badge.svg)](https://docs.rs/aws-bedrock-otel)
[![CI](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml/badge.svg)](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/aws-bedrock-otel.svg)](LICENSE)

OpenTelemetry tracing for Rust apps that use the official [`aws-sdk-bedrockruntime`](https://docs.rs/aws-sdk-bedrockruntime) client. Add one interceptor and your Bedrock calls are traced as spans following the OpenTelemetry [GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai).

> **Status:** early development. `Converse` and `ConverseStream` calls are traced; `InvokeModel` is not yet.

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

`BedrockInterceptor::new()` uses the global tracer provider. Use `BedrockInterceptor::with_tracer_provider(&provider)` to pass one explicitly.

Each `Converse` and `ConverseStream` call records a `chat {model}` span with these attributes. A `ConverseStream` span ends when the response stream is read to the end or dropped.

| Attribute | Source |
|---|---|
| `gen_ai.operation.name` | `chat` |
| `gen_ai.provider.name` | `aws.bedrock` |
| `gen_ai.request.model` | model ID |
| `gen_ai.request.max_tokens`, `gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.stop_sequences` | inference configuration, when set |
| `aws.bedrock.guardrail.id` | guardrail configuration, when set |
| `gen_ai.request.stream` | `true` for `ConverseStream` |
| `gen_ai.response.finish_reasons` | stop reason |
| `gen_ai.response.time_to_first_chunk` | seconds until the first streamed text, for `ConverseStream` |
| `gen_ai.usage.input_tokens` | input tokens, including cache reads and writes |
| `gen_ai.usage.output_tokens` | output tokens |
| `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens` | cached tokens, when reported |
| `error.type` | error code, when the call or the stream fails |

## Examples

[`examples/stdout.rs`](examples/stdout.rs) sends one `Converse` request and prints the span to your terminal. It needs AWS credentials and access to the model:

```sh
BEDROCK_MODEL_ID=amazon.nova-lite-v1:0 cargo run --example stdout
```

## Minimum supported Rust version

Rust 1.94.1, the same as the AWS SDK for Rust.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
