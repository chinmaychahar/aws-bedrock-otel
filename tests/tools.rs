mod common;

use aws_bedrock_otel::BedrockInterceptor;
use aws_sdk_bedrockruntime::types::{
    ToolResultBlock, ToolResultContentBlock, ToolResultStatus, ToolUseBlock,
};
use aws_smithy_types::Document;
use opentelemetry::trace::{
    Span as _, SpanKind, Status, TraceContextExt, Tracer, TracerProvider as _,
};
use opentelemetry::{Context, KeyValue, Value};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::{Value as Json, json};

use common::{assert_matches_schema, metric_points};

const DURATION: &str = "gen_ai.execute_tool.duration";

struct Setup {
    tracer_provider: SdkTracerProvider,
    spans: InMemorySpanExporter,
    meter_provider: SdkMeterProvider,
    metrics: InMemoryMetricExporter,
}

impl Setup {
    fn new() -> Self {
        let spans = InMemorySpanExporter::default();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let metrics = InMemoryMetricExporter::default();
        let meter_provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        Self {
            tracer_provider,
            spans,
            meter_provider,
            metrics,
        }
    }

    fn interceptor(&self) -> BedrockInterceptor {
        BedrockInterceptor::new()
            .tracer_provider(&self.tracer_provider)
            .meter_provider(&self.meter_provider)
    }

    fn spans(&self) -> Vec<SpanData> {
        self.spans.get_finished_spans().unwrap()
    }

    fn only_span(&self) -> SpanData {
        let mut spans = self.spans();
        assert_eq!(spans.len(), 1, "expected exactly one span");
        spans.remove(0)
    }

    fn durations(&self) -> Vec<common::Point> {
        metric_points(&self.meter_provider, &self.metrics, DURATION)
    }
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn json_attribute(span: &SpanData, key: &str) -> Json {
    match attribute(span, key) {
        Some(Value::String(value)) => serde_json::from_str(value.as_str()).unwrap(),
        other => panic!("{key} is not a string: {other:?}"),
    }
}

fn sorted(mut attributes: Vec<KeyValue>) -> Vec<KeyValue> {
    attributes.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    attributes
}

fn weather_call() -> ToolUseBlock {
    ToolUseBlock::builder()
        .tool_use_id("tool-1")
        .name("get_weather")
        .input(Document::Object(
            [("city".to_owned(), Document::String("Paris".to_owned()))].into(),
        ))
        .build()
        .unwrap()
}

fn result(content: Vec<ToolResultContentBlock>) -> ToolResultBlock {
    ToolResultBlock::builder()
        .tool_use_id("tool-1")
        .set_content(Some(content))
        .build()
        .unwrap()
}

fn json_result() -> ToolResultBlock {
    result(vec![ToolResultContentBlock::Json(Document::Object(
        [("celsius".to_owned(), Document::from(18u64))].into(),
    ))])
}

#[test]
fn records_tool_use_span() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();
    let tool_use = weather_call();

    let parent = setup.tracer_provider.tracer("app").start("agent");
    let parent_context = Context::current_with_span(parent);
    let guard = parent_context.clone().attach();
    tools
        .tool_use(&tool_use)
        .start()
        .end_with_result(&json_result());
    drop(guard);
    parent_context.span().end();

    let spans = setup.spans();
    assert_eq!(spans.len(), 2);
    let span = &spans[0];
    assert_eq!(span.name, "execute_tool get_weather");
    assert_eq!(span.span_kind, SpanKind::Internal);
    assert_eq!(span.status, Status::Unset);
    assert_eq!(span.parent_span_id, spans[1].span_context.span_id());
    assert_eq!(
        sorted(span.attributes.clone()),
        sorted(vec![
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.name", "get_weather"),
            KeyValue::new("gen_ai.tool.type", "function"),
            KeyValue::new("gen_ai.tool.call.id", "tool-1"),
        ])
    );
}

