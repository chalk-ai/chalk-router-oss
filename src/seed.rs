//! `openai-server seed-connections`: idempotently **insert** provider connections
//! and model routes from a JSON seed file. This is insert-if-absent, **not** an
//! upsert — it never modifies an existing row.
//!
//! The seeder is **insert-if-absent**: a connection whose `name` already exists
//! is skipped, as is a route whose `public_model_id` already exists. It never
//! updates or deletes, so re-running it is safe. Because it never updates, a
//! changed field in the seed file (a rotated `api_key`, a new endpoint/prefix,
//! …) for an *existing* connection or route has **no effect**; the seeder logs a
//! `warn` naming the diverged fields so the silent no-op is visible, but applying
//! the change still requires the Providers UI or the update API. Creates are routed through the
//! service layer (`ProviderConnectionService` / `ModelRouteService`) so a seed
//! file is held to the exact same validation as the gRPC/UI create paths — a
//! genuinely invalid entry (bad prefix, reserved `chalk/`, empty upstream, …)
//! fails the whole seed run rather than writing a misrouting-prone row. Keys are
//! encrypted at rest by the store's `create()` (Postgres) — the seeder never
//! encrypts manually.
//!
//! Seed file format (see `seed-connections --file <path>` /
//! `ROUTER_CONNECTIONS_SEED_FILE`):
//! ```json
//! {
//!   "connections": [
//!     {"name":"openai","kind":"openai","api_key":"sk-..."},
//!     {"name":"chalk-gemma","kind":"openai-compatible","base_url":"https://.../v1",
//!      "api_key":"unused","exposure":"unlisted","prefix":"chalk-gemma/"}
//!   ],
//!   "routes": [
//!     {"public_model_id":"chalk/gemma","connection":"chalk-gemma",
//!      "upstream_model_id":"gemma-3-27b-it","published":true}
//!   ]
//! }
//! ```
//!
//! Defaults: `exposure` = `"dynamic"`; an omitted `api_key` becomes the
//! [`KEYLESS_PLACEHOLDER`]; a built-in kind's `prefix` defaults to its canonical
//! prefix unless its exposure is `explicit`; an `openai-compatible` connection
//! requires a `base_url` (and a `prefix` unless its exposure is `explicit`).
//!
//! `${VAR}` interpolation: before parsing, `${VAR}` placeholders in the file are
//! replaced with the value of environment variable `VAR`, JSON-escaped so a
//! credential containing `"`, `\`, or a newline can't corrupt the document
//! (`$${` is a literal `${`). Put placeholders in JSON string positions
//! (`"api_key":"${ANTHROPIC_API_KEY}"`). This lets a seed file carry credentials
//! by *reference* and hold no plaintext secrets itself — the k8s seed Job injects
//! the values from the `chalk-router-seed-env` Secret via `envFrom`. A
//! referenced-but-unset variable fails the run (fail closed). A file with no
//! placeholders passes through unchanged (literal keys still work).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Deserialize;

use crate::capabilities::CapabilityPatch;
use crate::config::RouterConfig;
use crate::model_routes::{ModelRoute, ModelRouteStore, PostgresModelRouteStore, RouteInput};
use crate::provider_connections::{
    ConnectionInput, ExposurePolicy, PostgresProviderConnectionStore, ProviderConnectionStore,
    StoredConnection, KEYLESS_PLACEHOLDER,
};
use crate::provider_registry::{self, OPENAI_COMPATIBLE_KIND};
use crate::secrets::SecretCipher;
use crate::service::{ModelRouteService, ProviderConnectionService};

/// A parsed seed file.
#[derive(Debug, Deserialize)]
struct SeedFile {
    #[serde(default)]
    connections: Vec<SeedConnection>,
    #[serde(default)]
    routes: Vec<SeedRoute>,
}

/// One connection entry in a seed file.
#[derive(Debug, Deserialize)]
struct SeedConnection {
    name: String,
    kind: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    exposure: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    routing_enabled: Option<bool>,
    #[serde(default)]
    declared_capabilities: Option<CapabilityPatch>,
}

/// One model-route entry in a seed file.
#[derive(Debug, Deserialize)]
struct SeedRoute {
    public_model_id: String,
    /// Name of the connection this route dispatches to (resolved to its id).
    connection: String,
    upstream_model_id: String,
    #[serde(default)]
    published: Option<bool>,
}

