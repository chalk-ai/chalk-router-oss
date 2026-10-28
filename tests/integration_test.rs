use openai_server::{
    backend::{
        AnthropicBackend, BackendProvider, OpenAICompatibleBackend, RouterBackend, VoyageBackend,
    },
    model_routes::InMemoryModelRouteStore,
    provider_connections::{
        ConnectionInput, ExposurePolicy, InMemoryProviderConnectionStore, ProviderConnectionStore,
    },
    types::{
        ChatCompletionRequest, ChatMessage, CreateEmbeddingRequest, EmbeddingData, EmbeddingInput,
    },
};
use std::sync::Arc;

/// Live provider smoke tests hit real OpenAI / Anthropic / Voyage endpoints
/// with paid API keys. They are **opt-in**: they run only when
/// `ROUTER_LIVE_API_TESTS` is set (`1`/`true`). Without it every test in this
/// file no-ops, so `cargo test --test integration_test` makes no live request.
/// (The suite is also `test = false` in Cargo.toml, so the default `cargo test`
/// and CI never run it either.)
fn live_api_tests_enabled() -> bool {
    std::env::var("ROUTER_LIVE_API_TESTS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Skip (no-op return) the enclosing `#[tokio::test]` unless live API tests are
/// explicitly enabled, so no real provider request is ever made by default.
macro_rules! require_live {
    () => {
        if !live_api_tests_enabled() {
            eprintln!("skipping live API integration test; set ROUTER_LIVE_API_TESTS=1 to run");
            return;
        }
    };
}

/// Build a [`RouterBackend`] whose connection store holds one connection per
/// `(kind, api_key)`, each routed by the kind's canonical prefix (e.g.
/// `openai/`). Mirrors how the Providers UI / seeder register connections.
async fn router_with_connections(conns: &[(&str, String)]) -> RouterBackend {
    let store: Arc<dyn ProviderConnectionStore> = Arc::new(InMemoryProviderConnectionStore::new());
    for (kind, api_key) in conns {
        store
            .create(ConnectionInput {
                name: kind.to_string(),
                provider_kind: kind.to_string(),
                base_url: None,
                prefix: Some(format!("{kind}/")),
                exposure: ExposurePolicy::Dynamic,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: api_key.clone(),
            })
            .await
            .unwrap();
    }
    RouterBackend::new(
        reqwest::Client::new(),
        store,
        Arc::new(InMemoryModelRouteStore::new()),
    )
}

#[tokio::test]
async fn test_openai_backend_chat_completion() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Create a simple chat completion request
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "gpt-3.5-turbo".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Hello, integration test!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    // Note: OpenAI may return a specific version like "gpt-3.5-turbo-0125" instead of just "gpt-3.5-turbo"
    assert!(
        response.model.starts_with("gpt-3.5-turbo"),
        "Model should be gpt-3.5-turbo variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    println!("Chat completion response: {:?}", response);
}

#[tokio::test]
async fn test_openai_backend_list_models() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // List models
    let response = backend
        .list_models()
        .await
        .expect("List models should succeed");

    // Verify the response
    assert_eq!(response.object, "list");
    assert!(!response.data.is_empty(), "Should have at least one model");

    println!("Found {} models", response.data.len());
    println!("First few models:");
    for model in response.data.iter().take(5) {
        println!("  - {} (owned by {})", model.id, model.owned_by);
    }
}

#[tokio::test]
async fn test_openai_backend_get_model() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Get a specific model
    let model = backend
        .get_model("gpt-3.5-turbo")
        .await
        .expect("Get model should succeed");

    // Verify the response
    assert_eq!(model.id, "gpt-3.5-turbo");
    assert_eq!(model.object, "model");

    println!("Model details: {:?}", model);
}

