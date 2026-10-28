# Dependency security

The META-855 refresh replaces the advisory-matching Rust and UI dependencies.
Run `cargo audit --deny unsound --deny yanked` from the root and
`npm audit --audit-level=low` from `ui/`. CI runs both, checks the Linux runtime
dependency graph, tests the patched Google signer, and runs router regression
checks on Ubuntu. These checks do not establish the absence of unknown flaws.

## Google credentials

The GCS service-account OAuth exchange reaches private RSA signing, even when
signed URLs are unused. RUSTSEC-2023-0071 has no patched RustCrypto RSA release.
The vendored reqsign-google patch uses the existing ring-backed RS256 signer for
OAuth JWTs and signed URLs. The root Cargo.lock contains no RustCrypto RSA;
CI rejects its reintroduction into the Linux runtime graph. See
[patch provenance and maintenance](../vendor/reqsign-google/SECURITY-PATCH.md).
Upstream tests use RSA only to generate synthetic keys; it is a development-only
dependency in the separately tested vendor manifest, not a router dependency.

## Remaining maintenance notices

The October 26, 2026 audit has two maintenance notices, with no vulnerability,
unsoundness, or yanked-package matches in the root lockfile:

- RUSTSEC-2025-0052: async-std 1.13.2 is discontinued. It is absent from the
  Linux normal dependency graph. Retire its optional dependency path during
  the next parent dependency refresh.
- RUSTSEC-2025-0134: rustls-pemfile 2.2.0 is unmaintained. It remains reachable
  through tonic 0.12.3, including the telemetry clients. The advisory describes
  this version as a wrapper over rustls-pki-types, without a known vulnerability.
  Migrating tonic and the associated telemetry stack will remove it.

These are maintenance debt, not suppressed vulnerability exceptions. No advisory
IDs are ignored. Review these notices by January 26, 2027, or sooner if an
applicable vulnerability is published. META-855 tracks the review.
