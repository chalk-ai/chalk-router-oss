//! Literal-wire regressions: parsing fixtures as Value would conceal reordered schemas.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::State,
    http::{header, Request, StatusCode, Uri},
    middleware,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use openai_server::{
    backend::RouterBackend,
    handlers,
    middleware::auth_middleware,
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tower::ServiceExt;

use crate::support::{
    app::{AppStateBuilder, TEST_API_KEY},
    mock::serve,
};

const SCHEMA: &str = r#"{"type":"object","properties":{"zeta_reason":{"type":"string"},"alpha_label":{"type":"string"},"mid_details":{"type":"object","properties":{"yy_last":{},"bb_first":{},"kk_middle":{}}},"beta_items":{"type":"array","items":{"properties":{"q_name":{},"c_score":{}}}}},"$defs":{"nested":{"properties":{"z":{},"a":{}}}}}"#;

type Captured = Arc<Mutex<Vec<(String, String)>>>;

async fn upstream(State(captured): State<Captured>, uri: Uri, body: Bytes) -> Response {
    let wire = String::from_utf8(body.to_vec()).unwrap();
    let request: Value = serde_json::from_str(&wire).unwrap();
    captured.lock().await.push((uri.path().to_string(), wire));
    let model = request["model"].as_str().unwrap();
    match (uri.path(), request["stream"].as_bool() == Some(true)) {
        ("/v1/messages", false) => Json(json!({
            "id":"msg_test","type":"message","model":model,"role":"assistant",
            "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}
        })).into_response(),
        ("/v1/messages", true) => ([(header::CONTENT_TYPE, "text/event-stream")],
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"type\":\"message\",\"model\":\"claude-sonnet-5\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n").into_response(),
        ("/v1/responses/compact", false) => Json(json!({
            "id":"cmp_test","object":"response.compaction","created_at":1,"model":model,
            "output":[{"type":"compaction","id":"cmp_item","encrypted_content":"opaque"}],
            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        })).into_response(),
        ("/v1/responses", false) => Json(json!({
            "id":"resp_test","object":"response","created_at":1,"status":"completed",
            "model":model,"output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
        })).into_response(),
        ("/v1/responses", true) => ([(header::CONTENT_TYPE, "text/event-stream")],
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_test\",\"object\":\"response\",\"created_at\":1,\"status\":\"completed\",\"model\":\"gpt-4o\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n").into_response(),
        (_, false) => Json(json!({
            "id":"chat_test","object":"chat.completion","created":1,"model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })).into_response(),
        (_, true) => ([(header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"id\":\"chat_test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n").into_response(),
    }
}

async fn app(provider: &str) -> (Router, Captured) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let base = serve(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .route("/v1/responses", post(upstream))
            .route("/v1/responses/compact", post(upstream))
            .route("/v1/messages", post(upstream))
            .with_state(captured.clone()),
    )
    .await;
    let connections = Arc::new(InMemoryProviderConnectionStore::new());
    connections
        .create(ConnectionInput {
            name: "schema-test".into(),
            provider_kind: provider.into(),
            base_url: Some(format!("{base}/v1")),
            prefix: Some("schema/".into()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            api_key: "mock-provider-key".into(),
        })
        .await
        .unwrap();
    let backend = Arc::new(RouterBackend::new(
        reqwest::Client::new(),
        connections,
        Arc::new(InMemoryModelRouteStore::new()),
    ));
    let state = AppStateBuilder::new(backend).build().await;
    let router = Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::chat::create_chat_completion),
        )
        .route("/v1/responses", post(handlers::responses::create_response))
        .route(
            "/v1/responses/compact",
            post(handlers::responses::compact_response),
        )
        .route("/v1/messages", post(handlers::anthropic::proxy_messages))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);
    (router, captured)
}

async fn send(app: Router, path: &str, wire: String) {
    let response = app
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .body(Body::from(wire))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

fn assert_order(wire: &str) {
    assert!(wire.contains(SCHEMA), "schema was changed: {wire}");
}

#[tokio::test]
async fn forwarded_chat_schema_keeps_order_and_routes_structured_output() {
    for provider in ["openai", "anthropic"] {
        let model = if provider == "openai" {
            "gpt-4o"
        } else {
            "claude-sonnet-5"
        };
        let (app, captured) = app(provider).await;
        send(app, "/v1/chat/completions", format!(r#"{{"model":"schema/{model}","messages":[{{"role":"user","content":"hi"}}],"response_format":{{{}"type":"json_schema","json_schema":{{"name":"ordered","schema":{{ "type":"object", "properties":{{ "z":{{"type":"string","minimum":1.2300e-4}}, "a":{{}} }} }} }} }} }}"#, " ".repeat(9000))).await;
        let calls = captured.lock().await;
        assert_eq!(calls.len(), 1);
        let wire = &calls[0].1;
        let value: Value = serde_json::from_str(wire).unwrap();
        if provider == "openai" {
            // Native OpenAI bodies are authoritative: bytes, including
            // whitespace, are forwarded as sent, with schema key order intact.
            let z = wire.find(r#""z":{"type":"string","minimum":1.2300e-4}"#);
            let a = wire.find(r#""a":{}"#);
            assert!(z.is_some() && a.is_some() && z < a, "{wire}");
            assert_eq!(value["response_format"]["type"], "json_schema");
            assert_eq!(wire.matches(r#""type":"json_schema""#).count(), 1);
        } else {
            // Translation re-serializes compactly and recognizes the schema, so
            // Anthropic dispatch receives an output schema rather than text mode.
            assert!(wire.len() < 2000, "schema whitespace forwarded: {wire}");
            assert!(
                wire.contains(r#""properties":{"z":{"type":"string","minimum":1.2300e-4},"a":{}}"#),
                "{wire}"
            );
            assert!(
                value["output_config"]["format"]["schema"].is_object(),
                "{wire}"
            );
        }
    }
}

#[tokio::test]
async fn native_chat_rejects_duplicate_keys_in_a_schema_it_would_have_to_reinterpret() {
    let (app, captured) = app("openai").await;
    let wire = r#"{"model":"schema/gpt-4o","messages":[{"role":"user","content":"hi"}],"response_format":{"type":"text","type":"json_schema"}}"#;
    let response = app
        .oneshot(
            Request::post("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                .body(Body::from(wire))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(captured.lock().await.is_empty());
}

#[tokio::test]
async fn native_responses_and_compact_forward_unique_nested_keys_and_unknown_fields() {
    for path in ["/v1/responses", "/v1/responses/compact"] {
        let (app, captured) = app("openai").await;
        send(app, path, format!(r#"{{ "model":"invalid-shadowed-route", "model":"schema/gpt-5-codex", "input":"hi", "future":{{{} "z":false, "a":[{{"key":1,"k\u0065y":null}},1.2300e-4], "z":true, "schema":{{"properties":{{"oneOf":{{}},"type":{{}}}}}} }} }}"#, " ".repeat(9000))).await;
        let calls = captured.lock().await;
        assert_eq!(calls.len(), 1);
        let wire = &calls[0].1;
        assert!(wire.len() < 2000, "{wire}");
        assert!(wire.contains(r#""future":{"a":[{"key":null},1.2300e-4],"z":true,"schema":{"properties":{"oneOf":{},"type":{}}}}"#), "{wire}");
        assert_eq!(wire.matches(r#""model":"#).count(), 1, "{wire}");
        assert!(!wire.contains("invalid-shadowed-route"));
    }
}

#[tokio::test]
async fn chat_retains_response_and_tool_schemas_in_both_modes_and_translates_to_anthropic() {
    for provider in ["openai", "anthropic"] {
        for streaming in [false, true] {
            let model = if provider == "openai" {
                "gpt-4o"
            } else {
                "claude-sonnet-5"
            };
            let (app, captured) = app(provider).await;
            send(app, "/v1/chat/completions", format!(r#"{{"model":"schema/{model}","messages":[{{"role":"user","content":"hi"}}],"stream":{streaming},"response_format":{{"type":"json_schema","json_schema":{{"name":"ordered","schema":{SCHEMA}}}}},"tools":[{{"type":"function","function":{{"name":"lookup","parameters":{SCHEMA}}}}}]}}"#)).await;
            let calls = captured.lock().await;
            assert_eq!(calls.len(), 1);
            assert_order(&calls[0].1);
            assert_eq!(calls[0].1.matches(SCHEMA).count(), 2);
            assert!(calls[0].1.contains(&format!(r#""model":"{model}""#)));
        }
    }
}

#[tokio::test]
async fn responses_retains_text_and_tool_schemas_native_and_translated_in_both_modes() {
    for provider in ["openai", "anthropic"] {
        for streaming in [false, true] {
            let model = if provider == "openai" {
                "gpt-4o"
            } else {
                "claude-sonnet-5"
            };
            let (app, captured) = app(provider).await;
            send(app, "/v1/responses", format!(r#"{{"model":"schema/{model}","input":"hi","stream":{streaming},"text":{{"format":{{"type":"json_schema","name":"ordered","schema":{SCHEMA}}}}},"tools":[{{"type":"function","name":"lookup","parameters":{SCHEMA}}}]}}"#)).await;
            let calls = captured.lock().await;
            assert_eq!(calls.len(), 1);
            assert_order(&calls[0].1);
            assert_eq!(calls[0].1.matches(SCHEMA).count(), 2);
        }
    }
}

#[tokio::test]
async fn native_and_translated_messages_retain_tool_schemas_after_model_rewrite() {
    for provider in ["anthropic", "openai"] {
        for streaming in [false, true] {
            let model = if provider == "openai" {
                "gpt-4o"
            } else {
                "claude-sonnet-5"
            };
            let (app, captured) = app(provider).await;
            send(app, "/v1/messages", format!(r#"{{"model":"schema/{model}","messages":[{{"role":"user","content":"hi"}}],"max_tokens":64,"stream":{streaming},"tools":[{{"name":"lookup","input_schema":{SCHEMA}}}]}}"#)).await;
            let calls = captured.lock().await;
            assert_eq!(calls.len(), 1);
            assert_order(&calls[0].1);
        }
    }
}

#[tokio::test]
async fn compact_retains_nested_forward_compatible_json_after_model_rewrite() {
    let (app, captured) = app("openai").await;
    // Compaction need not support structured output to forward future fields
    // losslessly. This checks the native transport contract, not provider acceptance.
    send(app, "/v1/responses/compact", format!(r#"{{"model":"schema/gpt-5-codex","input":"hi","future":{{"schema":{SCHEMA},"number":1.2300e-4,"nullable":null}}}}"#)).await;
    let calls = captured.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "/v1/responses/compact");
    assert_order(&calls[0].1);
    assert!(calls[0].1.contains(r#""model":"gpt-5-codex""#));
    assert!(calls[0].1.contains(r#""number":1.2300e-4"#));
    assert!(calls[0].1.contains(r#""nullable":null"#));
}

#[tokio::test]
async fn translated_messages_keep_multiple_tools_attached_to_their_own_schemas() {
    for streaming in [false, true] {
        let (app, captured) = app("openai").await;
        send(app, "/v1/messages", format!(r#"{{"model":"schema/gpt-4o","messages":[{{"role":"user","content":"hi"}}],"max_tokens":64,"stream":{streaming},"tools":[{{"name":"first","input_schema":{{"properties":{{"zz_first":{{}},"aa_first":{{}}}}}}}},{{"name":"second","input_schema":{{"properties":{{"yy_second":{{}},"bb_second":{{}}}}}}}}]}}"#)).await;
        let calls = captured.lock().await;
        assert_eq!(calls.len(), 1);
        let wire = &calls[0].1;
        assert!(
            wire.contains(
                r#""name":"first","parameters":{"properties":{"zz_first":{},"aa_first":{}}}"#
            ),
            "{wire}"
        );
        assert!(
            wire.contains(
                r#""name":"second","parameters":{"properties":{"yy_second":{},"bb_second":{}}}"#
            ),
            "{wire}"
        );
    }
}

#[tokio::test]
async fn non_object_responses_fail_before_upstream_dispatch_with_the_object_error() {
    for wire in ["[]", "null", "false", "42", r#""text""#] {
        let (app, captured) = app("openai").await;
        let response = app
            .oneshot(
                Request::post("/v1/responses")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                    .body(Body::from(wire))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("Responses request must be a JSON object"));
        assert!(captured.lock().await.is_empty());
    }
}

#[tokio::test]
async fn malformed_translated_tools_fail_before_upstream_dispatch() {
    for tools in [
        r#"[{"name":"valid","input_schema":{}},{"name":"missing_schema"}]"#,
        r#"[{"input_schema":{}}]"#,
        "{}",
    ] {
        let (app, captured) = app("openai").await;
        let wire = format!(
            r#"{{"model":"schema/gpt-4o","messages":[{{"role":"user","content":"hi"}}],"max_tokens":64,"tools":{tools}}}"#
        );
        let response = app
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {TEST_API_KEY}"))
                    .body(Body::from(wire))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(captured.lock().await.is_empty());
    }
}