#[test]
fn records_the_attributes_that_are_set() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    tools
        .tool("search_docs")
        .call_id("call-7")
        .tool_type("datastore")
        .description("Searches the docs")
        .agent_name("Helper")
        .conversation_id("conversation-1")
        .start()
        .end();

    let span = setup.only_span();
    assert_eq!(span.name, "execute_tool search_docs");
    assert_eq!(
        sorted(span.attributes.clone()),
        sorted(vec![
            KeyValue::new("gen_ai.operation.name", "execute_tool"),
            KeyValue::new("gen_ai.tool.name", "search_docs"),
            KeyValue::new("gen_ai.tool.type", "datastore"),
            KeyValue::new("gen_ai.tool.call.id", "call-7"),
            KeyValue::new("gen_ai.tool.description", "Searches the docs"),
            KeyValue::new("gen_ai.agent.name", "Helper"),
            KeyValue::new("gen_ai.conversation.id", "conversation-1"),
        ])
    );
}

#[test]
fn records_duration_metric() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    tools
        .tool("search_docs")
        .tool_type("datastore")
        .agent_name("Helper")
        .call_id("call-7")
        .conversation_id("conversation-1")
        .start()
        .end();
    tools.tool_use(&weather_call()).start().fail("timeout");

    let mut points = setup.durations();
    points.sort_by_key(|point| attribute_value(&point.attributes, "gen_ai.tool.name"));
    assert_eq!(points.len(), 2);
    assert_eq!(
        sorted(points[0].attributes.clone()),
        sorted(vec![
            KeyValue::new("gen_ai.tool.name", "get_weather"),
            KeyValue::new("gen_ai.tool.type", "function"),
            KeyValue::new("error.type", "timeout"),
        ])
    );
    assert_eq!(
        sorted(points[1].attributes.clone()),
        sorted(vec![
            KeyValue::new("gen_ai.tool.name", "search_docs"),
            KeyValue::new("gen_ai.tool.type", "datastore"),
            KeyValue::new("gen_ai.agent.name", "Helper"),
        ])
    );
    for point in &points {
        assert_eq!(point.count, 1);
        assert_eq!(
            point.bounds,
            [
                0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96,
                81.92
            ]
        );
    }
}

fn attribute_value(attributes: &[KeyValue], key: &str) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
        .unwrap_or_default()
}

#[test]
fn metric_duration_matches_the_span() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    let call = tools.tool("sleep").start();
    std::thread::sleep(std::time::Duration::from_millis(20));
    call.end();

    let span = setup.only_span();
    let span_seconds = span
        .end_time
        .duration_since(span.start_time)
        .unwrap()
        .as_secs_f64();
    let points = setup.durations();
    assert!(points[0].sum >= 0.02, "{}", points[0].sum);
    // Equal, within the 100 ns resolution of `SystemTime` on Windows.
    assert!((points[0].sum - span_seconds).abs() < 1e-6);
}

#[test]
fn failures_record_the_error_type() {
    let setup = Setup::new();
    let tools = setup.interceptor().content_capture(true).tool_tracer();

    tools.tool_use(&weather_call()).start().fail("timeout");

    let span = setup.only_span();
    assert_eq!(attribute(&span, "error.type"), Some(Value::from("timeout")));
    assert_eq!(span.status, Status::error("timeout"));
    assert_eq!(attribute(&span, "gen_ai.tool.call.result"), None);
}

#[test]
fn error_results_are_failures_without_a_result() {
    let setup = Setup::new();
    let tools = setup.interceptor().content_capture(true).tool_tracer();
    let error = ToolResultBlock::builder()
        .tool_use_id("tool-1")
        .content(ToolResultContentBlock::Text("city not found".into()))
        .status(ToolResultStatus::Error)
        .build()
        .unwrap();

    tools
        .tool_use(&weather_call())
        .start()
        .end_with_result(&error);

    let span = setup.only_span();
    assert_eq!(attribute(&span, "error.type"), Some(Value::from("_OTHER")));
    assert_eq!(span.status, Status::error("_OTHER"));
    assert_eq!(attribute(&span, "gen_ai.tool.call.result"), None);
    let points = setup.durations();
    assert_eq!(
        attribute_value(&points[0].attributes, "error.type"),
        "_OTHER"
    );
}

