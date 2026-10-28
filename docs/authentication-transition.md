# Authentication and UI sessions

These requirements apply to this combined release candidate. Check the deployed revision before using them with an older build.

## Configuration and authorization

- **Backend:** set `CHALK_JWT_AUDIENCE`; environment mode also requires
  `CHALK_TEAM_ID`. Environment-mode admin JWTs need a matching signed
  `environment_id`, team ownership, and the operation's grant. Obtain scoped
  replacement credentials for existing unscoped user tokens. Cross-environment
  inference acceptance cannot relax admin authorization. Compact `p` permission
  payloads remain rejected on admin/cluster requests pending decoder support.
- **Helm federation:** configure `backend.auth.jwtIssuers`,
  `backend.auth.jwtAudience`, and, for environment mode, `backend.auth.teamId`.
- **UI:** configure `ROUTER_UI_ORIGIN` and a dedicated
  `ROUTER_UI_SESSION_SECRET` of 64 random hexadecimal characters. Use HTTPS
  outside loopback. All replicas must share the UI secret; other applications
  must not. Missing/invalid configuration returns 503.
- **Helm UI:** set `ui.auth.origin` and supply the Secret referenced by
  `ui.auth.existingSecret`, with key `ROUTER_UI_SESSION_SECRET`. The UI never mounts the backend admin key.
- **Sessions:** JWT sign-in requires `tokens.list`; other RPCs are authorized
  individually by the backend. Encrypted, HttpOnly, SameSite=Strict sessions last
  at most 15 minutes or JWT expiry, with a 3,800-character cookie limit.
  Signing out clears the browser cookie; rotate the UI secret to invalidate
  copied sessions, and follow issuer policy for a compromised JWT.
- **Playground:** provision a separate router key with limited models,
  permissions, and budgets. There is no shared admin-key fallback. State changes
  require the exact same origin and JSON content type (DELETE may omit it).

After building the UI, run `npm run test:auth` from `ui/` for the synthetic authentication smoke test. `/healthz` exposes liveness only.