#[tokio::test]
async fn test_anthropic_backend_haiku_chat_completion() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get Anthropic API key from environment
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set in environment or .env file for integration tests");

    // Create Anthropic backend
    let backend: Arc<dyn BackendProvider> =
        Arc::new(AnthropicBackend::new(reqwest::Client::new(), api_key));

    // Create a simple chat completion request
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "claude-haiku-4-5".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Hello from Haiku!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.contains("claude-haiku-4"),
        "Model should be Claude Haiku 4.x variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    // Verify usage info is present
    assert!(response.usage.is_some(), "Should have usage information");
    if let Some(usage) = &response.usage {
        assert!(usage.prompt_tokens > 0, "Should have prompt tokens");
        assert!(
            usage.completion_tokens.unwrap_or(0) > 0,
            "Should have completion tokens"
        );
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens.unwrap_or(0)
        );
    }

    println!("Anthropic Haiku chat completion response: {:?}", response);
}

#[tokio::test]
async fn test_router_backend_anthropic_prefix() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get API keys from environment
    let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set in environment or .env file for integration tests");

    // Create router backend with a stored anthropic connection.
    let backend: Arc<dyn BackendProvider> =
        Arc::new(router_with_connections(&[("anthropic", anthropic_api_key)]).await);

    // Create a chat completion request with anthropic/ prefix
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "anthropic/claude-haiku-4-5".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Router test passed!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.contains("claude-haiku-4"),
        "Model should be Claude Haiku 4.x variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    println!(
        "Router backend (anthropic/) chat completion response: {:?}",
        response
    );
}

#[tokio::test]
async fn test_router_backend_openai_prefix() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get API keys from environment
    let openai_api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create router backend with a stored openai connection.
    let backend: Arc<dyn BackendProvider> =
        Arc::new(router_with_connections(&[("openai", openai_api_key)]).await);

    // Create a chat completion request with openai/ prefix
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "openai/gpt-3.5-turbo".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'OpenAI router test passed!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.starts_with("gpt-3.5-turbo"),
        "Model should be gpt-3.5-turbo variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    println!(
        "Router backend (openai/) chat completion response: {:?}",
        response
    );
}

#[tokio::test]
async fn test_openai_gpt5_nano_chat_completion() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Create a simple chat completion request
    // Note: GPT-5 Nano only supports default temperature (1) and requires max_completion_tokens
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "gpt-5-nano".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Hello from GPT-5 Nano!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: None,
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: None,
        max_completion_tokens: Some(50),
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    // Note: OpenAI may return a specific version with suffix instead of just "gpt-5-nano"
    assert!(
        response.model.starts_with("gpt-5-nano"),
        "Model should be gpt-5-nano variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    // Verify usage info is present
    assert!(response.usage.is_some(), "Should have usage information");
    if let Some(usage) = &response.usage {
        assert!(usage.prompt_tokens > 0, "Should have prompt tokens");
        assert!(
            usage.completion_tokens.unwrap_or(0) > 0,
            "Should have completion tokens"
        );
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens.unwrap_or(0)
        );
    }

    println!("GPT-5 Nano chat completion response: {:?}", response);
}

#[tokio::test]
async fn test_openai_gpt4o_chat_completion() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Create a simple chat completion request
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "gpt-4o".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Hello from GPT-4o!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.starts_with("gpt-4o"),
        "Model should be gpt-4o variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    // Verify usage info is present
    assert!(response.usage.is_some(), "Should have usage information");
    if let Some(usage) = &response.usage {
        assert!(usage.prompt_tokens > 0, "Should have prompt tokens");
        assert!(
            usage.completion_tokens.unwrap_or(0) > 0,
            "Should have completion tokens"
        );
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens.unwrap_or(0)
        );
    }

    println!("GPT-4o chat completion response: {:?}", response);
}

