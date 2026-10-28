//! Placeholder for a trained lightweight classifier judge.
//!
//! The RouteLLM/Anyscale offline-trained path: a small BERT-style head that
//! scores difficulty in <10ms with no per-request LLM call. It is a drop-in
//! [`Judge`] once we have usage data to train on; kept as a stub so the trait
//! and wiring are exercised without pulling in an inference runtime yet.

use async_trait::async_trait;

use super::{Judge, JudgeError, JudgeRequest, JudgeVerdict};

/// Not yet implemented — always errors so the caller fails open. Selecting the
/// classifier backend before it ships leaves the judge effectively disabled.
pub struct ClassifierJudge;

#[async_trait]
impl Judge for ClassifierJudge {
    async fn score(&self, _req: &JudgeRequest<'_>) -> Result<JudgeVerdict, JudgeError> {
        Err(JudgeError::Backend(
            "classifier judge backend not implemented".to_string(),
        ))
    }
}
