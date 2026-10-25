mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::operation::converse::ConverseError;
use aws_sdk_bedrockruntime::types::error::ValidationException;
use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
use opentelemetry::Context;
use opentelemetry::trace::{FutureExt, Status, TraceContextExt, Tracer, TracerProvider as _};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

use common::{converse_output, full_stream, metric_points, stream_rule};

const DURATION: &str = "gen_ai.client.inference.duration";

struct Setup {
    tracer_provider: SdkTracerProvider,
    span_exporter: InMemorySpanExporter,
    meter_provider: SdkMeterProvider,
    metric_exporter: InMemoryMetricExporter,
}

impl Setup {
    fn new() -> Self {
        let span_exporter = InMemorySpanExporter::default();
        let metric_exporter = InMemoryMetricExporter::default();
        Self {
            tracer_provider: SdkTracerProvider::builder()
                .with_simple_exporter(span_exporter.clone())
                .build(),
            span_exporter,
            meter_provider: SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
                .build(),
            metric_exporter,
        }
    }

    fn client(
        &self,
        rule: &Rule,
        configure: fn(BedrockInterceptor) -> BedrockInterceptor,
    ) -> Client {
        let tracer_provider = &self.tracer_provider;
        let meter_provider = &self.meter_provider;
        mock_client!(
            aws_sdk_bedrockruntime,
            RuleMode::Sequential,
            &[rule],
            move |config| config.interceptor(configure(
                BedrockInterceptor::with_tracer_provider(tracer_provider)
                    .with_meter_provider(meter_provider)
            ))
        )
    }

    fn span_names(&self) -> Vec<String> {
        self.span_exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .map(|span| span.name.into_owned())
            .collect()
    }

    fn duration_count(&self) -> u64 {
        metric_points(&self.meter_provider, &self.metric_exporter, DURATION)
            .iter()
            .map(|point| point.count)
            .sum()
    }
}

#[tokio::test]
async fn without_spans_records_metrics_and_leaves_the_current_span_alone() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule, BedrockInterceptor::without_spans);

    let parent = setup.tracer_provider.tracer("app").start("request");
    let parent_context = Context::current_with_span(parent);
    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .with_context(parent_context.clone())
        .await
        .unwrap();

    assert!(setup.span_names().is_empty(), "no span is created or ended");
    assert_eq!(setup.duration_count(), 1);

    parent_context.span().end();
    let spans = setup.span_exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "request");
    assert!(
        spans[0].attributes.is_empty(),
        "the caller's span gets no attributes"
    );
}

#[tokio::test]
async fn without_spans_records_metrics_for_streams() {
    let setup = Setup::new();
    let rule = stream_rule(full_stream());
    let client = setup.client(&rule, BedrockInterceptor::without_spans);

    let parent = setup.tracer_provider.tracer("app").start("request");
    let parent_context = Context::current_with_span(parent);
    async {
        let mut output = client
            .converse_stream()
            .model_id("amazon.nova-lite")
            .send()
            .await
            .unwrap();
        while output.stream.recv().await.unwrap().is_some() {}
    }
    .with_context(parent_context.clone())
    .await;

    assert!(setup.span_names().is_empty(), "no span is created or ended");
    assert_eq!(setup.duration_count(), 1);
    parent_context.span().end();
    let spans = setup.span_exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert!(spans[0].attributes.is_empty());
}

#[tokio::test]
async fn without_metrics_records_spans_only() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule, BedrockInterceptor::without_metrics);

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    assert_eq!(setup.span_names(), ["chat amazon.nova-lite"]);
    assert_eq!(setup.duration_count(), 0);
}

#[tokio::test]
async fn without_spans_leaves_the_current_span_alone_on_errors() {
    let setup = Setup::new();
    let rule = mock!(Client::converse)
        .then_error(|| ConverseError::ValidationException(ValidationException::builder().build()));
    let client = setup.client(&rule, BedrockInterceptor::without_spans);

    let parent = setup.tracer_provider.tracer("app").start("request");
    let parent_context = Context::current_with_span(parent);
    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .with_context(parent_context.clone())
        .await
        .unwrap_err();

    assert!(setup.span_names().is_empty());
    parent_context.span().end();
    let spans = setup.span_exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    assert!(spans[0].attributes.is_empty());
    assert_eq!(spans[0].status, Status::Unset);
}

#[tokio::test]
async fn without_spans_and_metrics_records_nothing() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let client = setup.client(&rule, |interceptor| {
        interceptor.without_spans().without_metrics()
    });

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    assert!(setup.span_names().is_empty());
    assert_eq!(setup.duration_count(), 0);
}

#[tokio::test]
async fn without_metrics_takes_precedence_over_a_meter_provider() {
    let setup = Setup::new();
    let rule = mock!(Client::converse).then_output(converse_output);
    let meter_provider = setup.meter_provider.clone();
    let tracer_provider = setup.tracer_provider.clone();
    let client = mock_client!(
        aws_sdk_bedrockruntime,
        RuleMode::Sequential,
        &[&rule],
        move |config| config.interceptor(
            BedrockInterceptor::with_tracer_provider(&tracer_provider)
                .without_metrics()
                .with_meter_provider(&meter_provider)
        )
    );

    client
        .converse()
        .model_id("amazon.nova-lite")
        .send()
        .await
        .unwrap();

    assert_eq!(setup.duration_count(), 0);
}
