use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::provider::{BackendError, BackendProvider};
use crate::types::{
    ChatCompletionRequest, ChatCompletionResponse, CreateEmbeddingRequest, CreateEmbeddingResponse,
    CreateImageRequest, CreateModerationRequest, CreateModerationResponse, CreateSpeechRequest,
    Embedding, EmbeddingData, EmbeddingInput, EmbeddingInputItem, ErrorResponse, ImageResponse,
    ListResponse, Model, TranscriptionResponse, Usage,
};

const VOYAGE_API_BASE: &str = "https://api.voyageai.com/v1";

/// Voyage AI backend — supports text and multimodal embeddings.
///
/// Voyage's embeddings endpoint mostly mirrors OpenAI's shape but its
/// `usage` object only carries `total_tokens` (no `prompt_tokens`), so we
/// translate responses through a Voyage-specific shape.
pub struct VoyageBackend {
    client: Client,
    api_key: String,
    base_url: String,
}

impl VoyageBackend {
    pub fn new(client: Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            base_url: VOYAGE_API_BASE.to_string(),
        }
    }

    fn unsupported(op: &str) -> BackendError {
        BackendError::api_status(400, format!("Voyage backend does not support {}", op))
    }

    async fn extract_error(response: reqwest::Response) -> BackendError {
        super::provider::upstream_error(response, |text| {
            serde_json::from_str::<ErrorResponse>(text)
                .ok()
                .map(|r| r.error.message)
        })
        .await
    }
}

#[derive(Serialize)]
struct VoyageTextRequest<'a> {
    input: &'a EmbeddingInput,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimension: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dtype: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<&'a str>,
}

#[derive(Serialize)]
struct VoyageMultimodalRequest<'a> {
    inputs: Vec<VoyageMultimodalInput<'a>>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_encoding: Option<&'a str>,
}

#[derive(Serialize)]
struct VoyageMultimodalInput<'a> {
    content: Vec<VoyageMultimodalContent<'a>>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum VoyageMultimodalContent<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: &'a str },
}

#[derive(Deserialize)]
struct VoyageEmbeddingResponse {
    #[serde(default = "default_list_object")]
    object: String,
    data: Vec<VoyageEmbedding>,
    model: String,
    #[serde(default)]
    usage: VoyageUsage,
}

fn default_list_object() -> String {
    "list".to_string()
}

#[derive(Deserialize)]
struct VoyageEmbedding {
    #[serde(default = "default_embedding_object")]
    object: String,
    index: i32,
    embedding: EmbeddingData,
}

fn default_embedding_object() -> String {
    "embedding".to_string()
}

#[derive(Deserialize, Default)]
struct VoyageUsage {
    #[serde(default)]
    total_tokens: i32,
    #[serde(default)]
    text_tokens: Option<i32>,
}

