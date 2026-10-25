//! Calls dropped before they finish, for example by a timeout, are recorded as cancelled.

mod common;

use std::time::Duration;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::config::retry::RetryConfig;
use aws_sdk_bedrockruntime::config::timeout::TimeoutConfig;
use aws_sdk_bedrockruntime::config::{Builder, RuntimeComponents};
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use opentelemetry::KeyValue;
use opentelemetry::trace::Status;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

use common::metric_points;

/// An HTTP client whose requests never complete.
#[derive(Debug)]
struct NeverResponds;

impl HttpConnector for NeverResponds {
    fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::new(std::future::pending())
    }
}

impl HttpClient for NeverResponds {
    fn http_connector(
        &self,
        _settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(NeverResponds)
    }
}

struct Setup {
    _tracer_provider: SdkTracerProvider,
    span_exporter: InMemorySpanExporter,
    meter_provider: SdkMeterProvider,
    metric_exporter: InMemoryMetricExporter,
    client: Client,
}

impl Setup {
    fn new() -> Self {
        Self::with_config(|config| config)
    }

    fn with_config(configure: fn(Builder) -> Builder) -> Self {
        let span_exporter = InMemorySpanExporter::default();
        let metric_exporter = InMemoryMetricExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(span_exporter.clone())
            .build();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metric_exporter.clone()).build())
            .build();
        let config = configure(
            aws_sdk_bedrockruntime::Config::builder()
                .with_test_defaults_v2()
                .http_client(NeverResponds)
                .interceptor(
                    BedrockInterceptor::with_tracer_provider(&tracer_provider)
                        .with_meter_provider(&meter_provider),
                ),
        )
        .build();
        Self {
            _tracer_provider: tracer_provider,
            span_exporter,
            meter_provider,
            metric_exporter,
            client: Client::from_conf(config),
        }
    }

    fn assert_cancelled(&self) {
        self.assert_error_type("cancelled");
    }

    fn assert_error_type(&self, error_type: &'static str) {
        let spans = self.span_exporter.get_finished_spans().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(
            spans[0]
                .attributes
                .contains(&KeyValue::new("error.type", error_type))
        );
        assert_eq!(spans[0].status, Status::error(error_type));

        let duration = metric_points(
            &self.meter_provider,
            &self.metric_exporter,
            "gen_ai.client.inference.duration",
        );
        assert_eq!(duration.len(), 1);
        assert_eq!(duration[0].count, 1);
        assert!(
            duration[0]
                .attributes
                .contains(&KeyValue::new("error.type", error_type))
        );
        let input_tokens = metric_points(
            &self.meter_provider,
            &self.metric_exporter,
            "gen_ai.client.inference.operation.input_tokens",
        );
        assert_eq!((input_tokens[0].count, input_tokens[0].sum), (1, 0.0));
    }
}

#[tokio::test]
async fn converse_dropped_by_a_timeout_is_cancelled() {
    let setup = Setup::new();
    let call = setup.client.converse().model_id("m").send();
    tokio::time::timeout(Duration::from_millis(50), call)
        .await
        .unwrap_err();
    setup.assert_cancelled();
}

#[tokio::test]
async fn converse_stream_dropped_before_a_response_is_cancelled() {
    let setup = Setup::new();
    let call = setup.client.converse_stream().model_id("m").send();
    tokio::time::timeout(Duration::from_millis(50), call)
        .await
        .unwrap_err();
    setup.assert_cancelled();
}

#[tokio::test]
async fn attempt_timeouts_are_recorded_as_timeout() {
    let setup = Setup::with_config(|config| {
        config
            .timeout_config(
                TimeoutConfig::builder()
                    .operation_attempt_timeout(Duration::from_millis(50))
                    .build(),
            )
            .retry_config(RetryConfig::disabled())
    });

    setup
        .client
        .converse()
        .model_id("m")
        .send()
        .await
        .unwrap_err();
    setup.assert_error_type("timeout");
}

#[tokio::test]
async fn operation_timeouts_are_recorded_as_timeout() {
    let setup = Setup::with_config(|config| {
        config.timeout_config(
            TimeoutConfig::builder()
                .operation_timeout(Duration::from_millis(50))
                .build(),
        )
    });

    setup
        .client
        .converse()
        .model_id("m")
        .send()
        .await
        .unwrap_err();
    setup.assert_error_type("timeout");
}

/// An HTTP client whose connections time out at once.
#[derive(Debug)]
struct ConnectTimeout;

impl HttpConnector for ConnectTimeout {
    fn call(&self, _request: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::ready(Err(ConnectorError::timeout("connect timed out".into())))
    }
}

impl HttpClient for ConnectTimeout {
    fn http_connector(
        &self,
        _settings: &HttpConnectorSettings,
        _components: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(ConnectTimeout)
    }
}

#[tokio::test]
async fn connection_timeouts_are_recorded_as_timeout() {
    let setup = Setup::with_config(|config| {
        config
            .http_client(ConnectTimeout)
            .retry_config(RetryConfig::disabled())
    });

    setup
        .client
        .converse()
        .model_id("m")
        .send()
        .await
        .unwrap_err();
    setup.assert_error_type("timeout");
}
