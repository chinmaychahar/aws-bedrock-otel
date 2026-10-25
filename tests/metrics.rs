mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::SharedInterceptor;
use aws_sdk_bedrockruntime::config::interceptors::AfterDeserializationInterceptorContextRef;
use aws_sdk_bedrockruntime::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_bedrockruntime::error::BoxError;
use aws_sdk_bedrockruntime::error::ErrorMetadata;
use aws_sdk_bedrockruntime::operation::converse::ConverseError;
use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::SdkTracerProvider;

use common::{
    Point, converse_output, event, exception, full_stream, metric_points, repeated_stream_rule,
    stream_rule, stream_rule_with_trailer,
};

const DURATION: &str = "gen_ai.client.inference.duration";
const TIME_TO_FIRST_CHUNK: &str = "gen_ai.client.inference.time_to_first_chunk";
const TIME_PER_OUTPUT_CHUNK: &str = "gen_ai.client.inference.time_per_output_chunk";
const INPUT_TOKENS: &str = "gen_ai.client.inference.operation.input_tokens";
const OUTPUT_TOKENS: &str = "gen_ai.client.inference.operation.output_tokens";
const USAGE_INPUT: &str = "gen_ai.client.inference.usage.input_tokens";
const USAGE_OUTPUT: &str = "gen_ai.client.inference.usage.output_tokens";
const USAGE_CACHE_READ: &str = "gen_ai.client.inference.usage.cache_read.input_tokens";
const USAGE_CACHE_WRITE: &str = "gen_ai.client.inference.usage.cache_write.input_tokens";
const USAGE_COUNTERS: [&str; 4] = [
    USAGE_INPUT,
    USAGE_OUTPUT,
    USAGE_CACHE_READ,
    USAGE_CACHE_WRITE,
];

const DURATION_BOUNDARIES: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
const TOKEN_BOUNDARIES: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

const MODEL: &str = "amazon.nova-lite";

#[derive(Debug)]
struct FailAfterDeserialization;

impl Intercept for FailAfterDeserialization {
    fn name(&self) -> &'static str {
        "FailAfterDeserialization"
    }

    fn read_after_deserialization(
        &self,
        _context: &AfterDeserializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        Err("failed".into())
    }
}

struct Setup {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Setup {
    fn new() -> Self {
        let exporter = InMemoryMetricExporter::default();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        Self {
            tracer_provider: SdkTracerProvider::builder().build(),
            meter_provider,
            exporter,
        }
    }

    fn client(&self, rule: &Rule) -> Client {
        let tracer_provider = &self.tracer_provider;
        let meter_provider = &self.meter_provider;
        mock_client!(
            aws_sdk_bedrockruntime,
            RuleMode::Sequential,
            &[rule],
            move |config| config.interceptor(
                BedrockInterceptor::with_tracer_provider(tracer_provider)
                    .with_meter_provider(meter_provider)
            )
        )
    }

    /// A client with another interceptor that fails after the response is deserialized.
    fn client_failing_after_deserialization(&self, rule: &Rule) -> Client {
        let tracer_provider = &self.tracer_provider;
        let meter_provider = &self.meter_provider;
        mock_client!(
            aws_sdk_bedrockruntime,
            RuleMode::Sequential,
            &[rule],
            move |config| config
                .interceptor(
                    BedrockInterceptor::with_tracer_provider(tracer_provider)
                        .with_meter_provider(meter_provider)
                )
                .interceptor(FailAfterDeserialization)
        )
    }

    fn points(&self, name: &str) -> Vec<Point> {
        metric_points(&self.meter_provider, &self.exporter, name)
    }