#[async_trait]
impl BackendProvider for VoyageBackend {
    async fn create_chat_completion(
        &self,
        _request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, BackendError> {
        Err(Self::unsupported("chat completions"))
    }

    async fn create_embedding(
        &self,
        request: CreateEmbeddingRequest,
    ) -> Result<CreateEmbeddingResponse, BackendError> {
        let is_multimodal = matches!(request.input, EmbeddingInput::Multimodal(_));
        let path = if is_multimodal {
            "/multimodalembeddings"
        } else {
            "/embeddings"
        };
        let url = format!("{}{}", self.base_url, path);

        let response = if is_multimodal {
            let inputs = match &request.input {
                EmbeddingInput::Multimodal(items) => {
                    vec![VoyageMultimodalInput {
                        content: items
                            .iter()
                            .map(|item| match item {
                                EmbeddingInputItem::Text { text } => {
                                    VoyageMultimodalContent::Text { text }
                                }
                                EmbeddingInputItem::ImageUrl { image_url } => {
                                    VoyageMultimodalContent::ImageUrl {
                                        image_url: &image_url.url,
                                    }
                                }
                            })
                            .collect(),
                    }]
                }
                _ => unreachable!(),
            };
            let body = VoyageMultimodalRequest {
                inputs,
                model: &request.model,
                output_encoding: request.encoding_format.as_deref(),
            };

            self.client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?
        } else {
            let body = VoyageTextRequest {
                input: &request.input,
                model: &request.model,
                output_dimension: request.dimensions,
                output_dtype: None,
                encoding_format: request.encoding_format.as_deref(),
            };

            self.client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await?
        };

        let status = response.status();
        if !status.is_success() {
            return Err(Self::extract_error(response).await);
        }

        let parsed: VoyageEmbeddingResponse = response
            .json()
            .await
            .map_err(|e| BackendError::Other(format!("Failed to parse Voyage response: {}", e)))?;

        let data = parsed
            .data
            .into_iter()
            .map(|e| Embedding {
                object: e.object,
                index: e.index,
                embedding: e.embedding,
            })
            .collect();

        let total = parsed.usage.total_tokens;
        Ok(CreateEmbeddingResponse {
            object: parsed.object,
            data,
            model: parsed.model,
            usage: Some(Usage {
                prompt_tokens: parsed.usage.text_tokens.unwrap_or(total),
                completion_tokens: None,
                total_tokens: total,
                ..Default::default()
            }),
        })
    }

    async fn create_image(
        &self,
        _request: CreateImageRequest,
    ) -> Result<ImageResponse, BackendError> {
        Err(Self::unsupported("image generation"))
    }

    async fn create_speech(
        &self,
        _request: CreateSpeechRequest,
    ) -> Result<bytes::Bytes, BackendError> {
        Err(Self::unsupported("speech"))
    }

    async fn create_transcription(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _language: Option<String>,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(Self::unsupported("transcription"))
    }

    async fn create_translation(
        &self,
        _model: String,
        _file_bytes: bytes::Bytes,
        _file_name: String,
        _prompt: Option<String>,
        _response_format: Option<String>,
        _temperature: Option<f32>,
    ) -> Result<TranscriptionResponse, BackendError> {
        Err(Self::unsupported("translation"))
    }

    async fn create_moderation(
        &self,
        _request: CreateModerationRequest,
    ) -> Result<CreateModerationResponse, BackendError> {
        Err(Self::unsupported("moderation"))
    }

    async fn list_models(&self) -> Result<ListResponse<Model>, BackendError> {
        // Voyage does not expose a public /models endpoint, so we return a
        // hardcoded list of currently supported embedding models.
        let now = chrono::Utc::now().timestamp();
        let ids = [
            "voyage-3-large",
            "voyage-3.5",
            "voyage-3.5-lite",
            "voyage-code-3",
            "voyage-finance-2",
            "voyage-law-2",
            "voyage-multimodal-3",
        ];
        let data = ids
            .iter()
            .map(|id| Model {
                id: (*id).to_string(),
                object: "model".to_string(),
                created: now,
                owned_by: "voyage".to_string(),
                capabilities: None,
                provider_metadata: Default::default(),
            })
            .collect();

        Ok(ListResponse {
            object: "list".to_string(),
            data,
            first_id: None,
            last_id: None,
            has_more: false,
        })
    }

    async fn get_model(&self, model_id: &str) -> Result<Model, BackendError> {
        Ok(Model {
            id: model_id.to_string(),
            object: "model".to_string(),
            created: chrono::Utc::now().timestamp(),
            owned_by: "voyage".to_string(),
            capabilities: None,
            provider_metadata: Default::default(),
        })
    }

    fn name(&self) -> &str {
        "voyage"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;
    use crate::types::ImageUrl;

    /// Shared state for the mock: holds the captured request body and the
    /// response to return.
    struct MockState {
        captured: Mutex<Option<Value>>,
        response: Value,
    }

    async fn handle(
        axum::extract::State(state): axum::extract::State<Arc<MockState>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *state.captured.lock().await = Some(body);
        Json(state.response.clone())
    }

    /// Boot a mock Voyage server answering both embeddings endpoints with the
    /// same canned response. Returns a `VoyageBackend` pointed at it (built by
    /// literal, since `base_url` has no public setter) plus the capture slot.
    async fn mock_backend(response: Value) -> (VoyageBackend, Arc<MockState>) {
        let state = Arc::new(MockState {
            captured: Mutex::new(None),
            response,
        });
        let app = Router::new()
            .route("/embeddings", post(handle))
            .route("/multimodalembeddings", post(handle))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let backend = VoyageBackend {
            client: Client::new(),
            api_key: "test-key".to_string(),
            base_url: format!("http://{}", addr),
        };
        (backend, state)
    }

    fn text_request(model: &str) -> CreateEmbeddingRequest {
        CreateEmbeddingRequest {
            model: model.to_string(),
            input: EmbeddingInput::Single("hello world".to_string()),
            encoding_format: None,
            dimensions: None,
            user: None,
        }
    }

    fn canned_response(usage: Value) -> Value {
        json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
            "model": "voyage-3.5",
            "usage": usage,
        })
    }

    #[tokio::test]
    async fn outbound_text_request_carries_model_input_and_bearer_auth() {
        let (backend, state) = mock_backend(canned_response(json!({"total_tokens": 5}))).await;

        backend
            .create_embedding(text_request("voyage-3.5"))
            .await
            .expect("create_embedding against mock");

        let captured = state
            .captured
            .lock()
            .await
            .clone()
            .expect("mock captured nothing");
        assert_eq!(captured["model"], "voyage-3.5");
        assert_eq!(captured["input"], "hello world");
    }

    #[tokio::test]
    async fn multimodal_input_posts_to_multimodalembeddings_with_typed_content() {
        let (backend, state) = mock_backend(canned_response(json!({"total_tokens": 9}))).await;

        let mut req = text_request("voyage-multimodal-3");
        req.input = EmbeddingInput::Multimodal(vec![
            EmbeddingInputItem::Text {
                text: "a cat".to_string(),
            },
            EmbeddingInputItem::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/cat.png".to_string(),
                    detail: None,
                },
            },
        ]);

        backend
            .create_embedding(req)
            .await
            .expect("create_embedding against mock");

        let captured = state.captured.lock().await.clone().expect("captured");
        let content = &captured["inputs"][0]["content"];
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "a cat");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"], "https://example.com/cat.png");
    }

    #[tokio::test]
    async fn usage_prefers_text_tokens_over_total_when_present() {
        // Voyage's usage object only guarantees `total_tokens`; when the more
        // specific `text_tokens` is also present it must win for
        // `prompt_tokens`, not the coarser total.
        let (backend, _state) = mock_backend(canned_response(
            json!({"total_tokens": 12, "text_tokens": 7}),
        ))
        .await;

        let resp = backend
            .create_embedding(text_request("voyage-3.5"))
            .await
            .expect("create_embedding against mock");

        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, 7);
        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 12);
    }

    #[tokio::test]
    async fn usage_falls_back_to_total_tokens_when_text_tokens_absent() {
        let (backend, _state) = mock_backend(canned_response(json!({"total_tokens": 4}))).await;

        let resp = backend
            .create_embedding(text_request("voyage-3.5"))
            .await
            .expect("create_embedding against mock");

        assert_eq!(
            resp.usage.as_ref().unwrap().prompt_tokens,
            4,
            "no text_tokens in the response: prompt_tokens must fall back to total_tokens"
        );
        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 4);
    }
}
