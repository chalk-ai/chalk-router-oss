# chalk-federated-auth

Federated JWT validation against trusted external issuers — most importantly
Chalk's metadata API server, which signs RS256 user/service JWTs and serves
OIDC discovery (`/.well-known/openid-configuration`) plus a JWKS endpoint
(`/.well-known/jwks.json`).

Extracted from `chalk-mcp-gateway/src/auth/oidc.rs` so any Chalk service
(chalk-mcp-gateway, chalk-router, ...) can accept Chalk-issued JWTs without
sharing a secret with the API server.

## Usage

```rust
use chalk_federated_auth::{IssuerConfig, OidcRegistry};

// At startup. The registry caches JWKS (5 min TTL) and discovery results,
// so construct it once and share it.
let registry = OidcRegistry::from_config(&[
    IssuerConfig::discovery("https://api.chalk.ai"),
])?;

// Per request:
match registry.try_validate(bearer_token).await {
    None => { /* token's `iss` is not a trusted issuer — fall back to your own auth */ }
    Some(Ok(claims)) => { /* authenticated; claims.sub is the Chalk user id */ }
    Some(Err(e)) => { /* token claimed a trusted issuer but failed validation — reject */ }
}
```

Issuers can alternatively be configured with an explicit `jwks_uri`, or with
`secret_env` for HS256 shared-secret validation. `claims_map` remaps which
token claims populate `Claims::sub` / `Claims::scopes` (Chalk user tokens
carry the user id in `sub`; service tokens carry permissions in
`permissions`). The serde shape of `IssuerConfig` matches the gateway's
historical `[[oidc_issuers]]` TOML sections.

## Security notes

- `peek_issuer` reads the **unverified** `iss` claim purely to route the token
  to the right key material; trust is only established by `try_validate`.
- The signing algorithm is taken from the token header but the key is fixed by
  issuer config, so an attacker cannot downgrade an RS256 issuer to HS256:
  `DecodingKey::from_jwk` keys only verify the JWK's key family.
- Unknown `kid`s trigger one JWKS refetch (key rotation) before rejecting.

## Development

```bash
cargo test
cargo clippy --all-targets -- -Dwarnings
cargo fmt
```
