# Google signer security patch

This directory contains reqsign-google **3.2.0** from its crates.io release,
including its Apache-2.0 LICENSE and NOTICE. It is selected through the root
Cargo.toml `[patch.crates-io]`. Upstream: https://github.com/apache/opendal-reqsign.

RUSTSEC-2023-0071 has no patched RSA release. The router can reach the original
RustCrypto RSA operation when a GCS service-account credential mints an OAuth
JWT. Upstream also uses it for signed URLs. Neither is dismissed as safe merely
because the router does not expose RSA decryption.

The local changes replace both operations with the existing jsonwebtoken 9.3.1 /
ring RS256 implementation. OAuth claims and URL canonicalization are unchanged;
signed URLs retain the upstream uppercase hexadecimal signature representation.
RSA is now a development-only dependency for upstream test key generation. The
root production lockfile and Linux normal dependency graph contain no rsa crate.

Changed upstream files:

- Cargo.toml: add jsonwebtoken, make base64 available for signature decoding,
  move rsa to dev-dependencies, and remove reqsign-core's RSA-backed jwt feature.
- src/lib.rs and new src/crypto.rs: small ring-backed signing adapter.
- src/sign_request.rs: replace signed-URL signing and fallback OAuth JWT signing;
  retain signed-URL wire encoding.
- src/provide_credential/service_account_token.rs: replace OAuth JWT signing.
- Test key generation uses 2048 bits, required by ring's RSA signing policy.

- testdata/test_credential.json: replace the embedded private key and key ID with explicit non-signing parser placeholders.
- testdata/testbucket_credential.json: remove an unused credential fixture.

The original manifest is retained as Cargo.toml.orig. Other upstream files are
unchanged. Source archives are available at
https://static.crates.io/crates/reqsign-google/reqsign-google-3.2.0.crate.

Remove this patch when an upstream release removes or replaces the affected
private-key operations. Do not remove it merely because the package version
changes. Re-run the upstream library tests, router tests, Linux dependency-tree
check, cargo audit, and npm audit before switching. Track this in META-855.

The retained testdata credential is a parser-only fixture and cannot sign. Crypto
tests generate their own ephemeral keys. Public upstream provenance alone does
not establish whether an embedded key was ever used operationally, so the two
embedded keys are excluded from this distribution rather than allowlisted. No
new runtime credentials or signing configuration are introduced.
