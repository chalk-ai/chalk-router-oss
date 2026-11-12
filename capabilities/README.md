# Current model capabilities

[`model_capabilities.json`](https://raw.githubusercontent.com/chalk-ai/chalk-router-oss/main/capabilities/model_capabilities.json)
is the public current-model capability catalog for routers with capability
refresh support. The router embeds the same combined file as its offline
fallback.

```text
capabilities/
  README.md
  model_capabilities.json
```

## Catalog format

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
additions, corrections, and removals in a normal PR. Publish the same combined
JSON used by the router's embedded fallback. The catalog is replaced as one
complete snapshot.

The default runtime URL is the combined file linked above.
`ROUTER_MODEL_CAPABILITY_URL` overrides it,
`ROUTER_MODEL_CAPABILITY_REFRESH_ENABLED=false` uses the embedded fallback,
and `ROUTER_MODEL_CAPABILITY_REFRESH_INTERVAL_SECS` controls periodic fetching.
The default interval is `0` (startup only); `86400` enables daily refreshes.
Failed downloads or invalid catalogs retain the last valid snapshot.
