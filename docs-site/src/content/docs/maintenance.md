---
title: Maintain and preview this site
---

The site lives in `docs-site/` and builds a self-contained static `dist/`. It does not run a router, connect to a database, decrypt secrets, or require a hosted documentation service.

## Framework choice

Starlight provides Markdown content, navigation, search, code blocks, and a local static build. It fits this repository's mix of prose, Rust codecs, provider-native payloads, and protobuf RPCs without presenting an invented OpenAPI schema as authoritative.

Mintlify was considered using its current official [CLI documentation](https://www.mintlify.com/docs/cli/install) and [OpenAPI setup](https://www.mintlify.com/docs/api-playground/openapi-setup). Its automatic OpenAPI endpoint pages are useful for an OpenAPI-led service. This repository instead has source-level HTTP routing, lossless native envelopes, and proto-defined administration. Starlight keeps the build independent of a hosting product while allowing both reference sources to share one site. This choice does not claim Mintlify cannot render prose or protobuf references.

Implementation follows Starlight's [manual setup](https://starlight.astro.build/manual-setup/) and [configuration reference](https://starlight.astro.build/reference/configuration/). Framework/package versions are pinned in `package.json` and the lockfile; update them deliberately and rerun checks.

## Commands

Prerequisites: Node.js 22.23.4+ (a current even-numbered LTS is recommended), npm, `just`, Rust/Cargo. The documentation extractor is an isolated Rust crate with its own lockfile and does not build the router or fetch its private dependency. Router example tests use the repository's Rust toolchain/dependencies. The generator uses the protoc binary and standard imports pinned by its own Cargo.lock; no system protoc is required. Router tests for the initial PR used Rust 1.98.0.

```sh
just docs-install        # npm ci for the pinned site dependencies
just docs-generate       # regenerate all references and validate ProtoJSON fixtures
just docs-dev            # local preview, bound to 127.0.0.1
just docs-check          # source drift, static build, local links/anchors/assets
just docs-test-examples  # actual router codecs + compatibility conversion checks
just docs-preview       # serve the built dist/ locally
```

The equivalent npm scripts run inside `docs-site/`. `npm run build` consumes committed references; `npm run check` additionally needs Rust for freshness verification. Builds include the local search index. No publication/deployment command or workflow is configured.

## What is generated

| Input | Output |
| --- | --- |
| `src/main.rs`, parsed with syn | Exact HTTP method/path/handler inventory |
| `scripts/operations.mjs` | Reviewed behavioral annotations: implemented, provider-dependent, retired/placeholder, guides and type mappings |
| Selected Rust type files, parsed with syn | Field tables, comments, Serde attributes, exact declarations and nested-type links |
| Router/health `.proto` plus imports, compiled by pinned vendored protoc | Every service/message/enum, field numbers, presence, oneofs, comments and custom permission options |
| Synthetic example files | Example reference page and downloadable payloads |
| Relevant implementation/proto/test files | `source-manifest.json` hashes for explicit drift review |

The descriptor set and machine-readable inventory are downloadable from the [gRPC index](/reference/grpc/). The generator does not hand-parse Rust or proto declarations with regular expressions. New HTTP handlers without a reviewed behavior annotation fail generation; removed handlers leave stale annotations that also fail. Generated-file checks compare bytes, detect missing/changed files and orphan reference pages, and never rewrite files in check mode.

Source hashes force a review when implementations change; they do not automatically prove prose is correct. After a semantic change, review the guide and tests before accepting a new hash. Source links target `main`, while source hashes and exact generated declarations identify the locally documented revision.

## Updating an API

1. Change source/proto and its behavioral tests through the normal development workflow.
2. Update `scripts/operations.mjs` if support status, handlers, or schema mappings changed.
3. Update focused guides and synthetic examples. Do not edit generated reference pages.
4. Run `just docs-generate`, `just docs-test-examples`, and `just docs-check`.
5. Inspect the affected pages locally, including navigation, search, narrow-screen layout, tables, and code blocks.
6. Commit source inputs and generated output together. Re-audit the security baseline when PR #353/#354 or related authorization changes merge.

## Validation scope and known gaps

The source extractor validates all checked gRPC bodies with the compiled descriptor's ProtoJSON deserializer. `tests/api_documentation_examples.rs` checks HTTP fixtures against real router types and selected translation/semantic validators, including a negative fidelity check. Neither executes provider inference. Existing integration tests remain the evidence for runtime routing, native fidelity, streaming, tenancy, and placeholder behavior.

`check-links.mjs` validates built local URLs, fragments, and assets, plus repository source-link paths. It does not make external HTTP requests or assert external-link availability. It ignores search-index metadata URLs that are not navigable HTML links.

Native Anthropic/Gemini bodies, provider response extensions, dynamic OpenAI-shaped decision outputs, and multipart variants are deliberately documented as open/provider-specific contracts where source has no complete machine schema. No comprehensive OpenAPI document or generic API playground is fabricated. New provider-specific guarantees need source/test evidence and their own schema source before generation can become authoritative.

GitHub's documentation workflow checks source drift and the standalone build without application secrets. The existing router test gate picks up the actual-codec example test; run it locally before opening the PR. The complete repository Rust gate is `just ci`.
