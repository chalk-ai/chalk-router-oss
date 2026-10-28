use crate::types::File;
use axum::{
    extract::{Json, Path, Query},
    http::StatusCode,
    response::Response,
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListFilesQuery {
    pub purpose: Option<String>,
}

pub async fn list_files(Query(query): Query<ListFilesQuery>) -> Response {
    tracing::debug!("File listing requested with purpose: {:?}", query.purpose);
    super::not_implemented("File listing")
}

/// No upload opt-in exists until file-specific grants, bounded storage/request
/// admission, ownership, deletion, and retention are implemented together.
/// Do not extract Multipart: reject without reading or buffering the body.
pub async fn upload_file() -> Response {
    super::not_implemented(
        "File uploads (file authorization, quotas, and retention are not implemented)",
    )
}

pub async fn get_file(Path(file_id): Path<String>) -> Result<Json<File>, StatusCode> {
    tracing::debug!("Getting file: {}", file_id);

    Err(StatusCode::NOT_FOUND)
}

pub async fn delete_file(Path(file_id): Path<String>) -> Response {
    tracing::debug!("File deletion requested: {}", file_id);
    super::not_implemented("File deletion")
}

pub async fn download_file(Path(file_id): Path<String>) -> Response {
    tracing::debug!("File download requested: {}", file_id);
    super::not_implemented("File download")
}
