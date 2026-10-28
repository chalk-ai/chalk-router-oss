#![allow(clippy::result_large_err)] // tonic::Status is the gRPC wire error

use fieldmask::{Maskable, SelfMaskable};
use tonic::{Request, Response, Status};

use crate::capabilities::CapabilityPatch;
use crate::provider_connections::{
    ConnectionInput, ConnectionMetadata, ConnectionUpdate, CredentialState, ExposurePolicy,
};
use crate::provider_registry;
use crate::service::provider_connections::{DraftInput, TestTarget};
use crate::AppState;

use super::conv;
use super::field_mask;
use super::proto;
use super::proto::provider_connection_service_server::ProviderConnectionService;

pub struct ProviderConnectionServer {
    pub state: AppState,
}

#[derive(Debug, Default, Maskable, SelfMaskable)]
struct ConnectionPatch {
    name: String,
    base_url: Option<String>,
    prefix: Option<String>,
    /// Proto enum ordinal; validated when converting to [`ExposurePolicy`].
    exposure: i32,
    routing_enabled: bool,
    api_key: String,
    declared_capabilities: Option<String>,
}

#[tonic::async_trait]
impl ProviderConnectionService for ProviderConnectionServer {
    async fn list_provider_connections(
        &self,
        req: Request<proto::ListProviderConnectionsRequest>,
    ) -> Result<Response<proto::ListProviderConnectionsResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let (connections, next_cursor) = state
            .provider_connection_service()
            .list_page(body.limit, body.cursor)
            .await?;
        Ok(Response::new(proto::ListProviderConnectionsResponse {
            connections: connections.iter().map(metadata_to_proto).collect(),
            next_cursor,
        }))
    }

    async fn list_provider_connection_models(
        &self,
        req: Request<proto::ListProviderConnectionModelsRequest>,
    ) -> Result<Response<proto::ListProviderConnectionModelsResponse>, Status> {
        let state = self.state.for_request(&req);
        let filters = req.into_inner().filters.unwrap_or_default();
        let models = state
            .backend
            .list_provider_connection_models(&filters.connection_ids)
            .await
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(proto::ListProviderConnectionModelsResponse {
            models: models
                .into_iter()
                .map(|model| proto::ProviderConnectionModel {
                    public_model_id: model.public_model_id,
                    upstream_model_id: model.upstream_model_id,
                    connection_id: model.connection_id,
                    capabilities: model.capabilities.as_ref().and_then(to_prost_struct),
                })
                .collect(),
        }))
    }

    async fn create_provider_connection(
        &self,
        req: Request<proto::CreateProviderConnectionRequest>,
    ) -> Result<Response<proto::CreateProviderConnectionResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let exposure = exposure_from_proto(body.exposure)
            .ok_or_else(|| Status::invalid_argument("exposure must be specified"))?;
        let input = ConnectionInput {
            name: body.name,
            provider_kind: body.provider_kind,
            base_url: body.base_url,
            prefix: body.prefix,
            exposure,
            routing_enabled: body.routing_enabled,
            declared_capabilities: body
                .declared_capabilities
                .map(capability_patch_from_struct)
                .transpose()?,
            api_key: body.api_key,
        };
        let created = state.provider_connection_service().create(input).await?;
        Ok(Response::new(proto::CreateProviderConnectionResponse {
            connection: Some(metadata_to_proto(&created)),
        }))
    }

    async fn update_provider_connection(
        &self,
        req: Request<proto::UpdateProviderConnectionRequest>,
    ) -> Result<Response<proto::UpdateProviderConnectionResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        let op = body.update.unwrap_or_default();
        let mut populated = Vec::new();
        if op.name.is_some() {
            populated.push("name");
        }
        if op.base_url.is_some() {
            populated.push("base_url");
        }
        if op.prefix.is_some() {
            populated.push("prefix");
        }
        if op.exposure.is_some() {
            populated.push("exposure");
        }
        if op.routing_enabled.is_some() {
            populated.push("routing_enabled");
        }
        if op.api_key.is_some() {
            populated.push("api_key");
        }
        if op.declared_capabilities.is_some() {
            populated.push("declared_capabilities");
        }

        let resolved = field_mask::resolve(&body.update_mask, &populated)?;

        let upd = if resolved.is_noop() {
            ConnectionUpdate::default()
        } else {
            let has_base_url = op.base_url.is_some();
            let has_prefix = op.prefix.is_some();
            let has_declared_capabilities = op.declared_capabilities.is_some();
            let mut target = ConnectionPatch::default();
            let source = ConnectionPatch {
                name: field_mask::trim_nonempty(op.name).unwrap_or_default(),
                base_url: op.base_url,
                prefix: op.prefix,
                exposure: op.exposure.unwrap_or_default(),
                routing_enabled: op.routing_enabled.unwrap_or_default(),
                api_key: field_mask::trim_nonempty(op.api_key).unwrap_or_default(),
                declared_capabilities: op
                    .declared_capabilities
                    .map(prost_struct_to_json)
                    .map(|value| value.to_string()),
            };
            field_mask::apply(&resolved, &mut target, source)?;

            field_mask::reject_if_blank(&resolved, "name", &target.name)?;
            field_mask::reject_if_blank(&resolved, "api_key", &target.api_key)?;

            let exposure = if resolved.contains("exposure") {
                Some(exposure_from_proto(target.exposure).ok_or_else(|| {
                    Status::invalid_argument(
                        "exposure must be a valid ExposurePolicy when present in update_mask",
                    )
                })?)
            } else {
                None
            };

            let declared_capabilities = match field_mask::clearable_update(
                &resolved,
                "declared_capabilities",
                has_declared_capabilities,
                target.declared_capabilities,
            ) {
                Some(Some(value)) => Some(Some(capability_patch_from_json(&value)?)),
                Some(None) => Some(None),
                None => None,
            };

            ConnectionUpdate {
                name: resolved.contains("name").then_some(target.name),
                base_url: field_mask::clearable_update(
                    &resolved,
                    "base_url",
                    has_base_url,
                    target.base_url,
                ),
                prefix: field_mask::clearable_update(
                    &resolved,
                    "prefix",
                    has_prefix,
                    target.prefix,
                ),
                exposure,
                routing_enabled: resolved
                    .contains("routing_enabled")
                    .then_some(target.routing_enabled),
                api_key: resolved.contains("api_key").then_some(target.api_key),
                declared_capabilities,
            }
        };

        let updated = state
            .provider_connection_service()
            .update(&body.id, upd)
            .await?;
        Ok(Response::new(proto::UpdateProviderConnectionResponse {
            connection: Some(metadata_to_proto(&updated)),
        }))
    }

    async fn delete_provider_connection(
        &self,
        req: Request<proto::DeleteProviderConnectionRequest>,
    ) -> Result<Response<proto::DeleteProviderConnectionResponse>, Status> {
        let state = self.state.for_request(&req);
        let body = req.into_inner();
        if body.id.is_empty() {
            return Err(Status::invalid_argument("id must be non-empty"));
        }
        let outcome = state.provider_connection_service().delete(&body.id).await?;

        // Concrete policy references are warning-only; the delete has already
        // succeeded (or the id was already gone).
        let warnings = if outcome.deleted {
            collect_route_policy_warnings(&state, &body.id).await
        } else {
            Vec::new()
        };

        Ok(Response::new(proto::DeleteProviderConnectionResponse {
            id: body.id,
            warnings,
        }))
    }

    async fn test_provider_connection(
        &self,
        req: Request<proto::TestProviderConnectionRequest>,
    ) -> Result<Response<proto::TestProviderConnectionResponse>, Status> {
        let state = self.state.for_request(&req);
        let target = test_target_from_proto(req.into_inner())?;

        let outcome = state.provider_connection_service().test(target).await;
        Ok(Response::new(proto::TestProviderConnectionResponse {
            ok: outcome.ok,
            error: outcome.error,
            discovered_models: outcome.discovered_models,
            latency: outcome.latency_ms.map(conv::duration_from_millis),
        }))
    }
}