#[tokio::test]
async fn test_anthropic_claude_sonnet_45_chat_completion() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get Anthropic API key from environment
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY must be set in environment or .env file for integration tests");

    // Create Anthropic backend
    let backend: Arc<dyn BackendProvider> =
        Arc::new(AnthropicBackend::new(reqwest::Client::new(), api_key));

    // Create a simple chat completion request
    let request = ChatCompletionRequest {
        fallbacks: Default::default(),
        reasoning_effort: None,
        model: "claude-sonnet-4-5".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some(serde_json::json!(
                "Say 'Hello from Claude Sonnet 4.5!' and nothing else."
            )),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.7),
        top_p: None,
        n: None,
        stream: Some(false),
        stop: None,
        max_tokens: Some(50),
        max_completion_tokens: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        user: None,
        tools: None,
        tool_choice: None,
        response_format: None,
        stream_options: None,
        parallel_tool_calls: None,
    };

    // Call the backend
    let response = backend
        .create_chat_completion(request)
        .await
        .expect("Chat completion should succeed");

    // Verify the response
    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.contains("claude-sonnet-4"),
        "Model should be Claude Sonnet 4.x variant"
    );
    assert!(
        !response.choices.is_empty(),
        "Should have at least one choice"
    );

    let first_choice = &response.choices[0];
    assert_eq!(first_choice.message.role, "assistant");
    assert!(
        first_choice.message.content.is_some(),
        "Should have content"
    );

    // Verify usage info is present
    assert!(response.usage.is_some(), "Should have usage information");
    if let Some(usage) = &response.usage {
        assert!(usage.prompt_tokens > 0, "Should have prompt tokens");
        assert!(
            usage.completion_tokens.unwrap_or(0) > 0,
            "Should have completion tokens"
        );
        assert_eq!(
            usage.total_tokens,
            usage.prompt_tokens + usage.completion_tokens.unwrap_or(0)
        );
    }

    println!("Claude Sonnet 4.5 chat completion response: {:?}", response);
}

#[tokio::test]
async fn test_openai_text_embedding_3_small() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Create an embedding request with a single input
    let request = CreateEmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Single("The quick brown fox jumps over the lazy dog".to_string()),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    // Call the backend
    let response = backend
        .create_embedding(request)
        .await
        .expect("Embedding creation should succeed");

    // Verify the response
    assert_eq!(response.object, "list");
    assert_eq!(response.model, "text-embedding-3-small");
    assert!(
        !response.data.is_empty(),
        "Should have at least one embedding"
    );

    let first_embedding = &response.data[0];
    assert_eq!(first_embedding.object, "embedding");
    assert_eq!(first_embedding.index, 0);
    let dims = match &first_embedding.embedding {
        EmbeddingData::Float(v) => v.len(),
        EmbeddingData::Base64(_) => panic!("Expected float embedding"),
    };
    assert!(dims > 0, "Embedding vector should not be empty");

    // text-embedding-3-small has 1536 dimensions by default
    assert_eq!(dims, 1536, "Default dimensions should be 1536");

    // Verify usage info is present
    assert!(
        response.usage.as_ref().unwrap().prompt_tokens > 0,
        "Should have prompt tokens"
    );
    assert!(
        response.usage.as_ref().unwrap().total_tokens > 0,
        "Should have total tokens"
    );

    println!(
        "Text embedding response: model={}, dimensions={}, prompt_tokens={}",
        response.model,
        dims,
        response.usage.as_ref().unwrap().prompt_tokens
    );
}

#[tokio::test]
async fn test_openai_text_embedding_3_small_multiple_inputs() {
    // Load environment variables
    require_live!();
    dotenvy::dotenv().ok();

    // Get OpenAI API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");

    // Create OpenAI backend
    let backend: Arc<dyn BackendProvider> = Arc::new(OpenAICompatibleBackend::new(
        reqwest::Client::new(),
        api_key,
    ));

    // Create an embedding request with multiple inputs
    let request = CreateEmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Multiple(vec![
            "Hello world".to_string(),
            "Goodbye world".to_string(),
            "Testing embeddings".to_string(),
        ]),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    // Call the backend
    let response = backend
        .create_embedding(request)
        .await
        .expect("Embedding creation should succeed");

    // Verify the response
    assert_eq!(response.object, "list");
    assert_eq!(response.model, "text-embedding-3-small");
    assert_eq!(response.data.len(), 3, "Should have three embeddings");

    // Verify each embedding
    for (i, embedding) in response.data.iter().enumerate() {
        assert_eq!(embedding.object, "embedding");
        assert_eq!(embedding.index, i as i32);
        let dims = match &embedding.embedding {
            EmbeddingData::Float(v) => v.len(),
            EmbeddingData::Base64(_) => panic!("Expected float embedding"),
        };
        assert_eq!(dims, 1536, "Each embedding should have 1536 dimensions");
    }

    // Verify usage info is present
    assert!(
        response.usage.as_ref().unwrap().prompt_tokens > 0,
        "Should have prompt tokens"
    );
    assert!(
        response.usage.as_ref().unwrap().total_tokens > 0,
        "Should have total tokens"
    );

    println!(
        "Multiple text embeddings response: model={}, count={}, prompt_tokens={}",
        response.model,
        response.data.len(),
        response.usage.as_ref().unwrap().prompt_tokens
    );
}