    /// The single data point of the metric `name`.
    fn point(&self, name: &str) -> Point {
        let mut points = self.points(name);
        assert_eq!(points.len(), 1, "expected one data point for {name}");
        points.remove(0)
    }
}

fn base_attributes() -> Vec<KeyValue> {
    vec![
        KeyValue::new("gen_ai.operation.name", "chat"),
        KeyValue::new("gen_ai.provider.name", "aws.bedrock"),
        KeyValue::new("gen_ai.request.model", MODEL),
        KeyValue::new("server.address", "bedrock-runtime.us-east-1.amazonaws.com"),
        KeyValue::new("server.port", 443),
    ]
}

fn with(mut attributes: Vec<KeyValue>, extra: KeyValue) -> Vec<KeyValue> {
    attributes.push(extra);
    attributes
}

fn assert_attributes(point: &Point, expected: &[KeyValue]) {
    let mut actual = point.attributes.clone();
    let mut expected = expected.to_vec();
    actual.sort_by(|a, b| a.key.cmp(&b.key));
    expected.sort_by(|a, b| a.key.cmp(&b.key));
    assert_eq!(actual, expected);
}

fn assert_token_metrics(setup: &Setup, attributes: &[KeyValue]) {
    let input = setup.point(INPUT_TOKENS);
    assert_eq!((input.count, input.sum), (1, 15.0));
    assert_attributes(&input, attributes);
    let output = setup.point(OUTPUT_TOKENS);
    assert_eq!((output.count, output.sum), (1, 5.0));
    assert_attributes(&output, attributes);

    let modality = with(
        base_attributes(),
        KeyValue::new("gen_ai.token.modality", "unknown"),
    );
    for (name, tokens) in [
        (USAGE_INPUT, 15.0),
        (USAGE_OUTPUT, 5.0),
        (USAGE_CACHE_READ, 3.0),
        (USAGE_CACHE_WRITE, 2.0),
    ] {
        let point = setup.point(name);
        assert_eq!(point.sum, tokens, "{name}");
        assert_attributes(&point, &modality);
    }
}

fn validation_error() -> ValidationException {
    ValidationException::builder()
        .meta(ErrorMetadata::builder().code("ValidationException").build())
        .build()
}

#[tokio::test]
async fn converse_records_duration_and_tokens() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule);

    client.converse().model_id(MODEL).send().await.unwrap();

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert!(duration.sum > 0.0 && duration.sum < 5.0, "{}", duration.sum);
    assert_eq!(duration.bounds, DURATION_BOUNDARIES);
    assert_attributes(&duration, &base_attributes());

    assert_token_metrics(&setup, &base_attributes());
    assert_eq!(setup.point(INPUT_TOKENS).bounds, TOKEN_BOUNDARIES);
    assert!(setup.points(TIME_TO_FIRST_CHUNK).is_empty());
}

#[tokio::test]
async fn converse_error_records_error_type_and_zero_tokens() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .then_error(|| ConverseError::ValidationException(validation_error()));
    let client = setup.client(&rule);

    client.converse().model_id(MODEL).send().await.unwrap_err();

    let failed = with(
        base_attributes(),
        KeyValue::new("error.type", "ValidationException"),
    );
    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(&duration, &failed);

    for name in [INPUT_TOKENS, OUTPUT_TOKENS] {
        let point = setup.point(name);
        assert_eq!((point.count, point.sum), (1, 0.0), "{name}");
        assert_attributes(&point, &failed);
    }
    for name in USAGE_COUNTERS {
        assert!(setup.points(name).is_empty(), "{name}");
    }
}

#[tokio::test]
async fn converse_stream_records_metrics_when_read() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    assert!(
        setup.points(DURATION).is_empty(),
        "the stream records the duration"
    );
    while output.stream.recv().await.unwrap().is_some() {}

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(&duration, &base_attributes());

    let first_chunk = setup.point(TIME_TO_FIRST_CHUNK);
    assert_eq!(first_chunk.count, 1);
    assert!(first_chunk.sum <= duration.sum);
    assert_eq!(first_chunk.bounds, DURATION_BOUNDARIES);
    assert_attributes(&first_chunk, &base_attributes());

    assert_token_metrics(&setup, &base_attributes());
}

#[tokio::test]
async fn converse_stream_records_once_when_dropped() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    drop(output);

    assert_eq!(setup.point(DURATION).count, 1);
    assert_attributes(&setup.point(DURATION), &base_attributes());
    assert_eq!(setup.point(INPUT_TOKENS).sum, 15.0);
}

#[tokio::test]
async fn converse_stream_exception_records_error_type() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        exception("modelStreamErrorException"),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    output.stream.recv().await.unwrap_err();
    drop(output);

    let failed = with(
        base_attributes(),
        KeyValue::new("error.type", "ModelStreamErrorException"),
    );
    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(&duration, &failed);
    assert_eq!(setup.point(INPUT_TOKENS).sum, 0.0);
    assert!(setup.points(TIME_TO_FIRST_CHUNK).is_empty());
    for name in USAGE_COUNTERS {
        assert!(setup.points(name).is_empty(), "{name}");
    }
}

#[tokio::test]
async fn converse_stream_error_before_opening_records_error_type() {
    let setup = Setup::new();
    let rule = mock!(Client::converse_stream)
        .then_error(|| ConverseStreamError::ValidationException(validation_error()));
    let client = setup.client(&rule);

    client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap_err();

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(
        &duration,
        &with(
            base_attributes(),
            KeyValue::new("error.type", "ValidationException"),
        ),
    );
    assert!(setup.points(TIME_TO_FIRST_CHUNK).is_empty());
}

#[tokio::test]
async fn retried_calls_are_recorded_once() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .sequence()
        .http_status(503, None)
        .times(1)
        .output(converse_output)
        .build();
    let client = setup.client(&rule);

    client.converse().model_id(MODEL).send().await.unwrap();

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(&duration, &base_attributes());
    assert_eq!(rule.num_calls(), 2);
}

