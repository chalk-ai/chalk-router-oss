# Current model prices

[`rate_cards.json`](https://raw.githubusercontent.com/chalk-ai/chalk-router-oss/main/prices/rate_cards.json)
is the public current-price catalog consumed by chalk-router's optional startup
and periodic pricing fetches. The new schema-v4 runtime embeds the same file as its offline fallback.

Schema version 4 maps complete `<provider>/<model>` keys to current rates. The first
slash separates provider from model; all remaining model path segments are preserved.
Rates are USD per million tokens (`input_per_mtok`, `output_per_mtok`,
`cache_read_per_mtok`, `cache_write_per_mtok`, `input_image_per_mtok`,
`output_image_per_mtok`), USD per image (`input_per_image_usd`,
`output_per_image_usd`), or USD per million pixels (`input_per_mpixel`,
`output_per_mpixel`). Omitted rates mean unknown; explicit zero means free.
This catalog estimates public list prices; it is not an invoice or fee schedule.

To propose an update from LiteLLM's public map:

```sh
uv run scripts/fetch_rate_cards.py
uv run --with httpx python -m unittest scripts.test_fetch_rate_cards -v
# Optional live upstream staleness check; does not modify files:
uv run scripts/fetch_rate_cards.py --check
```

Review corrections, additions and removals in a normal PR. The generator writes
a single complete replacement atomically and leaves unchanged content untouched.
No history, effective dates, cloud uploads, credentials, or scheduled update job
are required. The initial catalog preserves all 1,090 current model rates from
chalk-router #414 without an upstream price update.

The legacy provider shards remain for compatibility with existing public router
source until its next source export brings in the schema-v4 runtime. Those builds
continue embedding their previous shards; the single-file catalog is the published
refresh artifact for the new runtime.