#[tokio::test]
async fn test_voyage_embedding_text_single() {
    require_live!();
    dotenvy::dotenv().ok();

    let api_key = std::env::var("VOYAGE_API_KEY")
        .expect("VOYAGE_API_KEY must be set in environment or .env file for integration tests");

    let backend: Arc<dyn BackendProvider> =
        Arc::new(VoyageBackend::new(reqwest::Client::new(), api_key));

    let request = CreateEmbeddingRequest {
        model: "voyage-3.5-lite".to_string(),
        input: EmbeddingInput::Single("The quick brown fox jumps over the lazy dog".to_string()),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    let response = backend
        .create_embedding(request)
        .await
        .expect("Voyage embedding creation should succeed");

    assert_eq!(response.object, "list");
    assert!(
        response.model.starts_with("voyage"),
        "Model should be a voyage variant, got: {}",
        response.model
    );
    assert_eq!(response.data.len(), 1, "Should have one embedding");

    let first = &response.data[0];
    assert_eq!(first.object, "embedding");
    assert_eq!(first.index, 0);
    let dims = match &first.embedding {
        EmbeddingData::Float(v) => v.len(),
        EmbeddingData::Base64(_) => panic!("Expected float embedding"),
    };
    assert!(dims > 0, "Embedding vector should not be empty");

    // Voyage only returns total_tokens; the backend mirrors that into prompt_tokens.
    assert!(
        response.usage.as_ref().unwrap().total_tokens > 0,
        "Should have total tokens"
    );
    assert_eq!(
        response.usage.as_ref().unwrap().prompt_tokens,
        response.usage.as_ref().unwrap().total_tokens,
        "Voyage backend should mirror total_tokens into prompt_tokens"
    );

    println!(
        "Voyage embedding response: model={}, dimensions={}, total_tokens={}",
        response.model,
        dims,
        response.usage.as_ref().unwrap().total_tokens
    );
}

#[tokio::test]
async fn test_voyage_embedding_text_multiple() {
    require_live!();
    dotenvy::dotenv().ok();

    let api_key = std::env::var("VOYAGE_API_KEY")
        .expect("VOYAGE_API_KEY must be set in environment or .env file for integration tests");

    let backend: Arc<dyn BackendProvider> =
        Arc::new(VoyageBackend::new(reqwest::Client::new(), api_key));

    let request = CreateEmbeddingRequest {
        model: "voyage-3.5-lite".to_string(),
        input: EmbeddingInput::Multiple(vec![
            "Hello world".to_string(),
            "Goodbye world".to_string(),
            "Testing embeddings".to_string(),
        ]),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    let response = backend
        .create_embedding(request)
        .await
        .expect("Voyage embedding creation should succeed");

    assert_eq!(response.object, "list");
    assert_eq!(response.data.len(), 3, "Should have three embeddings");

    let mut last_dims: Option<usize> = None;
    for (i, embedding) in response.data.iter().enumerate() {
        assert_eq!(embedding.object, "embedding");
        assert_eq!(embedding.index, i as i32);
        let dims = match &embedding.embedding {
            EmbeddingData::Float(v) => v.len(),
            EmbeddingData::Base64(_) => panic!("Expected float embedding"),
        };
        assert!(dims > 0, "Embedding vector should not be empty");
        if let Some(prev) = last_dims {
            assert_eq!(dims, prev, "All embeddings should share the same dimension");
        }
        last_dims = Some(dims);
    }

    assert!(
        response.usage.as_ref().unwrap().total_tokens > 0,
        "Should have total tokens"
    );
}

#[tokio::test]
async fn test_voyage_via_router_prefix() {
    require_live!();
    dotenvy::dotenv().ok();

    let voyage_api_key = std::env::var("VOYAGE_API_KEY")
        .expect("VOYAGE_API_KEY must be set in environment or .env file for integration tests");

    let backend: Arc<dyn BackendProvider> =
        Arc::new(router_with_connections(&[("voyage", voyage_api_key)]).await);

    let request = CreateEmbeddingRequest {
        model: "voyage/voyage-3.5-lite".to_string(),
        input: EmbeddingInput::Single("Routing through the voyage prefix".to_string()),
        encoding_format: None,
        dimensions: None,
        user: None,
    };

    let response = backend
        .create_embedding(request)
        .await
        .expect("Voyage embedding via router should succeed");

    assert_eq!(response.data.len(), 1);
    let dims = match &response.data[0].embedding {
        EmbeddingData::Float(v) => v.len(),
        EmbeddingData::Base64(_) => panic!("Expected float embedding"),
    };
    assert!(dims > 0);
    assert!(response.usage.as_ref().unwrap().total_tokens > 0);
}

// ---------------------------------------------------------------------------
// Braintrust span export (live)
// ---------------------------------------------------------------------------

/// Round-trip the router's frontend spans through Braintrust's real OTLP ingest
/// endpoint and read them back out of Braintrust's API.
///
/// The hermetic half of this feature — which spans the filter forwards, and
/// what the exporter puts on the wire — lives in
/// `tests/integration/braintrust_span_test.rs`. What only a live run can prove
/// is the other side of the contract: that Braintrust accepts our credentials
/// and project addressing, and that it reads the OpenInference attributes the
/// router already emits as an LLM span with parsed input, output, and token
/// metrics. If Braintrust ever stops interpreting those conventions, this is
/// what notices.
///
/// Opt-in twice over: `ROUTER_LIVE_API_TESTS` (like every test in this file)
/// and a `BRAINTRUST_API_KEY`.
#[tokio::test]
async fn test_braintrust_frontend_span_round_trip() {
    use openai_server::telemetry::braintrust::{
        BraintrustConfig, BraintrustConfigState, BraintrustSpanExporter, BraintrustSpanProcessor,
    };
    use opentelemetry::trace::{TraceContextExt, TracerProvider as _};
    use opentelemetry_sdk::export::trace::SpanExporter;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::{SimpleSpanProcessor, TracerProvider};
    use opentelemetry_sdk::Resource;
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    use tracing_subscriber::layer::SubscriberExt;

    require_live!();
    dotenvy::dotenv().ok();

    let Some(api_key) = std::env::var("BRAINTRUST_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!("skipping Braintrust round-trip; set BRAINTRUST_API_KEY to run");
        return;
    };
    let api_url = std::env::var("BRAINTRUST_API_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.braintrust.dev".to_string());
    let api_url = api_url.trim_end_matches('/').to_string();
    let project = std::env::var("BRAINTRUST_PROJECT")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "chalk-router-live-test".to_string());

    let client = reqwest::Client::new();

    // `PUT /v1/project` registers by name idempotently, so a repeat run reuses
    // the same project instead of accumulating one per run.
    let project_id = client
        .put(format!("{api_url}/v1/project"))
        .bearer_auth(&api_key)
        .json(&serde_json::json!({ "name": project }))
        .send()
        .await
        .expect("register Braintrust project")
        .error_for_status()
        .expect("Braintrust rejected the project registration")
        .json::<serde_json::Value>()
        .await
        .expect("project response is JSON")["id"]
        .as_str()
        .expect("project id")
        .to_string();

    // Capture through the real filter, so what goes out is exactly what a
    // production deployment would send.
    let captured = InMemorySpanExporter::default();
    let filter_configs = Arc::new(BraintrustConfigState::new(vec![BraintrustConfig {
        traces_url: "http://unused.test/otel/v1/traces".to_string(),
        api_key: "unused".to_string(),
        parent: "project_name:test".to_string(),
    }]));
    let provider = TracerProvider::builder()
        .with_span_processor(BraintrustSpanProcessor::new(
            SimpleSpanProcessor::new(Box::new(captured.clone())),
            filter_configs,
        ))
        .build();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("chalk-router")));

    // The span shape `OtelMakeSpan` + `handlers::chat` produce for one request:
    // a frontend request span with the LLM span nested inside it.
    let prompt = "Say hello to Braintrust from chalk-router.";
    let completion = "Hello from chalk-router!";
    let llm_span_id = {
        let _guard = tracing::subscriber::set_default(subscriber);

        let request_span = tracing::info_span!(
            "http_request",
            otel.name = "POST /v1/chat/completions",
            http.method = "POST",
        );
        request_span.set_attribute("chalk.frontend", "openai");
        request_span.set_attribute("chalk.frontend.endpoint", "chat.completions");

        let _entered = request_span.enter();

        let llm_span = tracing::info_span!("chat_completion", otel.name = "chat.completion");
        llm_span.set_attribute("openinference.span.kind", "LLM");
        llm_span.set_attribute("llm.system", "openai");
        llm_span.set_attribute("llm.provider", "openai");
        llm_span.set_attribute("llm.model_name", "gpt-4o-mini");
        llm_span.set_attribute("llm.input_messages.0.message.role", "user");
        llm_span.set_attribute("llm.input_messages.0.message.content", prompt);
        llm_span.set_attribute("llm.output_messages.0.message.role", "assistant");
        llm_span.set_attribute("llm.output_messages.0.message.content", completion);
        llm_span.set_attribute("llm.token_count.prompt", 11_i64);
        llm_span.set_attribute("llm.token_count.completion", 6_i64);
        llm_span.set_attribute("llm.token_count.total", 17_i64);

        let span_id = llm_span
            .context()
            .span()
            .span_context()
            .span_id()
            .to_string();
        drop(llm_span);
        span_id
    };

    let spans = captured.get_finished_spans().expect("captured spans");
    assert_eq!(
        spans.len(),
        2,
        "the filter should forward the request span and the LLM span"
    );

    let config = BraintrustConfig {
        traces_url: format!("{api_url}/otel/v1/traces"),
        api_key: api_key.clone(),
        parent: format!("project_id:{project_id}"),
    };
    BraintrustSpanExporter::new(
        Arc::new(BraintrustConfigState::new(vec![config])),
        &Resource::empty(),
    )
    .export(spans)
    .await
    .expect("Braintrust accepted the OTLP export");

    // Ingest is asynchronous on Braintrust's side, so poll instead of sleeping
    // once for a guessed interval.
    let fetch_logs = || async {
        client
            .post(format!("{api_url}/v1/project_logs/{project_id}/fetch"))
            .bearer_auth(&api_key)
            .json(&serde_json::json!({ "limit": 100 }))
            .send()
            .await
            .expect("fetch project logs")
            .json::<serde_json::Value>()
            .await
            .expect("project logs are JSON")
    };
    let row_for = |logs: &serde_json::Value, span_id: &str| -> Option<serde_json::Value> {
        logs["events"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["span_id"].as_str() == Some(span_id))
                .cloned()
        })
    };

    let mut logs = serde_json::Value::Null;
    let mut logged = None;
    for _ in 0..15 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        logs = fetch_logs().await;
        logged = row_for(&logs, &llm_span_id);
        if logged.is_some() {
            break;
        }
    }

    let logged = logged.unwrap_or_else(|| {
        panic!("span {llm_span_id} never appeared in Braintrust project {project_id}")
    });

    // Braintrust read the OpenInference conventions, not just the raw attributes.
    assert_eq!(logged["span_attributes"]["type"].as_str(), Some("llm"));
    assert_eq!(
        logged["span_attributes"]["name"].as_str(),
        Some("chat.completion")
    );
    assert_eq!(logged["input"][0]["role"].as_str(), Some("user"));
    assert_eq!(logged["input"][0]["content"].as_str(), Some(prompt));
    assert_eq!(logged["output"][0]["role"].as_str(), Some("assistant"));
    assert_eq!(logged["output"][0]["content"].as_str(), Some(completion));
    assert_eq!(logged["metrics"]["prompt_tokens"].as_i64(), Some(11));
    assert_eq!(logged["metrics"]["tokens"].as_i64(), Some(17));
    // Braintrust's OpenInference translation does not read
    // `llm.token_count.completion`; the exporter supplies it via
    // `braintrust.metrics`, and this is the assertion that catches it
    // regressing (or Braintrust starting to read it themselves).
    assert_eq!(logged["metrics"]["completion_tokens"].as_i64(), Some(6));
    assert_eq!(
        logged["metadata"]["llm.model_name"].as_str(),
        Some("gpt-4o-mini")
    );

    // And the frontend request span is the trace's root, carrying the dialect.
    let root_span_id = logged["span_parents"][0]
        .as_str()
        .expect("the LLM span is nested under the frontend request span")
        .to_string();
    let root = row_for(&logs, &root_span_id).expect("the frontend request span reached Braintrust");

    assert_eq!(
        root["span_attributes"]["name"].as_str(),
        Some("POST /v1/chat/completions")
    );
    assert_eq!(root["metadata"]["chalk.frontend"].as_str(), Some("openai"));
    assert_eq!(
        root["metadata"]["chalk.frontend.endpoint"].as_str(),
        Some("chat.completions")
    );
    assert_eq!(root["is_root"].as_bool(), Some(true));
}

