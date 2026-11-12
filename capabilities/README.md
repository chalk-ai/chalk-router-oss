# Current model capabilities

[`model_capabilities.json`](https://raw.githubusercontent.com/chalk-ai/chalk-router-oss/main/capabilities/model_capabilities.json)
is the public current-model capability catalog for routers with capability
refresh support. The router embeds the same combined file as its offline
fallback.

This directory follows the pricing catalog layout: an outer README, a combined
download artifact, and a folder containing one JSON file per provider.

```text
capabilities/
  README.md
  model_capabilities.json
  model_capabilities/
    anthropic.json
    bedrock.json
    ...
```

## Provider files

[`model_capabilities/`](model_capabilities/) contains the same facts split by
router provider kind. Each file has a `provider` field and a `models` object
keyed by the complete upstream model ID. Nested paths inside model IDs are
preserved.

| Provider | Models |
| --- | ---: |
| [`anthropic`](model_capabilities/anthropic.json) | 24 |
| [`bedrock`](model_capabilities/bedrock.json) | 340 |
| [`cohere`](model_capabilities/cohere.json) | 6 |
| [`fireworks`](model_capabilities/fireworks.json) | 348 |
| [`gemini`](model_capabilities/gemini.json) | 69 |
| [`groq`](model_capabilities/groq.json) | 10 |
| [`mistral`](model_capabilities/mistral.json) | 72 |
| [`openai`](model_capabilities/openai.json) | 217 |
| [`perplexity`](model_capabilities/perplexity.json) | 90 |
| [`together`](model_capabilities/together.json) | 82 |
| [`vertex`](model_capabilities/vertex.json) | 34 |
| [`voyage`](model_capabilities/voyage.json) | 31 |
| [`xai`](model_capabilities/xai.json) | 56 |

The provider files contain 1,379 models across 13 providers. They are
browsable slices of the combined catalog. Routers download the combined file
to get one consistent snapshot.

For example, a provider file has this shape:

```json
{
  "provider": "openai",
  "models": {
    "example": {
      "surfaces": {"chat": "supported"},
      "features": {"tool_calling": "supported"},
      "limits": {"max_input_tokens": 8192, "max_output_tokens": 1024}
    }
  }
}
```

## Combined catalog

Schema version 1 maps complete `<provider>/<model>` keys to sparse model facts:

```json
{
  "schema_version": 1,
  "models": {
    "openai/example": {
      "surfaces": {"chat": "supported"},
      "features": {"tool_calling": "supported"},
      "limits": {"max_input_tokens": 8192, "max_output_tokens": 1024}
    }
  }
}
```

The first slash separates provider from model; all remaining path segments are
part of the upstream model ID. Provider kinds use the router's identifiers,
including `cohere`, `fireworks`, `together`, and `vertex`.

Facts describe model surfaces, input/output modalities, feature support, and
token limits. Support values are `supported`, `unsupported`, or `unknown`.
Omitted facts remain unknown. Numeric token limits are positive integers;
explicit null clears a known limit. The catalog contains current facts without
historical versions or effective dates.

The source is explicit metadata from
[LiteLLM's public model map](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json).
Missing fields stay unknown, and contradictory modality evidence is recorded as
unknown. Curated model rules, live provider discovery, connection declarations,
and endpoint/adapter constraints continue to participate in the router's
effective capability resolution.

## Updates and runtime refresh

Generate the next complete catalog through the collection workflow, then review
additions, corrections, and removals in a normal PR. Publish the combined JSON
and regenerate all provider files from that same snapshot in the same PR.
Regeneration must also remove provider files for providers absent from the new
snapshot.

To verify the split, prefix every provider file's model keys with
`<provider>/` and merge the resulting objects. The merged models must exactly
equal the combined catalog's `models` object, with no duplicates or omissions.

The default runtime URL is the combined file linked above.
`ROUTER_MODEL_CAPABILITY_URL` overrides it,
`ROUTER_MODEL_CAPABILITY_REFRESH_ENABLED=false` uses the embedded fallback,
and `ROUTER_MODEL_CAPABILITY_REFRESH_INTERVAL_SECS` controls periodic fetching.
The default interval is `0` (startup only); `86400` enables daily refreshes.
Failed downloads or invalid catalogs retain the last valid snapshot.
