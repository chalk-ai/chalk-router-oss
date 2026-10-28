//! Run every loaded environment's workers, including environments loaded
//! after startup.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use futures::{stream, StreamExt};

use crate::{
    config::QuotaBackendKind,
    environment_runtime::{PendingJobRecovery, RECOVERY_INTERVAL},
    tenancy::EnvironmentRegistry,
    AppState,
};

/// One background-response worker per loaded environment.
#[derive(Default)]
pub struct BackgroundWorkers {
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

impl BackgroundWorkers {
    /// Start the environment's worker unless it is already running.
    pub fn ensure_started(&self, state: &AppState) {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(state.environment_id.clone())
            .or_insert_with(|| state.background_responses.start(state.clone()));
    }

    fn take_all(&self) -> Vec<tokio::task::JoinHandle<()>> {
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, task)| task)
            .collect()
    }
}

impl Drop for BackgroundWorkers {
    fn drop(&mut self) {
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            task.abort();
        }
    }
}

pub struct ClusterWorkers {
    registry: Arc<EnvironmentRegistry>,
    background: Arc<BackgroundWorkers>,
    maintenance: Option<tokio::task::JoinHandle<()>>,
    recovery: Option<tokio::task::JoinHandle<()>>,
}

impl ClusterWorkers {
    /// Start background workers for the loaded environments and a maintenance
    /// loop that refreshes exporter settings, cleans up quota leases and starts
    /// workers for environments loaded later. In cluster mode a separate task
    /// resumes environments with pending background jobs, starting right away:
    /// metadata API lookups can be slow, so they never hold up startup or
    /// maintenance.
    pub fn start(
        registry: Arc<EnvironmentRegistry>,
        background: Arc<BackgroundWorkers>,
        quota_kind: QuotaBackendKind,
        recovery: Option<Arc<PendingJobRecovery>>,
    ) -> Self {
        for state in registry.runtimes() {
            background.ensure_started(&state);
        }
        let maintenance = {
            let registry = registry.clone();
            let background = background.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(10));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut last_cleanup = Instant::now();
                let mut warned: HashMap<String, Instant> = HashMap::new();
                loop {
                    interval.tick().await;
                    let runtimes = registry.runtimes();
                    for state in &runtimes {
                        background.ensure_started(state);
                    }
                    let outcomes: Vec<_> = stream::iter(runtimes.clone())
                        .map(|state| async move {
                            let error = state.telemetry_settings.refresh().await.err();
                            (state.environment_id, error)
                        })
                        .buffer_unordered(4)
                        .collect()
                        .await;
                    for (environment, error) in outcomes {
                        match error {
                            None => {
                                if warned.remove(&environment).is_some() {
                                    tracing::info!(environment_id = %environment, "tenant exporter refresh recovered");
                                }
                            }
                            Some(error) => {
                                let now = Instant::now();
                                if warned.get(&environment).is_none_or(|last| {
                                    now.duration_since(*last) >= Duration::from_secs(3600)
                                }) {
                                    tracing::warn!(environment_id = %environment, %error, "tenant exporter refresh failed");
                                    warned.insert(environment, now);
                                }
                            }
                        }
                    }
                    if last_cleanup.elapsed() >= Duration::from_secs(60) {
                        last_cleanup = Instant::now();
                        cleanup_quotas(&runtimes, quota_kind).await;
                    }
                }
            })
        };
        let recovery = recovery.map(|recovery| {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(RECOVERY_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    recovery.run().await;
                }
            })
        });
        Self {
            registry,
            background,
            maintenance: Some(maintenance),
            recovery,
        }
    }

    pub async fn shutdown(mut self) {
        // Stop everything that can load an environment or start a worker
        // before collecting the workers.
        for task in [self.maintenance.take(), self.recovery.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
            let _ = task.await;
        }
        for task in self.background.take_all() {
            task.abort();
            let _ = task.await;
        }
        for state in self.registry.runtimes() {
            if let Some(tracker) = &state.usage_tracker {
                tracker.shutdown().await;
            }
        }
    }
}

/// Postgres expiry is global to the shared pool; memory expiry belongs to each
/// tenant's separate runtime. Keep maintenance scoped to that ownership.
pub async fn cleanup_quotas(runtimes: &[AppState], kind: QuotaBackendKind) {
    let count = if kind == QuotaBackendKind::Postgres {
        1
    } else {
        runtimes.len()
    };
    stream::iter(runtimes.iter().take(count)).for_each_concurrent(4, |state| async move {
        if let Err(error) = state.quota_backend.cleanup(500).await {
            tracing::warn!(environment_id = %state.environment_id, %error, "quota cleanup failed");
        }
    }).await;
}

impl Drop for ClusterWorkers {
    fn drop(&mut self) {
        for task in [&self.maintenance, &self.recovery].into_iter().flatten() {
            task.abort();
        }
    }
}