/// Jev prompting against the live TypeSafe API: a chat prompt with a
/// `response_format` schema is answered as typed decisions. Needs
/// `TYPESAFE_API_KEY` on top of `ROUTER_LIVE_API_TESTS`.
#[tokio::test]
async fn test_typesafe_jev_prompting_chat_completion() {
    require_live!();
    dotenvy::dotenv().ok();

    let api_key = std::env::var("TYPESAFE_API_KEY")
        .expect("TYPESAFE_API_KEY must be set in environment or .env file for integration tests");
    let router = router_with_connections(&[("typesafe", api_key)]).await;

    let request: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
        "model": "typesafe/jev-latest",
        "messages": [
            {"role": "system", "content": "You triage customer support tickets."},
            {"role": "user", "content": "I was charged twice for my order and nobody is answering my emails. This is the third time!"}
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": {
                "name": "triage",
                "schema": {
                    "type": "object",
                    "properties": {
                        "billing": {"type": "boolean", "description": "Is this message about billing?"},
                        "tone": {
                            "type": "string",
                            "description": "What is the tone of the customer's message?",
                            "oneOf": [
                                {"const": "angry", "description": "upset or hostile"},
                                {"const": "calm", "description": "neutral or polite"}
                            ]
                        },
                        "urgency": {
                            "type": "integer",
                            "description": "How urgent is this ticket?",
                            "oneOf": [
                                {"const": 0, "description": "Can wait"},
                                {"const": 1, "description": "Needs attention this week"},
                                {"const": 2, "description": "Needs attention today"}
                            ]
                        },
                        "spam_probability": {
                            "type": "number", "minimum": 0, "maximum": 1,
                            "description": "Is this message spam?"
                        }
                    }
                }
            }
        }
    }))
    .unwrap();

    let response = router
        .create_chat_completion(request)
        .await
        .expect("Jev prompting should succeed against the live TypeSafe API");

    assert_eq!(response.object, "chat.completion");
    assert!(
        response.model.starts_with("jev-"),
        "Model should be a Jev release, got: {}",
        response.model
    );
    let content = response.choices[0]
        .message
        .content
        .as_ref()
        .and_then(|content| content.as_str())
        .expect("assistant content should be a JSON string");
    let answers: serde_json::Value = serde_json::from_str(content).expect("content should be JSON");
    println!("Jev prompting answers: {answers}");
    assert_eq!(answers["billing"], true);
    assert_eq!(answers["tone"], "angry");
    assert_eq!(answers["urgency"], 2);
    let spam = answers["spam_probability"]
        .as_f64()
        .expect("spam_probability should be a number");
    assert!((0.0..=1.0).contains(&spam) && spam < 0.5, "spam={spam}");

    let usage = response.usage.expect("usage should be reported");
    assert!(usage.prompt_tokens > 0, "Should have prompt tokens");
    assert_eq!(
        usage.total_tokens,
        usage.prompt_tokens + usage.completion_tokens.unwrap_or_default()
    );
    println!(
        "Jev prompting response: model={}, prompt_tokens={}, completion_tokens={:?}",
        response.model, usage.prompt_tokens, usage.completion_tokens
    );
}

