use crate::types::{Assistant, CreateAssistantRequest};
use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i32>,
    pub order: Option<String>,
    pub after: Option<String>,
    pub before: Option<String>,
}

pub async fn list_assistants(Query(_query): Query<ListQuery>) -> Response {
    tracing::debug!("Assistant listing requested");
    super::not_implemented("Assistants")
}

pub async fn create_assistant(Json(_req): Json<CreateAssistantRequest>) -> Response {
    tracing::debug!("Assistant creation requested");
    super::not_implemented("Assistants")
}

pub async fn get_assistant(
    Path(assistant_id): Path<String>,
) -> Result<Json<Assistant>, StatusCode> {
    tracing::debug!("Getting assistant: {}", assistant_id);

    Err(StatusCode::NOT_FOUND)
}

pub async fn modify_assistant(
    Path(assistant_id): Path<String>,
    Json(_req): Json<CreateAssistantRequest>,
) -> Result<Json<Assistant>, StatusCode> {
    tracing::debug!("Modifying assistant: {}", assistant_id);

    Err(StatusCode::NOT_FOUND)
}

pub async fn delete_assistant(Path(assistant_id): Path<String>) -> Response {
    tracing::debug!("Assistant deletion requested: {}", assistant_id);
    super::not_implemented("Assistants")
}
