//! Execute Anthropic Messages requests through a Chat backend.

use bytes::Bytes;
use serde_json::Value;

use crate::backend::{BackendError, BackendProvider};
use crate::translation::anthropic_chat::{parse_request, response_value, streaming_body};

/// An Anthropic reply with its encoded body, streaming mode, and usage-bearing JSON.
pub struct TranslatedAnthropicResponse {
    pub body: Bytes,
    pub streaming: bool,
    pub value: Value,
}

/// Convert an Anthropic request, call the Chat backend, and encode the Anthropic reply.
/// Validation and backend errors propagate; SSE is synthesized after the complete reply.
pub async fn translate_messages(
    backend: &dyn BackendProvider,
    body: &Bytes,
) -> Result<TranslatedAnthropicResponse, BackendError> {
    let (request, streaming) = parse_request(body)?;
    let message = response_value(backend.create_chat_completion(request).await?)?;
    let body = if streaming {
        streaming_body(&message)
    } else {
        Bytes::from(
            serde_json::to_vec(&message).map_err(|error| BackendError::Other(error.to_string()))?,
        )
    };
    Ok(TranslatedAnthropicResponse {
        body,
        streaming,
        value: message,
    })
}
