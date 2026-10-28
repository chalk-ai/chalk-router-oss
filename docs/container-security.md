# Container and network defaults

Backend, UI, and the dedicated Caddy image run as UID/GID 10001. Build the latter
from `caddy/Dockerfile`: it strips the official binary's low-port file capability,
which otherwise prevents execution when all capabilities are dropped. Use this
image, not stock Caddy, in `caddy.image`; the listener uses unprivileged port 8443. The chart applies that identity to
backend, migration, UI, and Caddy; it prohibits privilege escalation, drops all
capabilities, selects RuntimeDefault seccomp, and makes root filesystems read-only.
Writable emptyDir mounts are limited to temporary files, Next.js cache, and Caddy
state/config, each with a sizeLimit. Do not put durable application data in them.
These settings follow the Kubernetes [restricted pod security controls](https://kubernetes.io/docs/concepts/security/pod-security-standards/).

Service-account token mounting defaults off for every pod. Set
`backend.automountServiceAccountToken=true` only for a backend integration that
needs a Kubernetes API token; configure its service account/RBAC separately.
Migration and UI pods never receive automatic tokens. Confirm your cloud workload
identity and mounted credential file permissions work with UID 10001 before rollout.

No ingress is rendered by default. Enable `ingress.uiEnabled` or
`ingress.apiEnabled` for private Tailscale ingress. The UI always sets Funnel false.
Public API exposure additionally requires `ingress.apiFunnelEnabled=true` and
`ingress.apiEnabled=true`. Enabling Funnel alone fails rendering. Existing releases
must explicitly opt into the exposure they intend to retain on upgrade.

[NetworkPolicy examples](security/network-policy-example.yaml) isolate UI and
backend ingress and egress. They are not automatically installed: namespace/pod
selectors and allowed DB, DNS, provider, metadata, telemetry, cloud identity, and
cache endpoints are deployment-specific. The example permits public IPv4 HTTPS,
not a provider hostname allowlist, and blocks private/metadata ranges until you
add required destinations. IPv6 egress is denied. NetworkPolicy needs a supporting
CNI; NAT and node-local traffic behavior must be verified in the target cluster.
See the [Kubernetes NetworkPolicy documentation](https://kubernetes.io/docs/concepts/services-networking/network-policies/).

Validate before rollout:

```sh
helm lint charts/chalk-router
python3 scripts/check-container-hardening.py # helm and mikefarah/yq required
```

The image smoke workflow builds images without publishing and exercises startup
under read-only roots, no added capabilities, and no privilege escalation. Template
checks cannot prove a particular cluster enforces seccomp or network policy. No
existing deployment or account-level exposure is changed by opening this PR.
