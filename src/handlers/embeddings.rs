use crate::capability_requirements::embedding_requirements;
use crate::embedding_quota::{embedding_token_reservation, EmbeddingDocument};
use crate::middleware::ResolvedApiKey;
use crate::native_json::NativeJsonRequest;
use crate::quota::TokenReservation;
use crate::ratelimit::RateLimitModel;
use crate::storage;
use crate::tenancy::RequestState as State;
use crate::types::{
    CreateEmbeddingRequest, EmbeddingData, EmbeddingInput, EmbeddingInputItem, Usage,
};
use crate::usage_estimator::NormalizedUsageInput;
use crate::usage_tracker::{UsageSource, UsageWriteError};
use axum::{extract::Json, response::IntoResponse};

use super::routing::{acquire_single_route_quota, check_passthrough_capabilities};
use super::trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter};
use super::usage::TokenUsage;

fn embedding_usage(
    reported: Option<&Usage>,
    provider_kind: &str,
    estimated_input_tokens: Option<u64>,
) -> Result<TokenUsage, UsageWriteError> {
    let Some(reported) = reported else {
        // Gemini's openai-compatible endpoint for embeddings omits usage metrics,
        // and Vertex's `:embedContent` does not promise them either.
        // Reusing the text-input reservation estimate for now
        let input_tokens = estimated_input_tokens
            .filter(|_| matches!(provider_kind, "gemini" | "vertex"))
            .ok_or(UsageWriteError::Invalid("embedding usage was missing"))?;
        return Ok(TokenUsage {
            input_tokens,
            total_tokens: input_tokens,
            usage_source: UsageSource::Estimated,
            ..Default::default()
        });
    };
    let prompt_tokens = u64::try_from(reported.prompt_tokens)
        .map_err(|_| UsageWriteError::Invalid("embedding prompt token count was negative"))?;
    if u64::try_from(reported.total_tokens).ok() != Some(prompt_tokens) {
        tracing::warn!(
            reported_total_tokens = reported.total_tokens,
            derived_total_tokens = prompt_tokens,
            "Embedding token total disagrees with normalized components"
        );
    }
    Ok(TokenUsage {
        input_tokens: prompt_tokens,
        total_tokens: prompt_tokens,
        input_token_attribution: reported.input_token_attribution,
        ..Default::default()
    })
}

