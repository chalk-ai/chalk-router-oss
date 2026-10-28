//! Which Chalk environments a cluster router may serve, and which team owns each.
//!
//! Cluster authentication asks the directory before it selects a runtime: a
//! credential naming an environment the directory does not confirm is denied,
//! and team-scoped credentials must match the owning team it reports.
use std::{collections::BTreeMap, sync::Arc};

/// A live environment, as the directory reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentRecord {
    pub id: String,
    pub team_id: String,
}

/// The directory could not answer. Distinct from "no such environment" so that
/// callers can return 503 instead of denying a credential that may be valid.
#[derive(Debug, Clone)]
pub struct DirectoryUnavailable(pub String);

impl std::fmt::Display for DirectoryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "environment directory unavailable: {}", self.0)
    }
}

impl std::error::Error for DirectoryUnavailable {}

#[async_trait::async_trait]
pub trait EnvironmentDirectory: Send + Sync {
    /// `Ok(None)` when the environment does not exist or this router must not
    /// serve it (archived, suspended, or on another cluster).
    async fn lookup(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvironmentRecord>, DirectoryUnavailable>;
}

/// A fixed environment-to-team map, for tests and local cluster runs.
pub struct StaticEnvironmentDirectory {
    environments: BTreeMap<String, EnvironmentRecord>,
}

impl StaticEnvironmentDirectory {
    pub fn new<E, T>(environments: impl IntoIterator<Item = (E, T)>) -> Arc<Self>
    where
        E: Into<String>,
        T: Into<String>,
    {
        Arc::new(Self {
            environments: environments
                .into_iter()
                .map(|(id, team_id)| {
                    let id = id.into();
                    let record = EnvironmentRecord {
                        id: id.clone(),
                        team_id: team_id.into(),
                    };
                    (id, record)
                })
                .collect(),
        })
    }
}

#[async_trait::async_trait]
impl EnvironmentDirectory for StaticEnvironmentDirectory {
    async fn lookup(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvironmentRecord>, DirectoryUnavailable> {
        Ok(self.environments.get(environment_id).cloned())
    }
}
