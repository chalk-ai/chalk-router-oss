# Design records

[Documentation index](../docs/README.md)

These RFPs preserve decisions, alternatives, implementation plans, and review
context. Status labels describe the recorded work; phrases such as “today” or
“current” in older sections refer to their revision date. Use the linked guides
and source for current operational behavior.

| Record | Topic and context |
| --- | --- |
| [0001](0001-router-motivation-and-architecture.md) | Motivation and architecture; September baseline, with later changes summarized in its dated note |
| [0002](0002-streaming-fallback.md) | Retry before stream commitment; current eligibility and precedence are in [model routing](../docs/model-routing.md) |
| [0003](0003-permission-scope-non-llm-endpoints.md) | Historical permission-scope proposal; audio and moderation have since gained pipeline enforcement |
| [0004](0004-per-model-modality.md) | Protocol adapters and capability profiles; capability semantics partly superseded by 0005 |
| [0005](0005-effective-per-model-capabilities.md) | Effective model capabilities, discovery, and interface contracts |
| [0006](0006-usage-attribution-and-cost-accounting.md) | Durable accounting; historical agent dimension was removed, as noted in the record |
| [0007](0007-backend-neutral-quota-enforcement.md) | Memory/Postgres admission, reservation, settlement, and quota design |
| [0008](0008-route-decision-grid.md) | Route-owned judging, fallback, authorization, and accounting |

Operational entry points: [API reference](../docs/api.md),
[development architecture](../docs/development.md#architecture),
[cluster tenancy](../docs/cluster-router.md), and
[migrations](../migrations/README.md). Historical changes remain in the
[changelogs](../changelogs/README.md).
