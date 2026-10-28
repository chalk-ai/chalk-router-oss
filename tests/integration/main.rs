// Each module below used to be its own `tests/*.rs` file, which cargo compiles
// as an independent test binary that relinks this crate and all of its
// dependencies. Merging them into modules of one binary cuts that relinking
// cost; `cargo nextest` still runs every `#[test]` in its own process, so no
// isolation is lost. Add new integration test files as a `mod` line here.
//
// `integration_test.rs` stays a separate target (see its `[[test]]` entry in
// Cargo.toml): it's an opt-in, paid-API smoke suite that must stay excluded
// from the default `cargo test` / CI run.

mod anthropic_messages_fallback_test;
mod anthropic_models_test;
mod anthropic_passthrough_test;
mod anthropic_prompt_cache_test;
mod anthropic_span_test;
mod anthropic_streaming_test;
mod anthropic_translation_test;
mod api_key_permission_test;
mod bedrock_mantle_test;
mod braintrust_span_test;
mod config_startup_test;
mod decisions_compat_test;
mod decisions_test;
mod development_auth_test;
mod embedding_response_test;
mod environment_isolation_test;
mod federated_admin_test;
mod federated_proxy_test;
mod gemini_embedding_quota_test;
mod gemini_embeddings_test;
mod gemini_images_test;
mod gemini_interactions_test;
mod grpc_smoke_test;
mod jev_prompting_test;
mod judge_test;
mod mantle_closed_gpt_test;
mod metadata_client_test;
mod migrate_bookkeeping_test;
mod migrate_invalid_index_test;
mod moderations_test;
mod native_audio_test;
mod native_fidelity_test;
mod postgres_atomic_key_writes_test;
mod postgres_pool_health_test;
mod prompt_cache_test;
mod quota_hot_path_test;
mod quota_lock_order_test;
mod rate_limit_migration_test;
mod ratelimit_conformance_test;
mod responses_chat_fidelity_test;
mod responses_test;
mod sampling_gate_test;
mod schema_order_test;
mod service_test;
mod session_baggage_test;
mod span_ingest_test;
mod span_test;
mod streaming_fallback_test;
mod streaming_open_deadline_test;
mod streaming_test;
mod support;
mod tenancy_test;
mod tenant_telemetry_test;
mod unsupported_endpoints_test;
mod usage_cost_test;
mod usage_pool_names_migration_test;
mod usage_pool_names_test;
mod usage_snapshot_test;
mod vertex_test;

mod route_grid_conformance_test;
