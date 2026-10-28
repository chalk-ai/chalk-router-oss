//! Shared fixtures for `tests/integration/*`. Every submodule here replaces
//! logic that used to be copy-pasted across multiple test files:
//!
//! - [`app`] — `AppStateBuilder` and the `build_*_app` helpers, replacing 27
//!   hand-built `AppState { .. }` literals across 16 files.
//! - [`mock`] — ephemeral mock upstream servers (incl. the four separate
//!   Anthropic mocks).
//! - [`auth`] — federated JWT helpers shared by the admin/proxy auth tests.
//! - [`db`] — the advisory-lock guard serializing the Postgres migration
//!   integration tests.
//! - [`otel`] — `InMemorySpanExporter` test scaffolding and the
//!   `ANTHROPIC_API_BASE` test mutex.

pub mod app;
pub mod auth;
pub mod db;
pub mod mock;
pub mod otel;
