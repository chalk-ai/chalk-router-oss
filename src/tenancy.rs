//! Loaded environment runtimes and request-bound runtime selection.
//!
//! `AppState` remains the environment bundle consumed by existing services.
//! A cluster owns several bundles; authentication selects one and inserts it
//! into request extensions. Neither handlers nor tonic adapters select tenants.

use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};

use axum::{extract::FromRequestParts, http::request::Parts};

use crate::AppState;

#[derive(Clone)]
pub struct RequestRuntime(pub AppState);

/// The environment runtimes this process has loaded. Which environments may be
/// served, and by whom, is decided by authentication, never by this map.
/// Runtimes are never evicted: streams, quota leases and background jobs hold
/// them for as long as the process runs.
pub struct EnvironmentRegistry {
    runtimes: RwLock<BTreeMap<String, AppState>>,
}

impl EnvironmentRegistry {
    pub fn new(runtimes: Vec<AppState>) -> Result<Arc<Self>, String> {
        let mut indexed = BTreeMap::new();
        for runtime in runtimes {
            if runtime.environment_id.trim().is_empty()
                || indexed
                    .insert(runtime.environment_id.clone(), runtime)
                    .is_some()
            {
                return Err("environment runtimes contain an empty or duplicate ID".into());
            }
        }
        Ok(Arc::new(Self {
            runtimes: RwLock::new(indexed),
        }))
    }

    pub fn get(&self, environment: &str) -> Option<AppState> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(environment)
            .cloned()
    }

    /// Adds a runtime unless one is already loaded for its environment, and
    /// returns whichever runtime the registry now serves for it.
    pub fn insert(&self, runtime: AppState) -> AppState {
        self.runtimes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(runtime.environment_id.clone())
            .or_insert(runtime)
            .clone()
    }

    /// A snapshot of the loaded runtimes, in environment order.
    pub fn runtimes(&self) -> Vec<AppState> {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.runtimes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Environment-aware replacement for Axum's process-state extractor. The
/// fallback preserves single-environment deployments and direct test routers.
pub struct RequestState<T>(pub T);

#[axum::async_trait]
impl FromRequestParts<AppState> for RequestState<AppState> {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<RequestRuntime>()
                .map(|runtime| runtime.0.clone())
                .unwrap_or_else(|| state.clone()),
        ))
    }
}

impl AppState {
    /// Scope handles and object paths without changing encryption envelopes or key hashes.
    pub fn bind_environment_resources(
        mut self,
        hasher: &crate::hashing::KeyHasher,
        legacy_environment: Option<&str>,
    ) -> Self {
        self.response_locator = hasher.response_locator_codec().for_environment(
            &self.environment_id,
            legacy_environment == Some(self.environment_id.as_str()),
        );
        self.object_store = self
            .object_store
            .as_ref()
            .map(|store| Arc::new(store.for_environment(&self.environment_id)));
        self
    }

    /// Tonic preserves HTTP extensions, including the runtime selected by auth.
    pub fn for_request<T>(&self, request: &tonic::Request<T>) -> Self {
        request
            .extensions()
            .get::<RequestRuntime>()
            .map(|runtime| runtime.0.clone())
            .unwrap_or_else(|| self.clone())
    }
}
