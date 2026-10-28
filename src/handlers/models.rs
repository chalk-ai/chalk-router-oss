use crate::api_keys::ApiKeyPermission;
use crate::middleware::ResolvedApiKey;
use crate::tenancy::RequestState as State;
use crate::types::{ErrorResponse, ListResponse, Model};
use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};

#[derive(serde::Deserialize)]
pub struct VscodeProjectionQuery {
    /// Public Responses ingress URL to stamp into generated client entries.
    #[serde(default = "default_responses_url")]
    url: String,
}

fn default_responses_url() -> String {
    "/v1/responses".to_string()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct VscodeModelConfig {
    id: String,
    name: String,
    tool_calling: bool,
    url: String,
    vision: bool,
}

#[derive(serde::Serialize)]
struct VscodeModelConfigList {
    models: Vec<VscodeModelConfig>,
}

fn vscode_model_eligible(model: &Model) -> bool {
    model
        .capabilities
        .as_ref()
        .and_then(|profile| {
            profile.operation(
                crate::capabilities::Protocol::OpenaiResponses,
                crate::capabilities::ProtocolOperation::Create,
            )
        })
        .is_none_or(|operation| operation.mode != crate::capabilities::DeliveryMode::Unsupported)
}

/// Reusable VS Code model projection. Deployment-specific headers remain the
/// caller's configuration; only capability-derived fields are generated here.
pub async fn project_models_for_vscode(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Query(query): Query<VscodeProjectionQuery>,
) -> Result<Json<impl serde::Serialize>, (StatusCode, Json<ErrorResponse>)> {
    let models = list_models_for_key(&state, &api_key.0.permission).await?;
    let models = models
        .data
        .into_iter()
        .filter(vscode_model_eligible)
        .map(|model| {
            let projection = model
                .capabilities
                .as_ref()
                .map(crate::capabilities::ClientCapabilityProjection::vscode)
                .unwrap_or(crate::capabilities::VscodeCapabilityProjection {
                    tool_calling: false,
                    vision: false,
                });
            VscodeModelConfig {
                name: model.id.clone(),
                id: model.id,
                tool_calling: projection.tool_calling,
                url: query.url.clone(),
                vision: projection.vision,
            }
        })
        .collect();
    Ok(Json(VscodeModelConfigList { models }))
}

pub async fn list_models(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
) -> Result<Json<ListResponse<Model>>, (StatusCode, Json<ErrorResponse>)> {
    list_models_for_key(&state, &api_key.0.permission)
        .await
        .map(Json)
}

/// Filter the returned catalog per caller, after reading the shared backend
/// cache. Both list formats use this path so permissions cannot drift.
async fn list_models_for_key(
    state: &crate::AppState,
    permission: &ApiKeyPermission,
) -> Result<ListResponse<Model>, (StatusCode, Json<ErrorResponse>)> {
    tracing::debug!("Listing models via backend: {}", state.backend.name());

    match state.backend.list_models().await {
        Ok(mut response) => {
            if !permission.is_restricted() {
                return Ok(response);
            }

            let mut allowed_models = Vec::with_capacity(response.data.len());
            for model in response.data {
                let route = match state.backend.resolve(&model.id).await {
                    Ok(route) => route,
                    Err(error) => {
                        tracing::warn!(model = %model.id, %error, "Omitting unresolvable model from catalog");
                        continue;
                    }
                };
                if permission
                    .allows_route(&route.resolution, &model.id)
                    .is_ok()
                {
                    allowed_models.push(model);
                }
            }
            response.data = allowed_models;
            if response.first_id.is_some() {
                response.first_id = response.data.first().map(|model| model.id.clone());
            }
            if response.last_id.is_some() {
                response.last_id = response.data.last().map(|model| model.id.clone());
            }
            Ok(response)
        }
        Err(e) => {
            tracing::error!("Backend error: {}", e);
            Err(ErrorResponse::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "backend_error",
                e.to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, provider: &str) -> Model {
        Model {
            id: id.to_string(),
            object: "model".to_string(),
            created: 0,
            owned_by: provider.to_string(),
            capabilities: Some(
                crate::capabilities::resolve(provider, id, None, None)
                    .unwrap()
                    .profile,
            ),
            provider_metadata: Default::default(),
        }
    }

    #[test]
    fn vscode_projection_omits_decision_only_models() {
        assert!(!vscode_model_eligible(&model("jev-latest", "typesafe")));
        assert!(vscode_model_eligible(&model("gpt-5", "openai")));

        let mut unverified = model("custom", "vllm");
        unverified.capabilities = None;
        assert!(vscode_model_eligible(&unverified));
    }
}

pub async fn get_model(
    State(state): State<crate::AppState>,
    api_key: ResolvedApiKey,
    Path(model_id): Path<String>,
) -> Response {
    tracing::debug!(
        "Getting model: {} via backend: {}",
        model_id,
        state.backend.name()
    );

    let permission = &api_key.0.permission;
    if permission.is_restricted() {
        let route = match state.backend.resolve(&model_id).await {
            Ok(route) => route,
            Err(error) => return crate::handlers::backend_error_response(&error),
        };
        if let Err(denied) = permission.allows_route(&route.resolution, &model_id) {
            return crate::handlers::forbidden_response(denied.to_string());
        }
    }

    match state.backend.get_model(&model_id).await {
        Ok(model) => Json(model).into_response(),
        Err(e) => {
            // Keep the backend's status: an unknown or unavailable model is a
            // 404, not a router failure.
            tracing::error!("Backend error: {}", e);
            crate::handlers::backend_error_response(&e)
        }
    }
}

/// Model-route deletion is an administrative operation exposed only through
/// ModelRouteService/DeleteModelRoute. Never mutate routes through inference auth.
pub async fn delete_model() -> Response {
    crate::handlers::not_implemented(
        "HTTP model deletion; use the authorized ModelRouteService/DeleteModelRoute admin RPC",
    )
}
