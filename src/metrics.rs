//! GenAI client metrics.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::attributes::{ERROR_TYPE, GEN_AI_TOKEN_MODALITY, TOKEN_MODALITY_UNKNOWN, TokenUsage};

const DURATION_BOUNDARIES: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
const TOKEN_BOUNDARIES: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

#[derive(Debug, Clone)]
pub(crate) struct Metrics {
    duration: Histogram<f64>,
    time_to_first_chunk: Histogram<f64>,
    time_per_output_chunk: Histogram<f64>,
    input_tokens: Histogram<u64>,
    output_tokens: Histogram<u64>,
    usage_input: Counter<u64>,
    usage_output: Counter<u64>,
    usage_cache_read: Counter<u64>,
    usage_cache_write: Counter<u64>,
}

impl Metrics {
    pub(crate) fn new(meter: &Meter) -> Self {
        let seconds = |name: &'static str, description: &'static str| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(description)
                .with_boundaries(DURATION_BOUNDARIES.to_vec())
                .build()
        };
        let tokens = |name: &'static str, description: &'static str| {
            meter
                .u64_histogram(name)
                .with_unit("{token}")
                .with_description(description)
                .with_boundaries(TOKEN_BOUNDARIES.to_vec())
                .build()
        };
        let counter = |name: &'static str, description: &'static str| {
            meter
                .u64_counter(name)
                .with_unit("{token}")
                .with_description(description)
                .build()
        };

        Self {
            duration: seconds(
                "gen_ai.client.inference.duration",
                "GenAI client inference operation duration.",
            ),
            time_to_first_chunk: seconds(
                "gen_ai.client.inference.time_to_first_chunk",
                "Time to receive the first chunk, measured from when the client issues the generation request to when the first chunk is received in the response stream.",
            ),
            time_per_output_chunk: seconds(
                "gen_ai.client.inference.time_per_output_chunk",
                "Time per output chunk, recorded for each chunk received after the first one, measured as the time elapsed from the end of the previous chunk to the end of the current chunk.",
            ),
            input_tokens: tokens(
                "gen_ai.client.inference.operation.input_tokens",
                "Input tokens used per inference operation.",
            ),
            output_tokens: tokens(
                "gen_ai.client.inference.operation.output_tokens",
                "Output tokens used per inference operation.",
            ),
            usage_input: counter(
                "gen_ai.client.inference.usage.input_tokens",
                "Input tokens used, including cached tokens.",
            ),
            usage_output: counter(
                "gen_ai.client.inference.usage.output_tokens",
                "Output tokens used.",
            ),
            usage_cache_read: counter(
                "gen_ai.client.inference.usage.cache_read.input_tokens",
                "Input tokens read from the cache.",
            ),
            usage_cache_write: counter(
                "gen_ai.client.inference.usage.cache_write.input_tokens",
                "Input tokens written to the cache.",
            ),
        }
    }

    /// Records a finished call. `attributes` are the shared metric attributes.
    pub(crate) fn record_call(
        &self,
        attributes: &[KeyValue],
        duration: Duration,
        usage: Option<&TokenUsage>,
        error_type: Option<&str>,
    ) {
        let mut with_error = attributes.to_vec();
        if let Some(error_type) = error_type {
            with_error.push(KeyValue::new(ERROR_TYPE, error_type.to_owned()));
        }
        self.duration.record(duration.as_secs_f64(), &with_error);

        // The convention records 0 when the response has no usage.
        let usage = usage.copied().unwrap_or_default();
        self.input_tokens
            .record(non_negative(usage.total_input()), &with_error);
        self.output_tokens
            .record(non_negative(usage.output), &with_error);

        let mut with_modality = attributes.to_vec();
        with_modality.push(KeyValue::new(GEN_AI_TOKEN_MODALITY, TOKEN_MODALITY_UNKNOWN));
        // Counters only take strictly positive contributions.
        let counts = [
            (&self.usage_input, Some(usage.total_input())),
            (&self.usage_output, Some(usage.output)),
            (&self.usage_cache_read, usage.cache_read),
            (&self.usage_cache_write, usage.cache_write),
        ];
        for (counter, tokens) in counts {
            if let Some(tokens) = tokens.filter(|tokens| *tokens > 0) {
                counter.add(non_negative(tokens), &with_modality);
            }
        }
    }

    pub(crate) fn record_time_to_first_chunk(&self, attributes: &[KeyValue], elapsed: Duration) {
        self.time_to_first_chunk
            .record(elapsed.as_secs_f64(), attributes);
    }

    pub(crate) fn record_time_per_output_chunk(&self, attributes: &[KeyValue], elapsed: Duration) {
        self.time_per_output_chunk
            .record(elapsed.as_secs_f64(), attributes);
    }
}

fn non_negative(tokens: i64) -> u64 {
    u64::try_from(tokens).unwrap_or(0)
}
