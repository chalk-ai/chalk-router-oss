use crate::{DebugCompletionEntry, DebugLog};

pub struct DebugService {
    debug_log: DebugLog,
}

impl DebugService {
    pub fn new(debug_log: DebugLog) -> Self {
        Self { debug_log }
    }

    pub async fn recent_completions(&self) -> Vec<DebugCompletionEntry> {
        self.debug_log.recent().await
    }
}
