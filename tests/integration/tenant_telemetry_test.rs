use std::sync::{Arc, Mutex};

use openai_server::telemetry::{
    sampling::{
        SamplingGate, SamplingGateSpanProcessor, TenantAttributionSpanProcessor, TenantSampling,
    },
    tenant_export::TenantResourceExporter,
    TelemetryRuntime, ENVIRONMENT_ID_ATTRIBUTE,
};
use opentelemetry::{
    trace::{Span as _, Tracer as _, TracerProvider as _},
    Context,
};
use opentelemetry_sdk::{
    export::trace::{SpanData, SpanExporter},
    testing::trace::InMemorySpanExporter,
    trace::{SimpleSpanProcessor, TracerProvider},
    Resource,
};

fn spans() -> Vec<SpanData> {
    let exporter = InMemorySpanExporter::default();
    let gate = Arc::new(SamplingGate::new());
    let provider = TracerProvider::builder()
        .with_span_processor(TenantAttributionSpanProcessor)
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(exporter.clone())),
            gate.clone(),
        ))
        .build();
    let tracer = provider.tracer("tenants");
    for environment in ["a", "b"] {
        let context = Context::new().with_value(TenantSampling {
            environment_id: environment.into(),
            gate: gate.clone(),
        });
        let mut span = tracer.build_with_context(
            tracer
                .span_builder(environment)
                .with_attributes([opentelemetry::KeyValue::new(
                    "openinference.span.kind",
                    "LLM",
                )]),
            &context,
        );
        span.end();
    }
    exporter.get_finished_spans().unwrap()
}

#[derive(Clone, Default)]
struct Collector(
    Arc<Mutex<Vec<opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest>>>,
    Arc<std::sync::atomic::AtomicBool>,
);

#[tonic::async_trait]
impl opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceService
    for Collector
{
    async fn export(
        &self,
        request: tonic::Request<
            opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest,
        >,
    ) -> Result<
        tonic::Response<
            opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceResponse,
        >,
        tonic::Status,
    > {
        let request = request.into_inner();
        let is_b = request.resource_spans.iter().any(|resource| {
            resource
                .scope_spans
                .iter()
                .any(|scope| scope.spans.iter().any(|span| span.name == "b"))
        });
        self.0.lock().unwrap().push(request);
        if is_b && self.1.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(tonic::Status::unavailable("retry only tenant b"));
        }
        Ok(tonic::Response::new(Default::default()))
    }
}

#[tokio::test]
async fn actual_otlp_payloads_group_a_mixed_batch_by_environment_resource() {
    check_otlp_groups(false).await;
}

#[tokio::test]
async fn partial_otlp_failure_retries_only_the_failed_tenant_with_its_original_resource() {
    check_otlp_groups(true).await;
}

async fn check_otlp_groups(fail_b_once: bool) {
    use futures::stream;
    use opentelemetry_otlp::WithExportConfig;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let collector = Collector::default();
    collector
        .1
        .store(fail_b_once, std::sync::atomic::Ordering::SeqCst);
    let incoming = stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let task = tokio::spawn(tonic::transport::Server::builder().add_service(opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::TraceServiceServer::new(collector.clone())).serve_with_incoming(incoming));
    let otlp = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(format!("http://{address}"))
        .build()
        .unwrap();
    let mut exporter = TenantResourceExporter::new(otlp);
    exporter.set_resource(&Resource::new([
        opentelemetry::KeyValue::new("service.name", "cluster-router"),
        opentelemetry::KeyValue::new(ENVIRONMENT_ID_ATTRIBUTE, "old-default"),
    ]));
    exporter.export(spans()).await.unwrap();
    let requests = collector.0.lock().unwrap();
    assert_eq!(requests.len(), if fail_b_once { 3 } else { 2 });
    let mut environments = Vec::new();
    for request in requests.iter() {
        assert_eq!(request.resource_spans.len(), 1);
        let resource = &request.resource_spans[0];
        let attribute = resource
            .resource
            .as_ref()
            .unwrap()
            .attributes
            .iter()
            .find(|kv| kv.key == ENVIRONMENT_ID_ATTRIBUTE)
            .unwrap();
        let value = match attribute.value.as_ref().unwrap().value.as_ref().unwrap() {
            opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(value) => value,
            _ => panic!("environment must be a string"),
        };
        assert_eq!(resource.scope_spans[0].spans[0].name, *value);
        environments.push(value.clone());
    }
    environments.sort();
    assert_eq!(
        environments,
        if fail_b_once {
            vec!["a", "b", "b"]
        } else {
            vec!["a", "b"]
        }
    );
    task.abort();
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<(http::HeaderMap, bytes::Bytes)>>>);

async fn capture(
    axum::extract::State(captured): axum::extract::State<Captured>,
    headers: http::HeaderMap,
    body: bytes::Bytes,
) -> http::StatusCode {
    captured.0.lock().unwrap().push((headers, body));
    http::StatusCode::OK
}

