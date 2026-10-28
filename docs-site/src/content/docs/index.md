---
title: Chalk Router API
description: Integrate with the inference and administration APIs supported by this checkout.
---

Chalk Router exposes HTTP inference APIs and a protobuf control plane on one listener. Choose a configured model or route, send a router credential, and use the protocol that model supports.

- [Make your first request](/guides/quickstart/): curl examples and model selection.
- [HTTP endpoint inventory](/reference/http/): every mounted method, with implemented and placeholder operations separated.
- [gRPC service reference](/reference/grpc/): generated messages, enums, RPC signatures, and permission declarations.
- [Validated example payloads](/reference/examples/): synthetic requests and responses checked against source.

## Read compatibility precisely

| Term | Meaning here |
| --- | --- |
| OpenAI-shaped | A familiar HTTP path and envelope; only the behavior documented here is supported. This is not a claim of complete OpenAI compatibility. |
| Native provider | The router retains provider fields and dispatches to a provider-native or adapted operation. Provider schemas, model capabilities, and account configuration still apply. |
| Translated | The router converts between protocols. Fields without a supported representation may be rejected or explicitly omitted. |
| Router extension | Chalk-specific routing, permissions, capabilities, decisions, trace ingestion, and administration. |
| Placeholder | A route is mounted but returns an error instead of implementing the operation. |

The generated Rust reference describes the router's typed view and compatibility codecs. Native Chat, Responses, image, audio, and provider APIs can accept fields beyond those types. Neither the typed view nor a mounted route alone establishes a complete provider contract.

## Version and security baseline

This site describes the combined release candidate, including the authentication,
private telemetry, and disabled file/model-deletion surface changes. Generated
references follow the current source hashes in `source-manifest.json`. These
changes are not a claim about older main or deployed revisions. Read
[authentication and permissions](/guides/authentication/) before integrating administration.
