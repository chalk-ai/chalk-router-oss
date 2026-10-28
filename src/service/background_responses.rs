use std::sync::Arc;

use super::{
    pagination::{parse_stable_cursor, resolve_limit},
    ServiceError,
};
use crate::background_responses::{BackgroundResponsePage, BackgroundResponses};

const MAX_LIST_LIMIT: i64 = 100;

pub struct BackgroundResponseService {
    runtime: Arc<BackgroundResponses>,
}

impl BackgroundResponseService {
    pub fn new(runtime: Arc<BackgroundResponses>) -> Self {
        Self { runtime }
    }

    pub async fn list(
        &self,
        phase: Option<&str>,
        cursor: Option<&str>,
        limit: Option<i32>,
    ) -> Result<BackgroundResponsePage, ServiceError> {
        let phase = phase.filter(|phase| !phase.is_empty());
        if phase.is_some_and(|phase| {
            !matches!(
                phase,
                "submitting" | "active" | "terminal" | "unresolved" | "settled"
            )
        }) {
            return Err(ServiceError::invalid_field(
                "phase",
                "unknown background response phase",
            ));
        }
        let cursor = parse_stable_cursor(cursor)?
            .map(|cursor| {
                uuid::Uuid::parse_str(&cursor.id)
                    .map(|event_id| (cursor.created_at, event_id))
                    .map_err(|_| ServiceError::invalid_field("cursor", "not a valid cursor"))
            })
            .transpose()?;
        self.runtime
            .list_jobs(phase, cursor, resolve_limit(limit)?.min(MAX_LIST_LIMIT))
            .await
    }
}
