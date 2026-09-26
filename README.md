# aws-bedrock-otel

[![Crates.io](https://img.shields.io/crates/v/aws-bedrock-otel.svg)](https://crates.io/crates/aws-bedrock-otel)
[![Docs.rs](https://docs.rs/aws-bedrock-otel/badge.svg)](https://docs.rs/aws-bedrock-otel)
[![CI](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml/badge.svg)](https://github.com/chinmaychahar/aws-bedrock-otel/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/aws-bedrock-otel.svg)](LICENSE)

OpenTelemetry tracing for Rust apps that use the official [`aws-sdk-bedrockruntime`](https://docs.rs/aws-sdk-bedrockruntime) client. Add one interceptor and your Bedrock calls are traced as spans following the OpenTelemetry [GenAI semantic conventions](https://github.com/open-telemetry/semantic-conventions-genai).

> **Status:** early development. Only non-streaming `Converse` calls are traced for now.

## Usage

Add `BedrockInterceptor` to your Bedrock Runtime client:

```rust
use aws_bedrock_otel::BedrockInterceptor;

let sdk_config = aws_config::load_from_env().await;
let config = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config)
    .interceptor(BedrockInterceptor::new())
    .build();
let client = aws_sdk_bedrockruntime::Client::from_conf(config);
```

`BedrockInterceptor::new()` uses the global tracer provider. Use `BedrockInterceptor::with_tracer_provider(&provider)` to pass one explicitly.

Each `Converse` call records a `chat {model}` span with these attributes:

| Attribute | Source |
|---|---|
| `gen_ai.operation.name` | `chat` |
| `gen_ai.provider.name` | `aws.bedrock` |
| `gen_ai.request.model` | model ID |
| `gen_ai.request.max_tokens`, `gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.stop_sequences` | inference configuration, when set |
| `aws.bedrock.guardrail.id` | guardrail configuration, when set |
| `gen_ai.response.finish_reasons` | stop reason |
| `gen_ai.usage.input_tokens` | input tokens, including cache reads and writes |
| `gen_ai.usage.output_tokens` | output tokens |
| `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens` | cached tokens, when reported |
| `error.type` | error code, when the call fails |

## Minimum supported Rust version

Rust 1.94.1, the same as the AWS SDK for Rust.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