/// The native decisions surface against the live TypeSafe API, for the same
/// key: catalog discovery plus one System One request.
#[tokio::test]
async fn test_typesafe_native_decisions() {
    require_live!();
    dotenvy::dotenv().ok();

    let api_key = std::env::var("TYPESAFE_API_KEY")
        .expect("TYPESAFE_API_KEY must be set in environment or .env file for integration tests");
    let router = router_with_connections(&[("typesafe", api_key)]).await;

    let catalog = router.list_models().await.expect("TypeSafe catalog");
    let ids: Vec<&str> = catalog.data.iter().map(|model| model.id.as_str()).collect();
    println!("TypeSafe catalog: {ids:?}");
    assert!(ids.contains(&"typesafe/jev-latest"), "{ids:?}");
    assert!(
        catalog.data.iter().all(|model| model.created > 0),
        "release timestamps should parse"
    );

    let request: openai_server::decisions::DecisionsRequest =
        serde_json::from_value(serde_json::json!({
            "model": "jev-latest",
            "state": "Checkout page renders blank after clicking Pay.",
            "questions": {
                "team": {
                    "type": "choice",
                    "instructions": "Which team owns this?",
                    "criteria": {
                        "frontend": "Rendering and browser issues",
                        "payments": "Billing and checkout issues"
                    }
                }
            }
        }))
        .unwrap();
    let route = router
        .resolve("typesafe/jev-latest")
        .await
        .expect("typesafe/jev-latest should resolve");
    let response = route
        .backend
        .send_decisions_request(request)
        .await
        .expect("System One request should succeed");
    assert!(response.status().is_success(), "{}", response.status());
    let body: serde_json::Value = response.json().await.unwrap();
    println!("System One answers: {body}");
    assert!(body["answers"]["team"]["choice"].is_string());
    assert!(body["usage"]["input_tokens"].as_u64().unwrap_or_default() > 0);
}