pub async fn create_embedding(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Json(mut req): Json<NativeJsonRequest<CreateEmbeddingRequest>>,
) -> impl IntoResponse {
    let span = tracing::info_span!("create_embedding", otel.name = "embedding.create",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Embedding);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone()
        .instrument(async move {
            let current_span = span;

            // OpenInference: invocation parameters
            let mut params = serde_json::Map::new();
            if let Some(ref fmt) = req.encoding_format {
                params.insert("encoding_format".into(), serde_json::json!(fmt));
            }
            if let Some(dims) = req.dimensions {
                params.insert("dimensions".into(), serde_json::json!(dims));
            }
            if !params.is_empty() {
                if let Ok(params_json) = serde_json::to_string(&params) {
                    current_span.set_invocation_parameters_json(params_json);
                }
            }

            // input_count is metadata and always recorded.
            let input_count = match &req.input {
                EmbeddingInput::Single(_)
                | EmbeddingInput::Tokens(_)
                | EmbeddingInput::Multimodal(_) => 1,
                EmbeddingInput::TokenBatch(inputs) => inputs.len(),
                EmbeddingInput::Multiple(arr) => arr.len(),
            };
            current_span.set_embedding_input_count(input_count);

            // OpenInference: input content (full input + per-item text) and image
            // uploads are only done when content logging is enabled. Skipping it when
            // off avoids serializing the input, cloning per-item text, and the image
            // upload round-trips entirely.
            let log_content = super::content_logging_enabled();
            if log_content {
                match &req.input {
                    EmbeddingInput::Single(value) => {
                        current_span.record_json(ContentDirection::Input, &[value]);
                    }
                    EmbeddingInput::Multiple(values) => {
                        current_span.record_json(ContentDirection::Input, values);
                    }
                    EmbeddingInput::Tokens(_) | EmbeddingInput::TokenBatch(_) => {
                        current_span.record_json(ContentDirection::Input, &req.input);
                    }
                    EmbeddingInput::Multimodal(items) => {
                        current_span.record_json(ContentDirection::Input, items);
                    }
                }
                let mut input_texts: Vec<(usize, String)> = Vec::new();
                let mut omitted_texts = 0usize;

                match &req.input {
                    EmbeddingInput::Single(s) => {
                        if current_span.try_content(
                            ContentDirection::Input,
                            "embedding.embeddings.0.embedding.text",
                            s.clone(),
                        ) {
                            input_texts.push((0, s.clone()));
                        } else {
                            omitted_texts += 1;
                        }
                    }
                    EmbeddingInput::Multiple(arr) => {
                        for (index, text) in arr.iter().enumerate() {
                            if current_span.try_content(
                                ContentDirection::Input,
                                format!("embedding.embeddings.{index}.embedding.text"),
                                text.clone(),
                            ) {
                                input_texts.push((index, text.clone()));
                            } else {
                                omitted_texts += 1;
                            }
                        }
                    }
                    EmbeddingInput::Tokens(_) | EmbeddingInput::TokenBatch(_) => {}
                    EmbeddingInput::Multimodal(items) => {
                        for (index, item) in items.iter().enumerate() {
                            match item {
                                EmbeddingInputItem::Text { text } => {
                                    if current_span.try_content(
                                        ContentDirection::Input,
                                        format!("embedding.embeddings.{index}.embedding.text"),
                                        text.clone(),
                                    ) {
                                        input_texts.push((index, text.clone()));
                                    } else {
                                        omitted_texts += 1;
                                    }
                                }
                                EmbeddingInputItem::ImageUrl { .. } => {
                                    if current_span.try_content(
                                        ContentDirection::Input,
                                        format!("embedding.embeddings.{index}.embedding.text"),
                                        "[image]",
                                    ) {
                                        input_texts.push((index, "[image]".to_string()));
                                    } else {
                                        omitted_texts += 1;
                                    }
                                }
                            }
                        }
                    }
                }

                if omitted_texts > 0 {
                    current_span.truncation(
                        ContentDirection::Input,
                        "embedding_texts",
                        serde_json::json!({
                            "total": input_count,
                            "recorded": input_texts.len(),
                            "content_fields_omitted": omitted_texts,
                        }),
                    );
                }
            }

            tracing::debug!(
                "Creating embedding for model: {} via backend: {}",
                req.model,
                state.backend.name()
            );

            // Embeddings are dispatched exactly once. The fallback policy is limited
            // to non-streaming chat by design, so no alternate-model retry is applied
            // here — a primary failure is returned as-is.
            let requested_model = req.model.clone();
            let fallback_usage_estimate = req.input.token_count().or_else(|| {
                serde_json::to_value(&req)
                    .ok()
                    .as_ref()
                    .and_then(NormalizedUsageInput::from_json)
                    .map(|input| input.conservative_tokens())
            });

            // Resolve the route EXACTLY ONCE and carry that resolution through the
            // permission gate, span tagging, dispatch, and usage attribution. A
            // resolve error is terminal (surfaced as the routing error) so a
            // restricted key whose model won't resolve is never dispatched unchecked.
            let route = match state.backend.resolve(&requested_model).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "Embedding routing error");
                    return super::backend_error_response(&e);
                }
            };
            let provider_kind = route.resolution.provider_kind.clone();
            let upstream_model_id = route.resolution.upstream_model_id.clone();

            // OpenInference: EMBEDDING-kind spans use the `embedding.*` namespace for
            // model/system (that's what consumers parse); keep the `llm.*` keys too
            // for generic consumers. Stamped from the resolution NOW — before the
            // gate and dispatch — so a denied/errored span still carries them.
            current_span.set_embedding_route(upstream_model_id.clone(), provider_kind.clone());

            // Enforce the key's provider/model allowlist before dispatch.
            if api_key.0.permission.is_restricted() {
                if let Err(denied) = api_key
                    .0
                    .permission
                    .allows_route(&route.resolution, &requested_model)
                {
                    tracing::warn!(
                        key_prefix = %api_key.0.key_prefix,
                        model = %requested_model,
                        reason = %denied,
                        "Embedding denied by API key permission"
                    );
                    return super::forbidden_response(denied.to_string());
                }
            }

            let requirements = embedding_requirements(&req.input);
            if let Err(e) = check_passthrough_capabilities(
                &route,
                Some(&requested_model),
                &requirements,
                "openai_embeddings.create",
            ) {
                return super::backend_error_response(&e);
            }

            // An admission upper bound is not a missing-usage estimate. Images
            // still require authoritative provider usage when settling the request.
            let image_count = match &req.input {
                EmbeddingInput::Multimodal(items) => items
                    .iter()
                    .filter(|item| matches!(item, EmbeddingInputItem::ImageUrl { .. }))
                    .count(),
                _ => 0,
            };
            let reservation_tokens = match embedding_token_reservation(
                &provider_kind,
                &upstream_model_id,
                [EmbeddingDocument {
                    image_count,
                    text_tokens: fallback_usage_estimate,
                }],
            ) {
                Ok(tokens) => tokens,
                Err(error) => return super::backend_error_response(&error),
            };

            let mut quota_permit = match acquire_single_route_quota(
                &state,
                &api_key,
                &route.resolution,
                RateLimitModel::Exact(&requested_model),
                TokenReservation::from_option(reservation_tokens),
                reservation_tokens,
            )
            .await
            {
                Ok(permit) => permit,
                Err(error) => return super::backend_error_response(&error),
            };

            if log_content {
                if let EmbeddingInput::Multimodal(items) = &req.input {
                    // Upload images to object storage and record their URIs
                    let mut image_idx = 0usize;
                    let mut omitted_images = 0usize;
                    for item in items {
                        if let EmbeddingInputItem::ImageUrl { image_url } = item {
                            if !current_span.can_record_attributes(ContentDirection::Input, 1) {
                                omitted_images += 1;
                                continue;
                            }
                            match upload_image_to_store(&image_url.url, &state.object_store).await {
                                Ok(uri) => {
                                    if current_span.try_content(
                                        ContentDirection::Input,
                                        format!("image.{image_idx}.url"),
                                        uri,
                                    ) {
                                        image_idx += 1;
                                    } else {
                                        omitted_images += 1;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        url = %image_url.url,
                                        "Failed to upload embedding image to object store"
                                    );
                                }
                            }
                        }
                    }
                    if omitted_images > 0 {
                        current_span.truncation(
                            ContentDirection::Input,
                            "images",
                            serde_json::json!({"attributes_omitted": omitted_images}),
                        );
                    }
                }
            }

            // Dispatch straight to the resolved leaf backend with the stripped model
            // — never back through the router (which would re-resolve).
            req.set_model(route.stripped_model.clone());
            let result = route.backend.create_native_embedding(req).await;

            match result {
                Ok(native) => {
                    let response = native.view;
                    let usage = match embedding_usage(
                        response.usage.as_ref(),
                        &provider_kind,
                        fallback_usage_estimate,
                    ) {
                        Ok(usage) => usage,
                        Err(error) => {
                            if let Some(permit) = quota_permit.take() {
                                permit.finish(None).await;
                            }
                            tracing::error!(%error, "Cannot account for embedding usage");
                            return super::usage_write_error_response();
                        }
                    };
                    usage.stamp(&current_span);

                    // OpenInference: output. Count + dimension are metadata; the raw
                    // vectors (output.value) are content and skipped when disabled —
                    // avoiding serializing every embedding vector into a span attribute.
                    let dimension =
                        response
                            .data
                            .first()
                            .and_then(|embedding| match &embedding.embedding {
                                EmbeddingData::Float(values) => Some(values.len()),
                                _ => None,
                            });
                    current_span.set_embedding_output_shape(response.data.len(), dimension);
                    if log_content {
                        current_span.record_json(ContentDirection::Output, &response.data);
                    }

                    tracing::debug!(
                        response.model = %response.model,
                        embeddings_count = response.data.len(),
                        usage.prompt_tokens = usage.input_tokens,
                        usage.total_tokens = usage.total_tokens,
                        usage.source = ?usage.usage_source,
                        "Embedding creation successful"
                    );

                    if super::usage::with_quota_settlement(
                        super::usage::record_future(
                            state.clone(),
                            super::usage::UsageAttribution::new(&api_key, &route.resolution),
                            usage,
                        ),
                        quota_permit.take(),
                        Some(usage.input_tokens),
                    )
                    .await
                    .is_err()
                    {
                        return super::usage_write_error_response();
                    }

                    super::native_body_response(native.response)
                }
                Err(e) => {
                    if let Some(permit) = quota_permit.take() {
                        permit.finish_failed().await;
                    }
                    tracing::error!(
                        error = %e,
                        error.type = "backend_error",
                        "Backend error occurred"
                    );
                    // Preserve the upstream status (e.g. a provider 429) and forward
                    // rate-limit / retry headers instead of collapsing to 500.
                    super::backend_error_response(&e)
                }
            }
        })
        .await
}

