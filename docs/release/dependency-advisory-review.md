# Dependency advisory review — 2026-10-28

GitHub advisory results are reviewed alongside Cargo audit; a clean RustSec
result alone does not establish absence of GitHub advisories.

- OpenSSL 0.10.81 replaces 0.10.74 and includes the fixes through
  GHSA-phqj-4mhp-q6mq. The transitive openssl-sys lock is refreshed too.
- JWT validation uses jsonwebtoken 10.4.0 with AWS-LC, including the fix for
  GHSA-h395-gr6q-cpjc. The remaining 9.3.1 instance is used only for outgoing
  Google signatures in reqsign-google; its only claim-validation call is a
  unit test. It does not validate incoming authentication tokens. Preserve
  this scope restriction until the signing adapter migrates.
- GHSA-w9wp-h8wv-79jx affects the older OpenTelemetry baggage propagator.
  The router applies the advisory workaround: reject baggage over 8192 bytes
  or 64 members before SDK parsing. Regression tests cover valid and excessive
  inputs. All production extraction uses this bounded propagator.
- Both Python test-service lockfiles are refreshed, including patched certifi,
  setuptools, aiohttp and Starlette. CI audits both complete lock exports with
  pip-audit, in addition to the mock response-contract tests.

The JWT signing-only and bounded-baggage dispositions apply to these code paths,
not to every possible use of the dependency versions. Reassess when those uses
change. No GitHub alerts are automatically dismissed by this document.
