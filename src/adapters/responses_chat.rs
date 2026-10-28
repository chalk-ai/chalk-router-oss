//! Execute Responses requests through a Chat backend.

use std::fmt;

use crate::backend::provider::{BackendJsonResponse, BackendSseResponse};
use crate::backend::{BackendError, BackendProvider};
use crate::native_chat::NativeChatRequest;
use crate::responses::ResponsesRequest;
use crate::translation::responses_chat::{
    retain_native_chat_reply_fields, synthesize_response_stream,
};

/// Map a compatibility validation error to the client-facing HTTP 400 error.
fn translation_error(error: impl fmt::Display) -> BackendError {
    BackendError::api_status(400, error.to_string())
}

/// Compatibility implementation used by providers that expose chat
/// completions but not the Responses API. The request converter rejects any
/// field or item that cannot be represented without changing semantics.
pub async fn create_response_via_chat<P>(
    provider: &P,
    request: ResponsesRequest,
) -> Result<BackendJsonResponse, BackendError>
where
    P: BackendProvider + Sync + ?Sized,
{
    let chat_request = request.to_chat_completion().map_err(translation_error)?;
    let chat_response = provider
        .create_native_chat_completion(NativeChatRequest::from_typed(chat_request)?)
        .await?;
    let raw_reply = serde_json::from_str(chat_response.body.get())
        .map_err(|error| BackendError::Other(format!("invalid native Chat reply: {error}")))?;
    let mut response = request
        .to_compatibility_response(chat_response.completion)
        .map_err(translation_error)?;
    retain_native_chat_reply_fields(&mut response, &raw_reply).map_err(translation_error)?;
    let body = serde_json::to_value(response).map_err(|error| {
        BackendError::Other(format!("failed to serialize Responses response: {error}"))
    })?;
    BackendJsonResponse::from_value(body, chat_response.headers)
}

/// Complete a compatible chat request and expose it as a synthesized Responses
/// stream. Kept alongside the non-streaming adapter so trait defaults and
/// concrete providers cannot drift in request mutation or event generation.
pub async fn create_response_stream_via_chat<P>(
    provider: &P,
    mut request: ResponsesRequest,
) -> Result<BackendSseResponse, BackendError>
where
    P: BackendProvider + Sync + ?Sized,
{
    request.set_stream(false);
    let response = create_response_via_chat(provider, request).await?;
    Ok(BackendSseResponse {
        stream: synthesize_response_stream(response.body)?,
        headers: response.response.headers,
        status: 200,
        content_type: "text/event-stream".into(),
    })
}