/// Hosted-tool soft caps settle the usage the provider reports on completion.
/// Confirm that OpenAI's Responses usage includes the provider-side search
/// context (far above the visible request) in both delivery modes.
#[tokio::test]
async fn test_openai_responses_web_search_reports_hosted_context_usage() {
    require_live!();
    dotenvy::dotenv().ok();
    let api_key = std::env::var("OPENAI_API_KEY")
        .expect("OPENAI_API_KEY must be set in environment or .env file for integration tests");
    let client = reqwest::Client::new();
    let body = |stream: bool| {
        serde_json::json!({
            "model": "gpt-4.1-mini",
            "input": "Search the web twice: once for today's top headline on bbc.com and once for today's top headline on reuters.com. Reply with both in one line.",
            "tools": [{"type": "web_search"}],
            "tool_choice": "required",
            "max_output_tokens": 200,
            "stream": stream,
        })
    };
    let visible_bytes = serde_json::to_vec(&body(false)).unwrap().len() as u64;

    let response: serde_json::Value = client
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(&api_key)
        .json(&body(false))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let searches = response["output"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item["type"] == "web_search_call")
                .count()
        })
        .unwrap_or_default();
    let usage = &response["usage"];
    println!("non-streaming: searches={searches} visible_bytes={visible_bytes} usage={usage}");
    assert!(searches >= 1, "expected a web_search_call: {response}");
    let input_tokens = usage["input_tokens"].as_u64().expect("input_tokens");
    assert!(
        input_tokens > visible_bytes,
        "usage should include hosted search context: {usage}"
    );

    let text = client
        .post("https://api.openai.com/v1/responses")
        .bearer_auth(&api_key)
        .json(&body(true))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let completed = text
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed event");
    let usage = &completed["response"]["usage"];
    println!("streaming: usage={usage}");
    assert!(
        usage["input_tokens"].as_u64().expect("input_tokens") > visible_bytes,
        "streamed usage should include hosted search context: {usage}"
    );
}
