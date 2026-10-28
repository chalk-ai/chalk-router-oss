use crate::capability_requirements::image_generation_requirements;
use crate::middleware::ResolvedApiKey;
use crate::native_json::NativeJsonRequest;
use crate::quota::TokenReservation;
use crate::ratelimit::RateLimitModel;
use crate::tenancy::RequestState as State;
use crate::types::CreateImageRequest;
use axum::{extract::Json, response::Response};

use super::routing::{acquire_single_route_quota, check_passthrough_capabilities};
use super::trace_attributes::{ContentDirection, LlmSpanKind, LlmSpanWriter};

pub async fn create_image(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Json(mut req): Json<NativeJsonRequest<CreateImageRequest>>,
) -> Response {
    if req
        .value()
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return super::not_implemented("streaming image generation");
    }
    let span = tracing::info_span!("create_image", otel.name = "image.create",);
    let span = LlmSpanWriter::new(span, state.object_store.clone(), LlmSpanKind::Llm);
    super::stamp_authenticated_principal(&span, &api_key);

    span.clone().instrument(async move {
        let current_span = span;
        if let Some(ref model) = req.model {
            current_span.set_model_system(model.clone(), state.backend.name().to_string());
        } else {
            current_span.set_system(state.backend.name().to_string());
        }
        let log_content = current_span.content_enabled();
        if log_content {
            current_span.record_text(ContentDirection::Input, req.prompt.clone());
        }

        // OpenInference: invocation parameters
        let mut params = serde_json::Map::new();
        if let Some(n) = req.n {
            params.insert("n".into(), serde_json::json!(n));
        }
        if let Some(ref quality) = req.quality {
            params.insert("quality".into(), serde_json::json!(quality));
        }
        if let Some(ref size) = req.size {
            params.insert("size".into(), serde_json::json!(size));
        }
        if let Some(ref style) = req.style {
            params.insert("style".into(), serde_json::json!(style));
        }
        if let Some(ref background) = req.background {
            params.insert("background".into(), serde_json::json!(background));
        }
        if let Some(ref output_format) = req.output_format {
            params.insert("output_format".into(), serde_json::json!(output_format));
        }
        if let Some(ref aspect_ratio) = req.aspect_ratio {
            params.insert("aspect_ratio".into(), serde_json::json!(aspect_ratio));
        }
        if let Some(ref image_size) = req.image_size {
            params.insert("image_size".into(), serde_json::json!(image_size));
        }
        if let Some(ref response_format) = req.response_format {
            params.insert("response_format".into(), serde_json::json!(response_format));
        }
        if !params.is_empty() {
            if let Ok(params_json) = serde_json::to_string(&params) {
                current_span.set_invocation_parameters_json(params_json);
            }
        }

        // Resolve + gate before dispatch. `/v1/images/generations` routes on the
        // model prefix to an arbitrary connection, so a restricted key must be
        // held to its provider/model allowlist here too (moderation stays
        // disabled). When a model is present we resolve it exactly once
        // (lenient) and dispatch straight to the resolved leaf backend; without a
        // model the request defaults to OpenAI, so a restricted key is best-effort
        // gated against provider "openai".
        let (dispatch, mut quota_permit) = match req.model.clone() {
            Some(model) => {
                let route = match state.backend.resolve(&model).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "Image generation routing error");
                        return super::backend_error_response(&e);
                    }
                };
                // system/model now describe the resolved upstream connection.
                current_span.set_route(&route.resolution);
                if api_key.0.permission.is_restricted() {
                    if let Err(denied) = api_key.0.permission.allows_route(&route.resolution, &model) {
                        tracing::warn!(
                            key_prefix = %api_key.0.key_prefix,
                            model = %model,
                            reason = %denied,
                            "Image generation denied by API key permission"
                        );
                        return super::forbidden_response(denied.to_string());
                    }
                }
                let requirements = image_generation_requirements(&req);
                if let Err(e) = check_passthrough_capabilities(
                    &route,
                    Some(&model),
                    &requirements,
                    "openai_images.generate",
                ) {
                    return super::backend_error_response(&e);
                }
                let quota_permit = match acquire_single_route_quota(
                    &state,
                    &api_key,
                    &route.resolution,
                    RateLimitModel::Exact(&model),
                    TokenReservation::NotApplicable,
                    None,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(error) => return super::backend_error_response(&error),
                };
                req.set_model(route.stripped_model.clone());
                (route.backend.create_native_image(req).await, quota_permit)
            }
            None => {
                // No model given: resolve the OpenAI default connection exactly
                // once, authorize against its real identity (kind + immutable
                // connection id), then dispatch straight to that leaf backend —
                // keeping `model: None` so the upstream picks its own default.
                // Avoids a fabricated identity and a separate dispatch-time
                // re-resolve.
                let route = match state.backend.resolve_default().await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::error!(error = %e, "Image generation (no model) routing error");
                        return super::backend_error_response(&e);
                    }
                };
                current_span.set_route(&route.resolution);
                if api_key.0.permission.is_restricted() {
                    if let Err(denied) = api_key.0.permission.allows_route(&route.resolution, "") {
                        tracing::warn!(
                            key_prefix = %api_key.0.key_prefix,
                            reason = %denied,
                            "Image generation (no model) denied by API key permission"
                        );
                        return super::forbidden_response(denied.to_string());
                    }
                }
                let requirements = image_generation_requirements(&req);
                if let Err(e) = check_passthrough_capabilities(
                    &route,
                    None,
                    &requirements,
                    "openai_images.generate",
                ) {
                    return super::backend_error_response(&e);
                }
                let quota_permit = match acquire_single_route_quota(
                    &state,
                    &api_key,
                    &route.resolution,
                    RateLimitModel::ProviderSelected,
                    TokenReservation::NotApplicable,
                    None,
                )
                .await
                {
                    Ok(permit) => permit,
                    Err(error) => return super::backend_error_response(&error),
                };
                (route.backend.create_native_image(req).await, quota_permit)
            }
        };

        match dispatch {
            Ok(native) => {
                let response = native.view;
                if let Some(permit) = quota_permit.take() {
                    permit.finish(None).await;
                }
                // OpenInference: output (content). Strip base64 blobs to keep the
                // span small; record the URL and revised prompt. Skipped entirely
                // when content logging is disabled.
                if log_content {
                    let sanitized: Vec<serde_json::Value> = response
                        .data
                        .iter()
                        .map(|img| {
                            serde_json::json!({
                                "url": img.url,
                                "revised_prompt": img.revised_prompt,
                                "b64_json": img.b64_json.as_ref().map(|_| "<base64 omitted>"),
                            })
                        })
                        .collect();
                    current_span.record_json(ContentDirection::Output, &sanitized);
                }
                super::native_body_response(native.response)
            }
            Err(e) => {
                if let Some(permit) = quota_permit.take() {
                    permit.finish_failed().await;
                }
                tracing::error!(error = %e, "Image generation backend error");
                // Preserve the upstream status (e.g. a provider 429) and forward
                // rate-limit / retry headers instead of collapsing to 500.
                super::backend_error_response(&e)
            }
        }
    })
    .await
}

pub async fn create_image_edit(
    State(_state): State<crate::AppState>,
    Json(_req): Json<serde_json::Value>,
) -> Response {
    super::not_implemented("Image editing")
}

pub async fn create_image_variation(
    State(_state): State<crate::AppState>,
    Json(_req): Json<serde_json::Value>,
) -> Response {
    super::not_implemented("Image variations")
}