#[test]
fn dropped_calls_are_cancelled() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    drop(tools.tool_use(&weather_call()).start());

    let span = setup.only_span();
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("cancelled"))
    );
    assert_eq!(span.status, Status::error("cancelled"));
    let points = setup.durations();
    assert_eq!(points.len(), 1);
    assert_eq!(
        attribute_value(&points[0].attributes, "error.type"),
        "cancelled"
    );
}

#[test]
fn content_is_not_captured_by_default() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    tools
        .tool_use(&weather_call())
        .start()
        .end_with_result(&json_result());

    let span = setup.only_span();
    assert_eq!(attribute(&span, "gen_ai.tool.call.arguments"), None);
    assert_eq!(attribute(&span, "gen_ai.tool.call.result"), None);
}

#[test]
fn captures_arguments_and_result() {
    let setup = Setup::new();
    let tools = setup.interceptor().content_capture(true).tool_tracer();

    tools
        .tool_use(&weather_call())
        .start()
        .end_with_result(&json_result());

    let span = setup.only_span();
    let arguments = json_attribute(&span, "gen_ai.tool.call.arguments");
    let result = json_attribute(&span, "gen_ai.tool.call.result");
    assert_matches_schema("gen_ai.tool.call.arguments", &arguments);
    assert_matches_schema("gen_ai.tool.call.result", &result);
    assert_eq!(arguments, json!({ "city": "Paris" }));
    assert_eq!(result, json!({ "celsius": 18 }));
}

/// The recorded `gen_ai.tool.call.result` for a successful call with `content`.
fn recorded_result(content: Vec<ToolResultContentBlock>) -> Json {
    let setup = Setup::new();
    let tools = setup.interceptor().content_capture(true).tool_tracer();
    tools.tool("tool").start().end_with_result(&result(content));
    json_attribute(&setup.only_span(), "gen_ai.tool.call.result")
}

#[test]
fn text_results_holding_an_object_are_recorded_as_the_object() {
    assert_eq!(
        recorded_result(vec![ToolResultContentBlock::Text(
            r#"{"celsius": 18}"#.into()
        )]),
        json!({ "celsius": 18 })
    );
    assert_eq!(
        recorded_result(vec![ToolResultContentBlock::Text("18 degrees".into())]),
        json!("18 degrees")
    );
    assert_eq!(
        recorded_result(vec![ToolResultContentBlock::Text("[1, 2]".into())]),
        json!("[1, 2]")
    );
    assert_eq!(
        recorded_result(vec![
            ToolResultContentBlock::Text("18 degrees".into()),
            ToolResultContentBlock::Json(Document::Bool(true)),
        ]),
        json!(["18 degrees", true])
    );
}

#[test]
fn long_arguments_and_results_are_cut() {
    let setup = Setup::new();
    let tools = setup
        .interceptor()
        .content_capture(true)
        .max_content_length(10)
        .tool_tracer();

    tools
        .tool_use(&weather_call())
        .start()
        .end_with_result(&result(vec![ToolResultContentBlock::Text(
            r#"{"forecast": "sunny all week"}"#.into(),
        )]));

    let span = setup.only_span();
    assert_eq!(
        json_attribute(&span, "gen_ai.tool.call.arguments"),
        json!(r#"{"city":"P"#)
    );
    assert_eq!(
        json_attribute(&span, "gen_ai.tool.call.result"),
        json!(r#"{"forecast"#)
    );
}

#[test]
fn spans_inside_the_call_are_its_children() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    let call = tools.tool("lookup").start();
    {
        let _guard = call.attach();
        setup.tracer_provider.tracer("app").start("http get").end();
    }
    call.end();

    let spans = setup.spans();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].name, "http get");
    assert_eq!(spans[0].parent_span_id, spans[1].span_context.span_id());
}