#[tokio::test]
async fn braintrust_mixed_batches_and_destination_changes_remain_tenant_bound() {
    use openai_server::telemetry::braintrust::{
        BraintrustConfig, BraintrustSpanExporter, BraintrustSpanProcessor,
    };
    use opentelemetry_sdk::trace::SpanProcessor;
    use prost::Message;
    let captured = Captured::default();
    let endpoint = crate::support::mock::serve(
        axum::Router::new()
            .route("/otel/v1/traces", axum::routing::post(capture))
            .with_state(captured.clone()),
    )
    .await;
    let config = |tenant: &str| BraintrustConfig {
        traces_url: format!("{endpoint}/otel/v1/traces"),
        api_key: format!("key-{tenant}"),
        parent: format!("project_name:{tenant}"),
    };
    let root = TelemetryRuntime::new(Vec::new());
    let a = TelemetryRuntime::new(vec![config("a")]);
    let b = TelemetryRuntime::new(vec![config("b")]);
    root.register_environment("a", &a, None, "test");
    root.register_environment("b", &b, None, "test");
    let queued = InMemorySpanExporter::default();
    let processor = BraintrustSpanProcessor::new(
        SimpleSpanProcessor::new(Box::new(queued.clone())),
        root.braintrust_state(),
    );
    for span in spans() {
        processor.on_end(span);
    }
    let batch = queued.get_finished_spans().unwrap();
    let mut exporter = BraintrustSpanExporter::new(root.braintrust_state(), &Resource::empty());
    exporter.export(batch.clone()).await.unwrap();
    {
        let captures = captured.0.lock().unwrap();
        assert_eq!(captures.len(), 2);
        for (headers, body) in captures.iter() {
            let request = opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest::decode(body.clone()).unwrap();
            let span = &request.resource_spans[0].scope_spans[0].spans[0];
            assert_eq!(
                headers["authorization"],
                format!("Bearer key-{}", span.name)
            );
            assert_eq!(
                headers["x-bt-parent"],
                format!("project_name:{}", span.name)
            );
        }
    }
    captured.0.lock().unwrap().clear();
    a.set_braintrust_configs(vec![config("a-new")]);
    exporter.export(batch).await.unwrap();
    let captures = captured.0.lock().unwrap();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].0["x-bt-parent"], "project_name:b");
}

#[tokio::test]
async fn client_spans_drop_forged_controls_and_follow_content_policy() {
    use axum::{body::Body, routing::post, Router};
    use openai_server::{
        handlers,
        middleware::auth_middleware,
        sampling_policy::{InMemorySamplingPolicyStore, SamplingPolicy},
    };
    use tower::ServiceExt;
    let mut state = crate::tenancy_test::runtime("a").await;
    let gate = Arc::new(SamplingGate::new());
    gate.set_store(Arc::new(InMemorySamplingPolicyStore::new(SamplingPolicy {
        enabled: true,
        default_log_content: false,
        ..Default::default()
    })));
    gate.refresh().await;
    state.sampling_gate = gate.clone();
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_span_processor(TenantAttributionSpanProcessor)
        .with_span_processor(SamplingGateSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(exporter.clone())),
            gate,
        ))
        .build();
    opentelemetry::global::set_tracer_provider(provider.clone());
    let app = Router::new()
        .route("/v1/traces/spans", post(handlers::traces::ingest_spans))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    let mut cases = vec![
        (
            serde_json::json!({"chalk.environment_id":"b"}),
            http::StatusCode::OK,
        ),
        (
            serde_json::json!({"chalk.sampling.force_keep":true}),
            http::StatusCode::OK,
        ),
        (
            serde_json::json!({"input.value":"secret", "output.value":"secret", "message.content":"secret", "tool_call.function.arguments":"secret", "retrieval.documents.0.document.content":"secret", "llm.prompt_template.variables":"secret", "prompt":"secret", "custom":"secret", "llm.model_name":"model", "llm.token_count.total":5}),
            http::StatusCode::OK,
        ),
    ];
    cases.extend(
        [
            "chalk.auth.principal.id",
            "chalk.auth.principal.type",
            "chalk.caller.identity",
            "chalk.frontend",
            "chalk.custom",
            "user.id",
        ]
        .into_iter()
        .map(|key| (serde_json::json!({(key):"forged"}), http::StatusCode::OK)),
    );
    for (attributes, expected) in cases {
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            - 60_000_000_000;
        let body = serde_json::json!({"spans":[{"name":"client", "kind":"llm", "trace_id":"00000000000000000000000000000001", "span_id":"0000000000000001", "start_time_unix_nano":start, "end_time_unix_nano":start + 1, "attributes":attributes}]});
        let response = app
            .clone()
            .oneshot(
                http::Request::builder()
                    .method("POST")
                    .uri("/v1/traces/spans")
                    .header(
                        "authorization",
                        format!("Bearer {}", crate::support::app::TEST_API_KEY),
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    // Reserved keys are dropped and the spans are still ingested.
    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 9);
    for span in &spans {
        let environments: Vec<_> = span
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == ENVIRONMENT_ID_ATTRIBUTE)
            .map(|kv| kv.value.as_str().into_owned())
            .collect();
        assert_eq!(environments, ["a"]);
        assert!(span
            .attributes
            .iter()
            .all(|kv| kv.value.as_str() != "forged"
                && kv.key.as_str() != "chalk.sampling.force_keep"));
        // Router-stamped identity and provenance survive with content logging off.
        let value = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key.as_str() == key)
                .map(|kv| kv.value.as_str().into_owned())
        };
        assert_eq!(value("chalk.span.source").as_deref(), Some("client"));
        assert_eq!(
            value("chalk.auth.principal.type").as_deref(),
            Some("router_api_key")
        );
        assert!(value("chalk.auth.principal.id").is_some());
    }
    let content = spans
        .iter()
        .find(|span| {
            span.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "llm.model_name")
        })
        .unwrap();
    assert!(content.attributes.iter().all(
        |kv| !kv.key.as_str().starts_with("input.") && !kv.key.as_str().starts_with("output.")
    ));
    assert!(content
        .attributes
        .iter()
        .all(|kv| kv.value.as_str() != "secret"));
    assert!(content
        .attributes
        .iter()
        .any(|kv| kv.key.as_str() == "llm.token_count.total"
            && kv.value == opentelemetry::Value::I64(5)));
    provider.shutdown().unwrap();
}