/// Read the seed file (from `file_arg` or `ROUTER_CONNECTIONS_SEED_FILE`), open
/// the Postgres stores the same way the serve path does, and apply the seed.
/// Returns a human-readable summary on success.
pub async fn run(file_arg: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let path = file_arg
        .map(str::to_string)
        .or_else(|| {
            std::env::var("ROUTER_CONNECTIONS_SEED_FILE")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .ok_or("no seed file: pass --file <path> or set ROUTER_CONNECTIONS_SEED_FILE")?;

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read seed file '{path}': {e}"))?;
    // Interpolate `${VAR}` placeholders from the environment before parsing, so a
    // seed file can carry provider keys by *reference* (e.g. `"${ANTHROPIC_API_KEY}"`)
    // and hold no plaintext secrets itself — the k8s seed Job supplies the values
    // via `envFrom: chalk-router-seed-env`. A referenced-but-unset var is a hard
    // error (fail closed) rather than silently seeding an empty/placeholder key.
    let raw = expand_env_vars(&raw, |name| std::env::var(name).ok())
        .map_err(|e| format!("seed file '{path}': {e}"))?;
    let seed: SeedFile = serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse seed file '{path}': {e}"))?;

    let config = RouterConfig::from_env();
    let database_url = config
        .database_url
        .ok_or("seed-connections: set ROUTER_DATABASE_URL")?;

    // Store SQL is `chalk_router.`-qualified. Avoid session state here because
    // transaction-pooling proxies may hand the server connection to another
    // client as soon as a transaction ends.
    let connect_options = crate::database::connect_options(&database_url)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect_with(connect_options)
        .await?;

    // Required whenever a DB is configured: keys are encrypted at rest under it.
    let cipher = SecretCipher::from_env()?;
    let connections: Arc<dyn ProviderConnectionStore> = Arc::new(
        PostgresProviderConnectionStore::new(pool.clone(), cipher, config.environment_id.clone()),
    );
    let routes: Arc<dyn ModelRouteStore> = Arc::new(PostgresModelRouteStore::new(
        pool.clone(),
        config.environment_id.clone(),
    ));

    // Route seeding through the service layer so seeds get the exact same
    // validation as the gRPC/UI create paths (prefix format, reserved `chalk/`,
    // canonical/cross-kind checks, api_key rules; and for routes the `chalk/`
    // prefix + non-empty suffix + non-empty upstream). The services wrap the
    // same stores, which still encrypt keys at rest on `create`.
    let connection_service =
        ProviderConnectionService::new(connections.clone(), reqwest::Client::new());
    let route_service = ModelRouteService::new(routes.clone(), connections.clone());

    apply(&connection_service, &route_service, seed).await
}

/// Expand `${VAR}` placeholders in a JSON `input` using `lookup`, returning the
/// substituted string. `${VAR}` is replaced by `lookup("VAR")`, **JSON-escaped**
/// (see [`json_escape`]) so a value containing `"`, `\`, a newline, or any other
/// JSON-sensitive character stays inside its surrounding string literal instead
/// of corrupting the document — placeholders therefore belong in JSON *string*
/// positions (`"api_key": "${ANTHROPIC_API_KEY}"`). A referenced-but-unset
/// variable is an error (fail closed — never substitute an empty string for a
/// missing credential). `$${` is an escape for a literal `${`. Var names are
/// `[A-Za-z0-9_]+`; anything else inside `${…}` is rejected so a malformed
/// placeholder can't slip through as a literal. Text containing no `${` passes
/// through unchanged, so a plain (secret-bearing) seed file with no placeholders
/// still works exactly as before.
fn expand_env_vars(input: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx..];
        if let Some(tail) = after.strip_prefix("$${") {
            // Escaped literal: `$${` -> `${`.
            out.push_str("${");
            rest = tail;
        } else if let Some(tail) = after.strip_prefix("${") {
            let end = tail
                .find('}')
                .ok_or_else(|| "unterminated '${' placeholder".to_string())?;
            let name = &tail[..end];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(format!("invalid placeholder '${{{name}}}'"));
            }
            let value = lookup(name)
                .ok_or_else(|| format!("references unset environment variable '{name}'"))?;
            // JSON-escape so special characters in a credential can't break the
            // surrounding string literal or alter the document's structure.
            out.push_str(&json_escape(&value));
            rest = &tail[end + 1..];
        } else {
            // A lone `$` not starting a placeholder: emit it and continue past it.
            out.push('$');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// JSON-escape a string's *contents* (no surrounding quotes) so it can be
/// substituted into a JSON string literal safely. Uses `serde_json` to get
/// correct handling of quotes, backslashes, control characters, and newlines,
/// then strips the quotes it adds.
fn json_escape(value: &str) -> String {
    let quoted = serde_json::Value::String(value.to_string()).to_string();
    // `Value::String` always serializes as `"…"`; drop the surrounding quotes.
    quoted[1..quoted.len() - 1].to_string()
}

/// Apply a parsed seed via the service layer (insert-if-absent), returning a
/// summary. Factored out so it can be exercised against in-memory stores.
///
/// Idempotency is preserved with a pre-check: a connection whose `name` (or a
/// route whose `public_model_id`) already exists is skipped without touching the
/// service. Everything else is created through the service, which runs the full
/// validation — so a genuinely invalid entry (bad prefix, reserved `chalk/`,
/// empty upstream, …) returns an error that aborts the whole seed run rather
/// than being silently inserted or skipped.
async fn apply(
    connections: &ProviderConnectionService,
    routes: &ModelRouteService,
    seed: SeedFile,
) -> Result<String, Box<dyn std::error::Error>> {
    // Existing connections indexed two ways: `name_to_id` (including ones we
    // create below) resolves the `connection` name each route points at and
    // decides what to skip; `existing_by_name` holds the full stored row so a
    // skipped-but-diverged seed entry can be warned about.
    let stored_conns = connections.list().await?;
    let mut name_to_id: HashMap<String, String> = stored_conns
        .iter()
        .map(|c| (c.name().to_string(), c.id().to_string()))
        .collect();
    let existing_by_name: HashMap<String, StoredConnection> = stored_conns
        .into_iter()
        .map(|c| (c.name().to_string(), c))
        .collect();

    let mut created_conns = 0usize;
    let mut skipped_conns = 0usize;
    for conn in seed.connections {
        if name_to_id.contains_key(&conn.name) {
            // Insert-if-absent: an existing name is skipped, never updated. Warn
            // if the seed diverges from the stored row so the no-op is visible. A
            // same-file duplicate isn't in `existing_by_name` — just skip it.
            match existing_by_name.get(&conn.name) {
                Some(existing) => warn_if_connection_differs(existing, &conn),
                None => tracing::info!(
                    name = %conn.name,
                    "connection already defined earlier in this seed; skipping"
                ),
            }
            skipped_conns += 1;
            continue;
        }
        let name = conn.name.clone();
        let created = connections
            .create(connection_input(conn)?)
            .await
            .map_err(|e| format!("invalid seed connection '{name}': {e}"))?;
        name_to_id.insert(name, created.id);
        created_conns += 1;
    }

    // Tracks public ids that already exist OR were created earlier in THIS run,
    // so a second entry with the same `public_model_id` in one seed file is
    // skipped (mirroring the connection `name_to_id` handling) rather than
    // hitting the uniqueness constraint and failing the whole seed.
    // `existing_routes_by_id` holds the full stored route so a skipped-but-
    // diverged seed entry can be warned about.
    let existing_routes_by_id: HashMap<String, ModelRoute> = routes
        .list()
        .await?
        .into_iter()
        .map(|r| (r.route.public_model_id.clone(), r.route))
        .collect();
    let mut existing_route_ids: HashSet<String> = existing_routes_by_id.keys().cloned().collect();

    let mut created_routes = 0usize;
    let mut skipped_routes = 0usize;
    for route in seed.routes {
        if existing_route_ids.contains(&route.public_model_id) {
            // Insert-if-absent: an existing route is skipped, never updated. Warn
            // if the seed diverges from the stored row (a same-file duplicate
            // isn't in `existing_routes_by_id` — just skip it).
            match existing_routes_by_id.get(&route.public_model_id) {
                Some(existing) => {
                    warn_if_route_differs(existing, &route, name_to_id.get(&route.connection))
                }
                None => tracing::info!(
                    public_model_id = %route.public_model_id,
                    "route already defined earlier in this seed; skipping"
                ),
            }
            skipped_routes += 1;
            continue;
        }
        let connection_id = name_to_id.get(&route.connection).cloned().ok_or_else(|| {
            format!(
                "route '{}' references unknown connection '{}'",
                route.public_model_id, route.connection
            )
        })?;
        let public_model_id = route.public_model_id.clone();
        routes
            .create(RouteInput {
                routing: Default::default(),
                public_model_id: route.public_model_id,
                connection_id,
                upstream_model_id: route.upstream_model_id,
                published: route.published.unwrap_or(false),
            })
            .await
            .map_err(|e| format!("invalid seed route '{public_model_id}': {e}"))?;
        // Record it so a same-file duplicate is skipped like a pre-existing one.
        existing_route_ids.insert(public_model_id);
        created_routes += 1;
    }

    Ok(format!(
        "created {created_conns} connection(s) (skipped {skipped_conns} existing), \
         created {created_routes} route(s) (skipped {skipped_routes} existing)"
    ))
}

/// Warn when a seed entry names an existing connection whose stored config
/// differs from what the seed declares. The seeder is insert-if-absent, so a
/// divergent entry has no effect — surfacing it lets an operator notice that an
/// intended change (endpoint, prefix, exposure, routing, or a key rotation the
/// seeder can't even see) needs the Providers UI / update API instead. Only
/// fields the seed explicitly set are compared, so an omitted-and-defaulted
/// field isn't reported as a spurious difference.
fn warn_if_connection_differs(existing: &StoredConnection, seed: &SeedConnection) {
    let mut diffs: Vec<String> = Vec::new();
    if seed.kind != existing.provider_kind() {
        diffs.push(format!(
            "kind (seed {} vs stored {})",
            seed.kind,
            existing.provider_kind()
        ));
    }
    let seed_base = seed
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let stored_base = existing.base_url().map(str::trim).filter(|s| !s.is_empty());
    if seed_base != stored_base {
        diffs.push(format!(
            "base_url (seed {seed_base:?} vs stored {stored_base:?})"
        ));
    }
    if let Some(prefix) = seed.prefix.as_deref() {
        if Some(prefix) != existing.prefix() {
            diffs.push(format!(
                "prefix (seed {:?} vs stored {:?})",
                Some(prefix),
                existing.prefix()
            ));
        }
    }
    if let Some(exposure_str) = seed.exposure.as_deref() {
        if let Some(exposure) = ExposurePolicy::parse(exposure_str) {
            if exposure != existing.exposure() {
                diffs.push(format!(
                    "exposure (seed {:?} vs stored {:?})",
                    exposure,
                    existing.exposure()
                ));
            }
        }
    }
    if let Some(routing_enabled) = seed.routing_enabled {
        if routing_enabled != existing.routing_enabled() {
            diffs.push(format!(
                "routing_enabled (seed {} vs stored {})",
                routing_enabled,
                existing.routing_enabled()
            ));
        }
    }
    if let Some(declared) = &seed.declared_capabilities {
        if Some(declared) != existing.declared_capabilities() {
            diffs.push("declared_capabilities".to_string());
        }
    }

    if diffs.is_empty() {
        tracing::info!(name = %seed.name, "connection already exists; skipping");
    } else {
        tracing::warn!(
            name = %seed.name,
            diverged = %diffs.join(", "),
            "connection already exists; the seeder is insert-only and did NOT apply these changes \
             (nor can it rotate an api_key). Use the Providers UI or update API to change it."
        );
    }
}

/// Warn when a seed entry names an existing route whose stored target differs.
/// Like connections, existing routes are never updated by the seeder.
/// `seed_connection_id` is the id the seed's `connection` name resolves to (if
/// any), compared against the stored route's `connection_id`.
fn warn_if_route_differs(
    existing: &ModelRoute,
    seed: &SeedRoute,
    seed_connection_id: Option<&String>,
) {
    let mut diffs: Vec<String> = Vec::new();
    if let Some(id) = seed_connection_id {
        if id != &existing.connection_id {
            diffs.push(format!(
                "connection (seed '{}' -> {} vs stored {})",
                seed.connection, id, existing.connection_id
            ));
        }
    }
    if seed.upstream_model_id != existing.upstream_model_id {
        diffs.push(format!(
            "upstream_model_id (seed {:?} vs stored {:?})",
            seed.upstream_model_id, existing.upstream_model_id
        ));
    }
    if let Some(published) = seed.published {
        if published != existing.published {
            diffs.push(format!(
                "published (seed {} vs stored {})",
                published, existing.published
            ));
        }
    }

    if diffs.is_empty() {
        tracing::info!(public_model_id = %seed.public_model_id, "route already exists; skipping");
    } else {
        tracing::warn!(
            public_model_id = %seed.public_model_id,
            diverged = %diffs.join(", "),
            "route already exists; the seeder is insert-only and did NOT apply these changes. \
             Use the Model Routes UI or update API to change it."
        );
    }
}

/// Turn a seed entry into a [`ConnectionInput`], applying defaults and
/// validating the kind / base_url / prefix combination.
fn connection_input(conn: SeedConnection) -> Result<ConnectionInput, Box<dyn std::error::Error>> {
    let exposure_str = conn.exposure.as_deref().unwrap_or("dynamic");
    let exposure = ExposurePolicy::parse(exposure_str).ok_or_else(|| {
        format!(
            "connection '{}' has unknown exposure '{exposure_str}'",
            conn.name
        )
    })?;

    let is_builtin = provider_registry::is_builtin(&conn.kind);
    let is_compatible = conn.kind == OPENAI_COMPATIBLE_KIND;
    if !is_builtin && !is_compatible {
        return Err(format!(
            "connection '{}' has unknown kind '{}'",
            conn.name, conn.kind
        )
        .into());
    }

    // Explicit connections are route-only, so an omitted prefix stays absent.
    // Other built-ins retain the convenient canonical-prefix default.
    let prefix = match conn.prefix {
        Some(p) => Some(p),
        None if exposure == ExposurePolicy::Explicit => None,
        None => provider_registry::canonical_prefix(&conn.kind).map(str::to_string),
    };

    // vLLM and openai-compatible endpoints have no code-owned default endpoint,
    // so they must supply their own base URL (mirrors the service validation).
    // `requires_explicit_base_url` is exactly that set.
    if provider_registry::requires_explicit_base_url(&conn.kind) {
        let has_base_url = conn
            .base_url
            .as_deref()
            .map(|b| !b.trim().is_empty())
            .unwrap_or(false);
        if !has_base_url {
            return Err(format!(
                "connection '{}' ({}) requires a base_url",
                conn.name, conn.kind
            )
            .into());
        }
    }

    // A dynamic/unlisted openai-compatible endpoint must own a prefix to be
    // reachable (built-ins default to their canonical prefix above).
    if is_compatible && prefix.is_none() && exposure != ExposurePolicy::Explicit {
        return Err(format!(
            "connection '{}' (openai-compatible) requires a prefix unless exposure is explicit",
            conn.name
        )
        .into());
    }

    // Mirror the service create rule (`ProviderConnectionService::create`):
    // Runtime uses the process AWS credential chain, while the explicitly
    // addressed compatibility providers may be unauthenticated. All other
    // built-ins require a non-empty key.
    let is_bedrock_runtime = conn.kind == "bedrock"
        && crate::backend::bedrock::is_runtime_connection(conn.base_url.as_deref());
    let api_key = match conn.api_key.filter(|k| !k.trim().is_empty()) {
        Some(k) => k,
        None if provider_registry::requires_explicit_base_url(&conn.kind) || is_bedrock_runtime => {
            KEYLESS_PLACEHOLDER.to_string()
        }
        None => {
            return Err(format!("connection '{}': api_key must not be empty", conn.name).into());
        }
    };

    Ok(ConnectionInput {
        name: conn.name,
        provider_kind: conn.kind,
        base_url: conn.base_url,
        prefix,
        exposure,
        routing_enabled: conn.routing_enabled.unwrap_or(true),
        declared_capabilities: conn.declared_capabilities,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_routes::InMemoryModelRouteStore;
    use crate::provider_connections::InMemoryProviderConnectionStore;

    #[test]
    fn builtin_defaults_prefix_exposure_and_keyless_placeholder() {
        // vLLM is a legitimately keyless builtin: an omitted key becomes the
        // placeholder, and the canonical prefix + default exposure are applied.
        // (It still needs an explicit base_url — no code-owned default endpoint.)
        let input = connection_input(SeedConnection {
            name: "vllm".into(),
            kind: "vllm".into(),
            base_url: Some("http://localhost:8000/v1".into()),
            prefix: None,
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .unwrap();

        assert_eq!(input.prefix.as_deref(), Some("vllm/"));
        assert_eq!(input.exposure, ExposurePolicy::Dynamic);
        assert_eq!(input.api_key, KEYLESS_PLACEHOLDER);
        assert!(input.routing_enabled);
    }

    #[test]
    fn bedrock_runtime_uses_the_aws_chain_without_a_seeded_key() {
        let input = connection_input(SeedConnection {
            name: "bedrock-runtime".into(),
            kind: "bedrock".into(),
            base_url: Some("https://bedrock-runtime.us-west-2.amazonaws.com".into()),
            prefix: None,
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .unwrap();

        assert_eq!(input.api_key, KEYLESS_PLACEHOLDER);
    }

    #[test]
    fn vllm_requires_base_url() {
        // vLLM has no code-owned default endpoint, so an omitted base_url is an
        // error (mirrors the service validation) rather than an unroutable row.
        let err = connection_input(SeedConnection {
            name: "vllm".into(),
            kind: "vllm".into(),
            base_url: None,
            prefix: None,
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .err()
        .expect("expected error");
        assert!(err.to_string().contains("requires a base_url"), "{err}");
    }

    #[test]
    fn omitted_key_for_key_requiring_builtin_is_rejected() {
        // openai requires a real key: an omitted (or whitespace-only) api_key is
        // an error, matching the service create rule — it is NOT silently turned
        // into the keyless placeholder (which the store accepts but that 401s
        // upstream).
        for api_key in [None, Some("   ".to_string())] {
            let err = connection_input(SeedConnection {
                name: "openai".into(),
                kind: "openai".into(),
                base_url: None,
                prefix: None,
                exposure: None,
                api_key,
                routing_enabled: None,
                declared_capabilities: None,
            })
            .err()
            .expect("expected error");
            assert!(err.to_string().contains("must not be empty"), "{err}");
        }
    }

    #[test]
    fn openai_compatible_requires_base_url() {
        let err = connection_input(SeedConnection {
            name: "custom".into(),
            kind: OPENAI_COMPATIBLE_KIND.into(),
            base_url: None,
            prefix: Some("custom/".into()),
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .err()
        .expect("expected error");
        assert!(err.to_string().contains("requires a base_url"));
    }

    #[test]
    fn openai_compatible_requires_prefix_unless_explicit() {
        // No prefix + dynamic exposure is rejected.
        let err = connection_input(SeedConnection {
            name: "custom".into(),
            kind: OPENAI_COMPATIBLE_KIND.into(),
            base_url: Some("https://x/v1".into()),
            prefix: None,
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .err()
        .expect("expected error");
        assert!(err.to_string().contains("requires a prefix"));

        // No prefix + explicit exposure is allowed (reachable only via routes).
        let input = connection_input(SeedConnection {
            name: "custom".into(),
            kind: OPENAI_COMPATIBLE_KIND.into(),
            base_url: Some("https://x/v1".into()),
            prefix: None,
            exposure: Some("explicit".into()),
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .unwrap();
        assert_eq!(input.prefix, None);
        assert_eq!(input.exposure, ExposurePolicy::Explicit);
    }

    #[test]
    fn builtin_explicit_connection_may_omit_prefix() {
        let input = connection_input(SeedConnection {
            name: "openai-preview".into(),
            kind: "openai".into(),
            base_url: None,
            prefix: None,
            exposure: Some("explicit".into()),
            api_key: Some("sk-preview".into()),
            routing_enabled: None,
            declared_capabilities: None,
        })
        .unwrap();

        assert_eq!(input.prefix, None);
        assert_eq!(input.exposure, ExposurePolicy::Explicit);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = connection_input(SeedConnection {
            name: "x".into(),
            kind: "not-a-provider".into(),
            base_url: None,
            prefix: None,
            exposure: None,
            api_key: None,
            routing_enabled: None,
            declared_capabilities: None,
        })
        .err()
        .expect("expected error");
        assert!(err.to_string().contains("unknown kind"));
    }

    fn sample_seed() -> SeedFile {
        SeedFile {
            connections: vec![
                SeedConnection {
                    name: "openai".into(),
                    kind: "openai".into(),
                    base_url: None,
                    prefix: None,
                    exposure: None,
                    api_key: Some("sk-test".into()),
                    routing_enabled: None,
                    declared_capabilities: None,
                },
                SeedConnection {
                    name: "chalk-gemma".into(),
                    kind: OPENAI_COMPATIBLE_KIND.into(),
                    base_url: Some("https://gemma.internal/v1".into()),
                    prefix: Some("chalk-gemma/".into()),
                    exposure: Some("unlisted".into()),
                    api_key: None,
                    routing_enabled: None,
                    declared_capabilities: None,
                },
            ],
            routes: vec![SeedRoute {
                public_model_id: "chalk/gemma".into(),
                connection: "chalk-gemma".into(),
                upstream_model_id: "gemma-3-27b-it".into(),
                published: Some(true),
            }],
        }
    }

    /// Build in-memory stores plus the services layered over them. The store
    /// handles are returned too, so tests can assert directly against what was
    /// (or wasn't) persisted.
    fn harness() -> (
        Arc<dyn ProviderConnectionStore>,
        Arc<dyn ModelRouteStore>,
        ProviderConnectionService,
        ModelRouteService,
    ) {
        let routes: Arc<dyn ModelRouteStore> = Arc::new(InMemoryModelRouteStore::new());
        let connections: Arc<dyn ProviderConnectionStore> =
            Arc::new(InMemoryProviderConnectionStore::with_routes(routes.clone()));
        let conn_svc = ProviderConnectionService::new(connections.clone(), reqwest::Client::new());
        let route_svc = ModelRouteService::new(routes.clone(), connections.clone());
        (connections, routes, conn_svc, route_svc)
    }

    /// A valid openai-compatible connection routes can reference by name.
    fn ref_connection() -> SeedConnection {
        SeedConnection {
            name: "ref".into(),
            kind: OPENAI_COMPATIBLE_KIND.into(),
            base_url: Some("https://ref.internal/v1".into()),
            prefix: Some("ref/".into()),
            exposure: None,
            api_key: Some("sk-ref".into()),
            routing_enabled: None,
            declared_capabilities: None,
        }
    }

    #[tokio::test]
    async fn apply_creates_then_is_idempotent() {
        let (connections, routes, conn_svc, route_svc) = harness();

        // First run creates everything.
        let summary = apply(&conn_svc, &route_svc, sample_seed()).await.unwrap();
        assert!(summary.contains("created 2 connection(s)"), "{summary}");
        assert!(summary.contains("created 1 route(s)"), "{summary}");
        assert_eq!(connections.list().await.unwrap().len(), 2);
        assert_eq!(routes.list().await.unwrap().len(), 1);

        // The route resolved to the seeded connection's id.
        let conns = connections.list().await.unwrap();
        let gemma = conns
            .iter()
            .find(|c| c.prefix() == Some("chalk-gemma/"))
            .unwrap();
        let route = routes
            .get_by_public_id("chalk/gemma")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.connection_id, gemma.id());
        assert_eq!(route.upstream_model_id, "gemma-3-27b-it");
        assert!(route.published);

        // Second run skips everything (idempotent).
        let summary = apply(&conn_svc, &route_svc, sample_seed()).await.unwrap();
        assert!(
            summary.contains("created 0 connection(s) (skipped 2"),
            "{summary}"
        );
        assert!(
            summary.contains("created 0 route(s) (skipped 1"),
            "{summary}"
        );
        assert_eq!(connections.list().await.unwrap().len(), 2);
        assert_eq!(routes.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn apply_does_not_update_a_diverged_existing_connection_or_route() {
        // Insert-only: re-seeding an existing name / public_model_id with CHANGED
        // fields skips it (a warning is logged) and leaves the stored row
        // untouched — the seeder never turns into an implicit update.
        let (connections, routes, conn_svc, route_svc) = harness();
        apply(&conn_svc, &route_svc, sample_seed()).await.unwrap();

        // Re-seed with a rotated endpoint + a different upstream model id.
        let mut diverged = sample_seed();
        diverged.connections[1].base_url = Some("https://CHANGED.internal/v1".into());
        diverged.routes[0].upstream_model_id = "changed-model".into();

        let summary = apply(&conn_svc, &route_svc, diverged).await.unwrap();
        assert!(
            summary.contains("created 0 connection(s) (skipped 2"),
            "{summary}"
        );
        assert!(
            summary.contains("created 0 route(s) (skipped 1"),
            "{summary}"
        );

        // Stored values are unchanged — the divergence was not applied.
        let conns = connections.list().await.unwrap();
        let gemma = conns.iter().find(|c| c.name() == "chalk-gemma").unwrap();
        assert_eq!(gemma.base_url(), Some("https://gemma.internal/v1"));
        let route = routes
            .get_by_public_id("chalk/gemma")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(route.upstream_model_id, "gemma-3-27b-it");
    }

    #[tokio::test]
    async fn duplicate_route_public_model_id_in_one_seed_is_skipped() {
        // Two entries with the same `public_model_id` in ONE seed file: the first
        // is created, the second is skipped (not an error), mirroring how a
        // same-file duplicate connection name is skipped. Before the fix the
        // second `create` hit the uniqueness constraint and failed the whole run.
        let (_connections, routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![ref_connection()],
            routes: vec![
                SeedRoute {
                    public_model_id: "chalk/dup".into(),
                    connection: "ref".into(),
                    upstream_model_id: "model-a".into(),
                    published: Some(true),
                },
                SeedRoute {
                    public_model_id: "chalk/dup".into(),
                    connection: "ref".into(),
                    upstream_model_id: "model-b".into(),
                    published: Some(true),
                },
            ],
        };

        let summary = apply(&conn_svc, &route_svc, seed).await.unwrap();
        assert!(
            summary.contains("created 1 route(s) (skipped 1"),
            "{summary}"
        );
        // Exactly one route persisted — the first entry wins; the duplicate was
        // skipped rather than erroring.
        assert_eq!(routes.list().await.unwrap().len(), 1);
        let route = routes.get_by_public_id("chalk/dup").await.unwrap().unwrap();
        assert_eq!(route.upstream_model_id, "model-a");
    }

    #[tokio::test]
    async fn route_referencing_unknown_connection_errors() {
        let (_connections, _routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![],
            routes: vec![SeedRoute {
                public_model_id: "chalk/gemma".into(),
                connection: "missing".into(),
                upstream_model_id: "gemma".into(),
                published: Some(true),
            }],
        };

        let err = apply(&conn_svc, &route_svc, seed).await.unwrap_err();
        assert!(err.to_string().contains("unknown connection 'missing'"));
    }

    #[tokio::test]
    async fn seeding_rejects_reserved_chalk_connection_prefix() {
        let (connections, _routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![SeedConnection {
                name: "reserved".into(),
                kind: OPENAI_COMPATIBLE_KIND.into(),
                base_url: Some("https://x/v1".into()),
                prefix: Some("chalk/".into()),
                exposure: None,
                api_key: Some("sk".into()),
                routing_enabled: None,
                declared_capabilities: None,
            }],
            routes: vec![],
        };

        let err = apply(&conn_svc, &route_svc, seed).await.unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
        // The invalid row was not written.
        assert_eq!(connections.list().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn seeding_rejects_cross_kind_connection_prefix() {
        // `anthropic` kind claiming the `openai/` prefix: accepted by the old
        // seeder's partial checks, rejected by the service's canonical/cross-kind
        // validation.
        let (connections, _routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![SeedConnection {
                name: "anthropic".into(),
                kind: "anthropic".into(),
                base_url: None,
                prefix: Some("openai/".into()),
                exposure: None,
                api_key: Some("sk".into()),
                routing_enabled: None,
                declared_capabilities: None,
            }],
            routes: vec![],
        };

        let err = apply(&conn_svc, &route_svc, seed).await.unwrap_err();
        assert!(err.to_string().contains("prefix"), "{err}");
        assert_eq!(connections.list().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn seeding_rejects_empty_route_public_model_id_suffix() {
        // `public_model_id = "chalk/"` has an empty suffix after the prefix.
        let (_connections, routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![ref_connection()],
            routes: vec![SeedRoute {
                public_model_id: "chalk/".into(),
                connection: "ref".into(),
                upstream_model_id: "some-model".into(),
                published: None,
            }],
        };

        let err = apply(&conn_svc, &route_svc, seed).await.unwrap_err();
        assert!(err.to_string().contains("chalk/"), "{err}");
        // The valid connection was created, but the invalid route was not.
        assert_eq!(routes.list().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn seeding_rejects_empty_upstream_model_id() {
        let (_connections, routes, conn_svc, route_svc) = harness();
        let seed = SeedFile {
            connections: vec![ref_connection()],
            routes: vec![SeedRoute {
                public_model_id: "chalk/valid".into(),
                connection: "ref".into(),
                upstream_model_id: "   ".into(),
                published: None,
            }],
        };

        let err = apply(&conn_svc, &route_svc, seed).await.unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
        assert_eq!(routes.list().await.unwrap().len(), 0);
    }

    #[test]
    fn expand_env_vars_substitutes_and_escapes() {
        let env = |name: &str| match name {
            "OPENAI_API_KEY" => Some("sk-openai".to_string()),
            "ANTHROPIC_API_KEY" => Some("sk-ant".to_string()),
            _ => None,
        };

        // Placeholders are substituted; surrounding text is preserved.
        assert_eq!(
            expand_env_vars(r#"{"api_key":"${OPENAI_API_KEY}"}"#, env).unwrap(),
            r#"{"api_key":"sk-openai"}"#
        );
        // Multiple placeholders in one string.
        assert_eq!(
            expand_env_vars("${OPENAI_API_KEY}:${ANTHROPIC_API_KEY}", env).unwrap(),
            "sk-openai:sk-ant"
        );
        // No placeholders -> passthrough (literal-key seed files still work).
        assert_eq!(
            expand_env_vars(r#"{"api_key":"sk-literal"}"#, env).unwrap(),
            r#"{"api_key":"sk-literal"}"#
        );
        // `$${` escapes to a literal `${`, and a lone `$` is preserved.
        assert_eq!(
            expand_env_vars("price $5 and $${OPENAI_API_KEY}", env).unwrap(),
            "price $5 and ${OPENAI_API_KEY}"
        );
    }

    #[test]
    fn expand_env_vars_json_escapes_special_characters() {
        // A credential with a quote, backslash, and newline must NOT break the
        // JSON: after expansion the document parses and the value round-trips.
        let nasty = "ab\"c\\d\nef";
        let env = move |name: &str| (name == "KEY").then(|| nasty.to_string());
        let expanded = expand_env_vars(r#"{"connections":[{"api_key":"${KEY}"}]}"#, env).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&expanded)
            .unwrap_or_else(|e| panic!("expansion produced invalid JSON: {e}\n{expanded}"));
        assert_eq!(parsed["connections"][0]["api_key"], nasty);
    }

    #[test]
    fn expand_env_vars_fails_closed_on_unset_or_malformed() {
        let env = |_: &str| None;
        // An unset referenced var is a hard error (never substitutes empty).
        let err = expand_env_vars("${MISSING_KEY}", env).unwrap_err();
        assert!(
            err.contains("unset environment variable 'MISSING_KEY'"),
            "{err}"
        );
        // Unterminated placeholder.
        assert!(expand_env_vars("${OPENAI", env)
            .unwrap_err()
            .contains("unterminated"));
        // Invalid var name inside the braces.
        assert!(expand_env_vars("${bad-name}", env)
            .unwrap_err()
            .contains("invalid placeholder"));
    }

    #[test]
    fn parses_documented_seed_file_shape() {
        let json = r#"{
            "connections": [
                {"name":"openai","kind":"openai","api_key":"sk-..."},
                {"name":"chalk-gemma","kind":"openai-compatible","base_url":"https://x/v1","api_key":"unused","exposure":"unlisted","prefix":"chalk-gemma/"}
            ],
            "routes": [
                {"public_model_id":"chalk/gemma","connection":"chalk-gemma","upstream_model_id":"gemma-3-27b-it","published":true}
            ]
        }"#;
        let seed: SeedFile = serde_json::from_str(json).unwrap();
        assert_eq!(seed.connections.len(), 2);
        assert_eq!(seed.routes.len(), 1);
        assert_eq!(seed.routes[0].public_model_id, "chalk/gemma");
    }
}
