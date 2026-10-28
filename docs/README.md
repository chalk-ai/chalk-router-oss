# Documentation

[Repository overview](../README.md)

Use the guides for current behavior and operational instructions. RFPs retain
design rationale and point-in-time implementation plans; dated changelogs record
what changed, rather than prescribing today's setup.

## Use the router

| Task | Guide |
| --- | --- |
| Build and start a local router | [Building and running](../README.md#building--running) |
| Understand credentials and upcoming auth changes | [Authentication](../README.md#authentication) · [Pending changes](authentication-transition.md) |
| Call inference or admin APIs | [API reference](api.md) |
| Configure connections, aliases, judging, and fallback | [Model routing and providers](model-routing.md) |
| Configure coding clients | [Chalk CLI guide](router-cli.md) |
| Enable exact-prefix caching | [Prompt caching](prompt-caching.md) |
| Configure tracing, content capture, and exporters | [Tracing and telemetry](tracing.md) |

## Operate and develop

| Task | Guide |
| --- | --- |
| Understand the code and run checks | [Development](development.md) · [Repository instructions](../AGENTS.md) |
| Apply or recover migrations; update schema snapshots | [Migrations](../migrations/README.md) |
| Operate the existing Argo deployment | [Deployment](deployment.md) |
| Serve multiple teams' environments | [Cluster router](cluster-router.md) |
| Inspect background jobs and retention | [Background Responses operations](background-responses-operations.md) |
| Run a synthetic upstream | [Mock service](../test_services/mock_openai/README.md) |
| Run SDK smoke tests | [Smoke-test client](../frontend_test/README.md) |
| Measure isolated load and quotas | [Load benchmarks](../test_services/router_bench/README.md) |
| Understand retained-JSON guarantees and measurements | [JSON performance](schema-json-performance.md) |

## Design and history

- [RFP index](../rfp/README.md): architecture, capabilities, accounting, quotas,
  and routing decisions, with links to current guidance where behavior evolved.
- [Changelog index](../changelogs/README.md): dated user-visible change records.
- [Agent skills](../.agents/skills/): repository workflows; `CLAUDE.md` imports
  `AGENTS.md`, and the other agent skill directories link to the same sources.

Keep operational instructions in the relevant guide and link to them from
overviews. When moving a section, update relative links and preserve useful
entry points. Preserve historical records and add dated context when their
original assumptions no longer describe the implementation.
