---
title: Decision APIs
---

The router exposes three distinct decision request formats. They route through capable decision models; the paths do not imply identical request schemas or complete cross-provider equivalence.

| Path | Input | Output |
| --- | --- | --- |
| `POST /v1/alpha/decisions` | [DecisionsRequest](/reference/decisions-types/#decisionsrequest) | [DecisionsResponse](/reference/decisions-types/#decisionsresponse) |
| `POST /v1/decisions` | [OpenAiDecisionsRequest](/reference/openai-decisions-types/#openaidecisionsrequest) | OpenAI-shaped answer array in question order |
| `POST /v1/systemone` | [SystemOneRequest](/reference/decisions-types/#systemonerequest) | [SystemOneResponse](/reference/decisions-types/#systemoneresponse) |

## Router decisions

`model`, `state`, and a named `questions` map are required. [DecisionQuestion](/reference/decisions-types/#decisionquestion) is tagged by `type`: `noul`, `choice`, `score`, or `box`. Choice criteria map labels to optional descriptions; score criteria are an ordered array. Optional assets provide images associated with placeholders; support depends on the backend. `session_id` has explicit validation. See the exact generated declarations for tagged union variants and nested shapes.

[The validated example](/reference/examples/#router-decision-questions) classifies a synthetic task. A syntactically valid question is not proof that every decision model supports it; box questions require a capable model and have no OpenAI representation.

## OpenAI-shaped decisions

This ingress accepts `model`, `input`, and an ordered `questions` array. Questions are `predicate`, `choice`, or `score`; choices have typed string/boolean values, scores have labeled levels. Translation maps predicates to canonical `noul`, choices to criteria maps, and score levels to positions. Answers are reconstructed in request question order. Typed choice values colliding after string conversion are rejected.

Non-user input items, unsupported question asset references, and unsupported semantics fail translation. Prompt-cache breakpoints are omitted by canonical conversion. Extra ingress fields are forwarded only to upstreams speaking that ingress format; do not assume arbitrary provider extensions survive translation. [Example](/reference/examples/#openai-shaped-decisions).

## TypeSafe-shaped requests

The System One request uses `state`, a named question map, and optional assets. The handler supplies its TypeSafe routing prefix; use the model form described by your configured connection rather than copying a prefixed model from the other two surfaces. [Example](/reference/examples/#typesafe-shaped-decisions).

No collection pagination or generic decision SSE contract is implemented by these handlers. Usage fields differ between output formats. OpenAI-shaped outputs are constructed dynamically by the translator; the generated Rust reference intentionally does not invent a typed output schema for them.

Sources: [decision types and validation](https://github.com/chalk-ai/chalk-router/blob/main/src/decisions.rs), [OpenAI conversion](https://github.com/chalk-ai/chalk-router/blob/main/src/translation/decisions_openai.rs), [compatibility tests](https://github.com/chalk-ai/chalk-router/blob/main/tests/integration/decisions_compat_test.rs).
