//! Compile documentation examples against the actual wire types. No services or credentials.
use openai_server::{
    decisions::{DecisionsRequest, SystemOneRequest},
    native_chat::NativeChatRequest,
    native_json::NativeJsonRequest,
    responses::ResponsesRequest,
    translation::decisions_openai::OpenAiDecisionsRequest,
    types::*,
};
use serde::de::DeserializeOwned;
use std::path::Path;

fn example<T: DeserializeOwned>(name: &str) -> T {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs-site/examples")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}

#[test]
fn published_requests_match_actual_codecs() {
    let chat: NativeChatRequest = example("chat");
    chat.compatibility_request().unwrap();
    let stream: NativeChatRequest = example("chat-stream");
    assert!(stream.include_usage());
    stream.compatibility_request().unwrap();
    let responses: ResponsesRequest = example("responses");
    responses.to_chat_completion().unwrap();
    let _: CreateEmbeddingRequest = example("embeddings");
    let _: NativeJsonRequest<CreateImageRequest> = example("images");
    let _: NativeJsonRequest<CreateSpeechRequest> = example("speech");
    let _: CreateModerationRequest = example("moderations");
    example::<DecisionsRequest>("decisions").validate().unwrap();
    example::<SystemOneRequest>("systemone").validate().unwrap();
    let request: OpenAiDecisionsRequest = example("openai-decisions");
    openai_server::translation::decisions_openai::ingress_to_canonical(request).unwrap();
}

#[test]
fn illustrative_responses_match_wire_types() {
    let _: ChatCompletionResponse = example("chat-response");
    let _: CreateEmbeddingResponse = example("embeddings-response");
    let _: ErrorResponse = example("error-response");
}

#[test]
fn native_envelopes_are_not_mistaken_for_closed_compatibility_schemas() {
    let mut body: serde_json::Value = example("chat");
    body["example_future_provider_field"] = serde_json::json!({"retained": true});
    let native: NativeChatRequest = serde_json::from_value(body).unwrap();
    assert_eq!(
        native.value()["example_future_provider_field"]["retained"],
        true
    );
    assert!(native.compatibility_request().is_err());
    let mut body: serde_json::Value = example("responses");
    body["previous_response_id"] = serde_json::json!("opaque-example-id");
    let native: ResponsesRequest = serde_json::from_value(body).unwrap();
    assert!(native.to_chat_completion().is_err());
}