#[test]
fn spans_off_records_metrics_only() {
    let setup = Setup::new();
    let tools = setup
        .interceptor()
        .spans(false)
        .content_capture(true)
        .tool_tracer();

    let parent = setup.tracer_provider.tracer("app").start("agent");
    let parent_context = Context::current_with_span(parent);
    let guard = parent_context.clone().attach();
    let call = tools.tool_use(&weather_call()).start();
    // The caller's span is left as it is.
    assert_eq!(
        call.context().span().span_context(),
        parent_context.span().span_context()
    );
    call.fail("timeout");
    drop(guard);
    parent_context.span().end();

    let span = setup.only_span();
    assert_eq!(span.name, "agent");
    assert_eq!(span.attributes, Vec::new());
    assert_eq!(span.status, Status::Unset);
    assert_eq!(setup.durations().len(), 1);
}

#[test]
fn metrics_off_records_spans_only() {
    let setup = Setup::new();
    let tools = setup.interceptor().metrics(false).tool_tracer();

    tools.tool_use(&weather_call()).start().end();

    assert_eq!(setup.spans().len(), 1);
    assert!(setup.durations().is_empty());
}

#[test]
fn tool_tracer_and_calls_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<aws_bedrock_otel::ToolTracer>();
    assert_send_sync::<aws_bedrock_otel::ToolCall>();
}

#[tokio::test]
async fn futures_run_in_the_call_are_its_children() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();
    let tracer = setup.tracer_provider.tracer("app");

    // A call can be held across `.await` in a spawned task.
    let task = tokio::spawn(async move {
        let call = tools.tool("lookup").start();
        call.in_context(async move {
            tokio::task::yield_now().await;
            tracer.start("http get").end();
        })
        .await;
        call.end();
    });
    task.await.unwrap();

    let spans = setup.spans();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].name, "http get");
    assert_eq!(spans[0].parent_span_id, spans[1].span_context.span_id());
}

#[test]
fn panics_are_recorded_as_other() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _call = tools.tool("lookup").start();
        panic!("tool failed");
    }));

    assert!(panicked.is_err());
    let span = setup.only_span();
    assert_eq!(attribute(&span, "error.type"), Some(Value::from("_OTHER")));
    let points = setup.durations();
    assert_eq!(
        attribute_value(&points[0].attributes, "error.type"),
        "_OTHER"
    );
}

#[test]
fn empty_results_are_not_recorded() {
    let setup = Setup::new();
    let tools = setup.interceptor().content_capture(true).tool_tracer();

    tools
        .tool("lookup")
        .start()
        .end_with_result(&result(Vec::new()));
    tools
        .tool("lookup")
        .start()
        .end_with_result(&result(vec![ToolResultContentBlock::Text(String::new())]));

    let spans = setup.spans();
    assert_eq!(spans.len(), 2);
    for span in &spans {
        assert_eq!(attribute(span, "gen_ai.tool.call.result"), None);
        assert_eq!(attribute(span, "error.type"), None);
    }
}

#[test]
fn tools_without_a_name_have_a_plain_span_name() {
    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();

    tools.tool("").start().end();

    assert_eq!(setup.only_span().name, "execute_tool");
}

#[test]
fn calls_dropped_while_another_panic_unwinds_are_cancelled() {
    struct DropsACall(aws_bedrock_otel::ToolTracer);
    impl Drop for DropsACall {
        fn drop(&mut self) {
            drop(self.0.tool("cleanup").start());
        }
    }

    let setup = Setup::new();
    let tools = setup.interceptor().tool_tracer();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _cleanup = DropsACall(tools);
        panic!("unrelated");
    }));

    assert!(panicked.is_err());
    let span = setup.only_span();
    assert_eq!(
        attribute(&span, "error.type"),
        Some(Value::from("cancelled"))
    );
    let points = setup.durations();
    assert_eq!(
        attribute_value(&points[0].attributes, "error.type"),
        "cancelled"
    );
}

#[test]
fn results_cut_to_nothing_are_still_recorded() {
    let setup = Setup::new();
    let tools = setup
        .interceptor()
        .content_capture(true)
        .max_content_length(0)
        .tool_tracer();

    tools
        .tool("lookup")
        .start()
        .end_with_result(&result(vec![ToolResultContentBlock::Text("18".into())]));

    assert_eq!(
        json_attribute(&setup.only_span(), "gen_ai.tool.call.result"),
        json!("")
    );
}