#[tokio::test]
async fn unmodeled_http_errors_use_the_status_code() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .sequence()
        .http_status(400, None)
        .build();
    let client = setup.client(&rule);

    client.converse().model_id(MODEL).send().await.unwrap_err();

    assert_attributes(
        &setup.point(DURATION),
        &with(base_attributes(), KeyValue::new("error.type", "400")),
    );
}

#[tokio::test]
async fn stream_failing_after_deserialization_is_recorded_once() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client_failing_after_deserialization(&rule);

    client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap_err();

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(
        &duration,
        &with(base_attributes(), KeyValue::new("error.type", "_OTHER")),
    );
}

#[tokio::test]
async fn converse_stream_records_time_per_output_chunk() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"lo"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"!"}}"#,
        ),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert_eq!(setup.point(TIME_TO_FIRST_CHUNK).count, 1);
    let per_chunk = setup.point(TIME_PER_OUTPUT_CHUNK);
    assert_eq!(
        per_chunk.count, 2,
        "recorded for each chunk after the first"
    );
    assert_eq!(per_chunk.bounds, DURATION_BOUNDARIES);
    assert_attributes(&per_chunk, &base_attributes());
}

#[tokio::test]
async fn single_chunk_streams_have_no_time_per_output_chunk() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert!(setup.points(TIME_PER_OUTPUT_CHUNK).is_empty());
}

#[tokio::test]
async fn stream_dropped_before_its_final_event_is_cancelled() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"Hel"}}"#,
        ),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    drop(output);

    let cancelled = with(base_attributes(), KeyValue::new("error.type", "cancelled"));
    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(&duration, &cancelled);
    assert_eq!(setup.point(INPUT_TOKENS).sum, 0.0);
}

#[tokio::test]
async fn two_interceptors_record_each_call_once() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"a"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"b"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"c"}}"#,
        ),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ]);
    let tracer_provider = &setup.tracer_provider;
    let meter_provider = &setup.meter_provider;
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(
                BedrockInterceptor::with_tracer_provider(tracer_provider)
                    .with_meter_provider(meter_provider)
            )
            .interceptor(
                BedrockInterceptor::with_tracer_provider(tracer_provider)
                    .with_meter_provider(meter_provider)
            )
    );

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert_chunks_recorded_once(&setup);
}

#[tokio::test]
async fn stream_ending_before_message_stop_is_an_error() {
    let setup = Setup::new();
    let rule = stream_rule(vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"a"}}"#,
        ),
    ]);
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert_attributes(
        &setup.point(DURATION),
        &with(base_attributes(), KeyValue::new("error.type", "_OTHER")),
    );
}

#[tokio::test]
async fn malformed_stream_frames_are_an_error() {
    let setup = Setup::new();
    // A 16-byte frame with wrong checksums.
    let malformed = [0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let rule = stream_rule_with_trailer(
        vec![event("messageStart", r#"{"role":"assistant"}"#)],
        &malformed,
    );
    let client = setup.client(&rule);

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    output.stream.recv().await.unwrap();
    output.stream.recv().await.unwrap_err();
    drop(output);

    let duration = setup.point(DURATION);
    assert_eq!(duration.count, 1);
    assert_attributes(
        &duration,
        &with(base_attributes(), KeyValue::new("error.type", "_OTHER")),
    );
}

fn three_chunk_stream() -> Vec<aws_smithy_types::event_stream::Message> {
    vec![
        event("messageStart", r#"{"role":"assistant"}"#),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"a"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"b"}}"#,
        ),
        event(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"text":"c"}}"#,
        ),
        event("messageStop", r#"{"stopReason":"end_turn"}"#),
    ]
}

fn assert_chunks_recorded_once(setup: &Setup) {
    assert_eq!(setup.point(DURATION).count, 1);
    assert_eq!(setup.point(TIME_TO_FIRST_CHUNK).count, 1);
    assert_eq!(setup.point(TIME_PER_OUTPUT_CHUNK).count, 2);
}

#[tokio::test]
async fn the_same_interceptor_registered_twice_records_once() {
    let setup = Setup::new();
    let rule = stream_rule(three_chunk_stream());
    let shared = SharedInterceptor::new(
        BedrockInterceptor::with_tracer_provider(&setup.tracer_provider)
            .with_meter_provider(&setup.meter_provider),
    );
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config
            .interceptor(shared.clone())
            .interceptor(shared.clone())
    );

    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert_chunks_recorded_once(&setup);
}

#[tokio::test]
async fn config_override_with_the_client_config_records_once() {
    let setup = Setup::new();
    // The override also copies the mock interceptor, so the mock must answer twice.
    let rule = repeated_stream_rule(three_chunk_stream());
    let client = setup.client(&rule);

    // `to_builder` copies the client's interceptors into the override.
    let mut output = client
        .converse_stream()
        .model_id(MODEL)
        .customize()
        .config_override(client.config().to_builder())
        .send()
        .await
        .unwrap();
    while output.stream.recv().await.unwrap().is_some() {}

    assert_chunks_recorded_once(&setup);
}
