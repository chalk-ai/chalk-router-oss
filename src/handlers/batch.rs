use crate::types::{Batch, CreateBatchRequest};
use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i32>,
    pub after: Option<String>,
}

pub async fn create_batch(Json(_req): Json<CreateBatchRequest>) -> Response {
    tracing::debug!("Batch creation requested");
    super::not_implemented("Batch jobs")
}

pub async fn list_batches(Query(_query): Query<ListQuery>) -> Response {
    tracing::debug!("Batch listing requested");
    super::not_implemented("Batch jobs")
}

pub async fn get_batch(Path(batch_id): Path<String>) -> Result<Json<Batch>, StatusCode> {
    tracing::debug!("Getting batch: {}", batch_id);

    Err(StatusCode::NOT_FOUND)
}

pub async fn cancel_batch(Path(batch_id): Path<String>) -> Result<Json<Batch>, StatusCode> {
    tracing::debug!("Cancelling batch: {}", batch_id);

    Err(StatusCode::NOT_FOUND)
}