fn test_target_from_proto(
    body: proto::TestProviderConnectionRequest,
) -> Result<TestTarget, Status> {
    use proto::test_provider_connection_request::Target;
    let from_draft = |draft: proto::ProviderConnectionDraft| DraftInput {
        provider_kind: draft.provider_kind,
        base_url: draft.base_url,
        prefix: draft.prefix,
        exposure: exposure_from_proto(draft.exposure).unwrap_or(ExposurePolicy::Dynamic),
        api_key: draft.api_key,
    };
    match body.target {
        Some(Target::ConnectionId(id)) => Ok(TestTarget::Existing(id)),
        Some(Target::Draft(draft)) => Ok(TestTarget::Draft(from_draft(draft))),
        Some(Target::SavedDraft(saved)) => {
            if saved.connection_id.trim().is_empty() {
                return Err(Status::invalid_argument("connection_id must be set"));
            }
            let draft = saved
                .draft
                .ok_or_else(|| Status::invalid_argument("draft must be set"))?;
            Ok(TestTarget::SavedDraft {
                connection_id: saved.connection_id,
                draft: from_draft(draft),
            })
        }
        None => Err(Status::invalid_argument("target must be set")),
    }
}

/// Best-effort post-delete warnings for concrete policy targets. Default route
/// references are already blocked by the service's deletion guard.
async fn collect_route_policy_warnings(state: &AppState, connection_id: &str) -> Vec<String> {
    match state.model_route_store.list().await {
        Ok(routes) => routes
            .into_iter()
            .filter(|route| {
                route
                    .routing
                    .as_ref()
                    .is_some_and(|grid| grid.references_connection(connection_id))
            })
            .map(|route| {
                format!(
                    "Route '{}' policy references deleted connection '{}'",
                    route.public_model_id, connection_id
                )
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "route policies unavailable; skipping deletion warnings");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// proto <-> domain conversions (local to this service)
// ---------------------------------------------------------------------------

/// Map a proto `ExposurePolicy` i32 to the domain enum. `Unspecified` (and any
/// unknown value) maps to `None` so callers can treat it as absent/invalid.
fn exposure_from_proto(v: i32) -> Option<ExposurePolicy> {
    match proto::ExposurePolicy::try_from(v) {
        Ok(proto::ExposurePolicy::Dynamic) => Some(ExposurePolicy::Dynamic),
        Ok(proto::ExposurePolicy::Unlisted) => Some(ExposurePolicy::Unlisted),
        Ok(proto::ExposurePolicy::Explicit) => Some(ExposurePolicy::Explicit),
        _ => None,
    }
}

fn exposure_to_proto(e: ExposurePolicy) -> i32 {
    let v = match e {
        ExposurePolicy::Dynamic => proto::ExposurePolicy::Dynamic,
        ExposurePolicy::Unlisted => proto::ExposurePolicy::Unlisted,
        ExposurePolicy::Explicit => proto::ExposurePolicy::Explicit,
    };
    v as i32
}

/// Whether the connection has a usable endpoint: an explicit base URL, or a
/// built-in kind with a code-owned default (every built-in except vLLM, which
/// has no universal endpoint).
fn endpoint_present(kind: &str, base_url: Option<&str>) -> bool {
    if base_url.map(|b| !b.trim().is_empty()).unwrap_or(false) {
        return true;
    }
    provider_registry::is_builtin(kind) && kind != "vllm"
}

/// Connections that can legitimately route without a stored credential.
fn can_be_keyless(kind: &str, base_url: Option<&str>) -> bool {
    kind == "vllm"
        || kind == provider_registry::OPENAI_COMPATIBLE_KIND
        || (kind == "bedrock" && crate::backend::bedrock::is_runtime_connection(base_url))
}

/// Whether the connection's credential state is usable for its provider kind.
fn credential_is_usable(kind: &str, base_url: Option<&str>, state: CredentialState) -> bool {
    match state {
        CredentialState::Present => true,
        CredentialState::Keyless => can_be_keyless(kind, base_url),
        CredentialState::Unknown | CredentialState::Unreadable => false,
    }
}

/// Convert a non-secret metadata projection for list responses. Credential
/// health comes from the store's updated_at-aware cache, so repeated admin
/// polling neither fetches nor decrypts unchanged envelopes.
fn metadata_to_proto(connection: &ConnectionMetadata) -> proto::ProviderConnection {
    let configured =
        credential_is_usable(
            &connection.provider_kind,
            connection.base_url.as_deref(),
            connection.credential_state,
        ) && endpoint_present(&connection.provider_kind, connection.base_url.as_deref());
    proto::ProviderConnection {
        id: connection.id.clone(),
        name: connection.name.clone(),
        provider_kind: connection.provider_kind.clone(),
        base_url: connection.base_url.clone(),
        prefix: connection.prefix.clone(),
        exposure: exposure_to_proto(connection.exposure),
        routing_enabled: connection.routing_enabled,
        state: Some(proto::ConnectionState {
            configured,
            unreadable: connection.credential_state.unreadable(),
            healthy: None,
        }),
        created_at: Some(conv::ts_to_proto(connection.created_at)),
        updated_at: Some(conv::ts_to_proto(connection.updated_at)),
        declared_capabilities: connection
            .declared_capabilities
            .as_ref()
            .and_then(to_prost_struct),
    }
}

fn capability_patch_from_struct(value: prost_types::Struct) -> Result<CapabilityPatch, Status> {
    serde_json::from_value(prost_struct_to_json(value)).map_err(|error| {
        Status::invalid_argument(format!("invalid declared_capabilities: {error}"))
    })
}

fn capability_patch_from_json(value: &str) -> Result<CapabilityPatch, Status> {
    serde_json::from_str(value).map_err(|error| {
        Status::invalid_argument(format!("invalid declared_capabilities: {error}"))
    })
}

fn to_prost_struct(value: &impl serde::Serialize) -> Option<prost_types::Struct> {
    match serde_json::to_value(value).ok()? {
        serde_json::Value::Object(fields) => Some(prost_types::Struct {
            fields: fields
                .into_iter()
                .map(|(key, value)| (key, json_value_to_prost(value)))
                .collect(),
        }),
        _ => None,
    }
}

fn prost_struct_to_json(value: prost_types::Struct) -> serde_json::Value {
    serde_json::Value::Object(
        value
            .fields
            .into_iter()
            .map(|(key, value)| (key, prost_value_to_json(value)))
            .collect(),
    )
}

pub(crate) fn prost_value_to_json(value: prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match value.kind {
        None | Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::NumberValue(value)) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(value)) => serde_json::Value::String(value),
        Some(Kind::BoolValue(value)) => serde_json::Value::Bool(value),
        Some(Kind::StructValue(value)) => prost_struct_to_json(value),
        Some(Kind::ListValue(value)) => {
            serde_json::Value::Array(value.values.into_iter().map(prost_value_to_json).collect())
        }
    }
}

pub(crate) fn json_value_to_prost(value: serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(value) => Kind::BoolValue(value),
        serde_json::Value::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or_default()),
        serde_json::Value::String(value) => Kind::StringValue(value),
        serde_json::Value::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_value_to_prost).collect(),
        }),
        serde_json::Value::Object(fields) => Kind::StructValue(prost_types::Struct {
            fields: fields
                .into_iter()
                .map(|(key, value)| (key, json_value_to_prost(value)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_connections::{
        InMemoryProviderConnectionStore, ProviderConnectionStore, KEYLESS_PLACEHOLDER,
    };

    #[test]
    fn saved_draft_target_preserves_complete_draft_and_optional_clears() {
        let target = test_target_from_proto(proto::TestProviderConnectionRequest {
            target: Some(proto::test_provider_connection_request::Target::SavedDraft(
                proto::SavedProviderConnectionDraft {
                    connection_id: "saved-id".to_string(),
                    draft: Some(proto::ProviderConnectionDraft {
                        provider_kind: "openai".to_string(),
                        exposure: proto::ExposurePolicy::Explicit as i32,
                        ..Default::default()
                    }),
                },
            )),
        })
        .unwrap();
        let TestTarget::SavedDraft {
            connection_id,
            draft,
        } = target
        else {
            unreachable!("expected saved draft")
        };
        assert_eq!(connection_id, "saved-id");
        assert_eq!(draft.provider_kind, "openai");
        assert_eq!(draft.base_url, None);
        assert_eq!(draft.prefix, None);
        assert_eq!(draft.exposure, ExposurePolicy::Explicit);
        assert!(draft.api_key.is_empty());
    }

    #[test]
    fn saved_draft_target_requires_id_and_draft() {
        for saved in [
            proto::SavedProviderConnectionDraft {
                connection_id: " ".to_string(),
                draft: Some(proto::ProviderConnectionDraft::default()),
            },
            proto::SavedProviderConnectionDraft {
                connection_id: "saved-id".to_string(),
                draft: None,
            },
        ] {
            let result = test_target_from_proto(proto::TestProviderConnectionRequest {
                target: Some(proto::test_provider_connection_request::Target::SavedDraft(
                    saved,
                )),
            });
            assert!(matches!(result, Err(error) if error.code() == tonic::Code::InvalidArgument));
        }
    }

    #[tokio::test]
    async fn keyless_seeded_openai_compatible_connection_is_configured() {
        // F4: a seeded Chalk endpoint stores the keyless placeholder as its
        // api_key alongside a base_url. It routes fine, so a Readable view must
        // report configured rather than "not configured".
        let store = InMemoryProviderConnectionStore::new();
        let created = store
            .create(ConnectionInput {
                name: "Chalk".to_string(),
                provider_kind: provider_registry::OPENAI_COMPATIBLE_KIND.to_string(),
                base_url: Some("https://api.chalk.ai/v1".to_string()),
                prefix: Some("chalk-endpoint/".to_string()),
                exposure: ExposurePolicy::Unlisted,
                routing_enabled: true,
                declared_capabilities: None,
                api_key: KEYLESS_PLACEHOLDER.to_string(),
            })
            .await
            .unwrap();

        let proto = metadata_to_proto(&created);

        let state = proto.state.expect("state should be present");
        assert!(
            state.configured,
            "a resolvable keyless endpoint is configured"
        );
        assert!(!state.unreadable, "a Readable connection is not unreadable");
    }

    #[test]
    fn unreadable_metadata_stays_unreadable_in_mutation_responses() {
        let now = chrono::Utc::now();
        let connection = ConnectionMetadata {
            id: "connection-id".to_string(),
            name: "OpenAI".to_string(),
            provider_kind: "openai".to_string(),
            base_url: None,
            prefix: Some("openai/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: None,
            credential_state: CredentialState::Unreadable,
            created_at: now,
            updated_at: now,
        };

        let proto = metadata_to_proto(&connection);
        let state = proto.state.unwrap();
        assert!(!state.configured);
        assert!(state.unreadable);
    }

    #[test]
    fn metadata_includes_only_the_raw_capability_declaration() {
        let declared = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "endpoint": {
                "interfaces": {
                    "openai_responses": {
                        "operations": {"create": {"support": "unsupported"}}
                    }
                }
            }
        }))
        .unwrap();
        let now = chrono::Utc::now();
        let connection = ConnectionMetadata {
            id: "connection-id".to_string(),
            name: "OpenAI".to_string(),
            provider_kind: "openai".to_string(),
            base_url: None,
            prefix: Some("openai/".to_string()),
            exposure: ExposurePolicy::Dynamic,
            routing_enabled: true,
            declared_capabilities: Some(declared),
            credential_state: CredentialState::Present,
            created_at: now,
            updated_at: now,
        };

        let proto = metadata_to_proto(&connection);
        assert!(proto.declared_capabilities.is_some());
    }

    #[test]
    fn keyless_rule_holds_across_kinds() {
        // Runtime uses the process AWS credential chain, while vLLM and
        // openai-compatible connections may run unauthenticated.
        assert!(credential_is_usable(
            "vllm",
            Some("https://vllm.example/v1"),
            CredentialState::from_api_key(KEYLESS_PLACEHOLDER)
        ));
        assert!(credential_is_usable(
            provider_registry::OPENAI_COMPATIBLE_KIND,
            Some("https://compatible.example/v1"),
            CredentialState::from_api_key(KEYLESS_PLACEHOLDER)
        ));
        assert!(credential_is_usable(
            "bedrock",
            Some("https://bedrock-runtime.us-west-2.amazonaws.com"),
            CredentialState::from_api_key(KEYLESS_PLACEHOLDER)
        ));
        assert!(!credential_is_usable(
            "bedrock",
            Some("https://bedrock-mantle.us-west-2.api.aws/v1"),
            CredentialState::from_api_key(KEYLESS_PLACEHOLDER)
        ));
        assert!(credential_is_usable(
            "openai",
            None,
            CredentialState::from_api_key("sk-real")
        ));
        assert!(!credential_is_usable(
            "openai",
            None,
            CredentialState::from_api_key(KEYLESS_PLACEHOLDER)
        ));
    }
}