/// Upload an image (from URL or data URI) to the configured object store.
/// Returns the stored object URI, or an error string.
async fn upload_image_to_store(
    image_url: &str,
    object_store: &crate::storage::SharedObjectStore,
) -> Result<String, String> {
    let store = object_store
        .as_ref()
        .ok_or_else(|| "object store not configured".to_string())?;

    let (bytes, ext) = storage::fetch_image_bytes(image_url).await?;
    let key = store
        .put_hashed(&bytes, &ext)
        .await
        .map_err(|e| format!("object store write failed: {}", e))?;
    Ok(store.uri_for_key(&key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracing_image_upload_refuses_private_urls_with_an_object_store() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let fetches = Arc::new(AtomicUsize::new(0));
        let calls = fetches.clone();
        let app = axum::Router::new().route(
            "/secret.png",
            axum::routing::get(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { ([("content-type", "image/png")], "internal-secret") }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = Some(Arc::new(crate::storage::ObjectStore::memory()));
        let error = upload_image_to_store(&format!("http://{address}/secret.png"), &store)
            .await
            .unwrap_err();
        assert!(error.contains("image URL not allowed"), "{error}");
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
        // A configured store still receives valid inline images through this path.
        let uri = upload_image_to_store("data:image/png;base64,aW1hZ2U=", &store)
            .await
            .unwrap();
        assert!(uri.ends_with(".png"), "{uri}");
        server.abort();
    }

    #[test]
    fn missing_gemini_usage_is_estimated_as_input_only() {
        let usage = embedding_usage(None, "gemini", Some(50)).unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.total_tokens, 50);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.usage_source, UsageSource::Estimated);
        assert!(embedding_usage(None, "gemini", None).is_err());
        assert!(embedding_usage(None, "openai-compatible", Some(50)).is_err());
    }

    #[test]
    fn reported_embedding_usage_takes_precedence_over_estimates() {
        let mut reported = Usage {
            prompt_tokens: 5,
            total_tokens: 5,
            ..Default::default()
        };
        let usage = embedding_usage(Some(&reported), "gemini", Some(50)).unwrap();
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.total_tokens, 5);
        assert_eq!(usage.usage_source, UsageSource::Reported);
        reported.prompt_tokens = -1;
        assert!(embedding_usage(Some(&reported), "gemini", Some(50)).is_err());
    }
}
