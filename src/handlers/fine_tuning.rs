use crate::types::{CreateFineTuningJobRequest, FineTuningJob};
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

pub async fn create_fine_tuning_job(Json(_req): Json<CreateFineTuningJobRequest>) -> Response {
    tracing::debug!("Fine-tuning job creation requested");
    super::not_implemented("Fine-tuning")
}

pub async fn list_fine_tuning_jobs(Query(_query): Query<ListQuery>) -> Response {
    tracing::debug!("Fine-tuning job listing requested");
    super::not_implemented("Fine-tuning")
}

pub async fn get_fine_tuning_job(
    Path(job_id): Path<String>,
) -> Result<Json<FineTuningJob>, StatusCode> {
    tracing::debug!("Getting fine-tuning job: {}", job_id);

    Err(StatusCode::NOT_FOUND)
}

pub async fn cancel_fine_tuning_job(
    Path(job_id): Path<String>,
) -> Result<Json<FineTuningJob>, StatusCode> {
    tracing::debug!("Cancelling fine-tuning job: {}", job_id);

    Err(StatusCode::NOT_FOUND)
}
