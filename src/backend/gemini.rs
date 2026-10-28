//! Native Gemini Interactions and image embeddings alongside Google's OpenAI compatibility API.

use super::openai_compatible::OpenAICompatibleBackend;
use super::provider::{BackendError, BackendProvider, SseByteStream};
use crate::embedding_limits::gemini_embedding_2_limits;
use crate::types::*;
use async_trait::async_trait;
use axum::http::HeaderMap;
use bytes::Bytes;

mod embeddings;

pub const GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

pub struct GeminiBackend {
    client: reqwest::Client,
    api_key: String,
    native_base_url: String,
    compatible: OpenAICompatibleBackend,
}

/// Existing saved `/v1beta/openai` URLs retain their compatibility behavior.
/// Native operations use the sibling API root, preserving custom prefixes.
pub fn gemini_backend(
    client: reqwest::Client,
    api_key: String,
    base_url_override: Option<&str>,
) -> GeminiBackend {
    let base = base_url_override
        .unwrap_or(GEMINI_API_BASE_URL)
        .trim_end_matches('/');
    GeminiBackend {
        compatible: OpenAICompatibleBackend::for_gemini(
            client.clone(),
            api_key.clone(),
            base.to_string(),
        ),
        client,
        api_key,
        native_base_url: base.strip_suffix("/openai").unwrap_or(base).to_string(),
    }
}

#[async_trait]
impl BackendProvider for GeminiBackend {
    fn supports_native_chat(&self) -> bool {
        true
    }
    async fn create_native_chat_completion(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendChatResponse, BackendError> {
        self.compatible.create_native_chat_completion(request).await
    }
    async fn create_native_chat_completion_stream(
        &self,
        request: crate::native_chat::NativeChatRequest,
    ) -> Result<super::provider::BackendSseResponse, BackendError> {
        self.compatible
            .create_native_chat_completion_stream(request)
            .await
    }
    async fn create_native_embedding(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateEmbeddingRequest>,
    ) -> Result<super::provider::BackendJsonView<CreateEmbeddingResponse>, BackendError> {
        if gemini_embedding_2_limits(&request.model).is_none() {
            return self.compatible.create_native_embedding(request).await;
        }
        let view = self
            .create_embedding(request.compatibility_request()?)
            .await?;
        Ok(super::provider::BackendJsonView {
            response: super::provider::BackendBodyResponse::from_json(&view)?,
            view,
        })
    }
    async fn create_native_image(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateImageRequest>,
    ) -> Result<super::provider::BackendJsonView<ImageResponse>, BackendError> {
        self.compatible.create_native_image(request).await
    }
    async fn create_native_speech(
        &self,
        request: crate::native_json::NativeJsonRequest<CreateSpeechRequest>,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        self.compatible.create_native_speech(request).await
    }
    async fn create_native_audio(
        &self,
        request: crate::native_audio::NativeAudioRequest,
    ) -> Result<super::provider::BackendBodyResponse, BackendError> {
        self.compatible.create_native_audio(request).await
    }
    async fn send_gemini_interaction(
        &self,
        headers: &HeaderMap,
        body: Bytes,
        stream: bool,
    ) -> Result<reqwest::Response, BackendError> {
        let mut request = self
            .client
            .post(format!("{}/interactions", self.native_base_url))
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .body(body);
        // Credentials and billing-project headers always belong to the saved
        // connection. Only protocol/client metadata is forwarded from callers.
        for name in ["x-goog-api-client", "x-goog-api-version"] {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        if stream {
            request = request.query(&[("alt", "sse")]);
        }
        // Preserve status and error bytes; the handler owns fallback decisions.
        Ok(request.send().await?)
    }

    async fn create_chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        self.compatible.create_chat_completion(request).await
    }
    async fn create_chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<SseByteStream, BackendError> {
        self.compatible.create_chat_completion_stream(request).await
    }
    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        if gemini_embedding_2_limits(&request.model).is_some() {
            self.create_multimodal_embedding(request).await
        } else {
            self.compatible.create_embedding(request).await
        }
    }
    async fn create_image(
        &self,
        request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        self.compatible.create_image(request).await
    }
    async fn create_speech(
        &self,
        request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        self.compatible.create_speech(request).await
    }
    async fn create_transcription(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        language: Option<String>,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        self.compatible
            .create_transcription(
                model,
                file_bytes,
                file_name,
                language,
                prompt,
                response_format,
                temperature,
            )
            .await
    }
    async fn create_translation(
        &self,
        model: String,
        file_bytes: bytes::Bytes,
        file_name: String,
        prompt: Option<String>,
        response_format: Option<String>,
        temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        self.compatible
            .create_translation(
                model,
                file_bytes,
                file_name,
                prompt,
                response_format,
                temperature,
            )
            .await
    }
    async fn create_moderation(
        &self,
        request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        self.compatible.create_moderation(request).await
    }
    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        self.compatible.list_models().await
    }
    async fn probe_models(&self) -> Result<ListResponse<Model>, BackendError> {
        self.compatible.probe_models().await
    }
    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        self.compatible.get_model(model_id).await
    }
    fn name(&self) -> &str {
        "gemini"
    }
}
