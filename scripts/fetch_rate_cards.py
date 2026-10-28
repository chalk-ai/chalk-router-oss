#!/usr/bin/env -S uv run --script
#
# /// script
# requires-python = ">=3.12"
# dependencies = ["httpx"]
# ///
"""Refresh prices/rate_cards/ from the upstream price database.

The router prices every request it proxies and records the result as
`llm.cost.*`. Those prices have to come from somewhere auditable, and they
change: a model gets cheaper, a cache tier is introduced, a model is retired.
The catalog is sharded by provider and keeps a sparse history per model: a new
revision is written only when that model's prices change, and a `null` revision
records its removal. Each model and its complete history occupy one line so a
reviewer can scan or diff a provider without walking a single generated file.

Why history rather than one table:

  * A span from June has to be priceable at June's rates. Overwriting a table
    silently re-prices the past every time a vendor changes a number, and any
    cost trend computed across the change becomes meaningless.
  * Looking up historical prices is still direct: find the model, then binary
    search its usually tiny revision list. No global update log is replayed.

Re-running when nothing has changed is a true no-op: it does not append a
version or rewrite a verification timestamp. `--check` compares the materialized
current catalog and also requires the latest schema.

Usage:

    scripts/fetch_rate_cards.py            # refresh in place
    scripts/fetch_rate_cards.py --check    # exit 1 if a refresh would change
                                           # anything; for CI staleness checks
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import pathlib
import sys
import tempfile
from typing import Any

import httpx

# LiteLLM's database is the only machine-readable, broadly-maintained source for
# this; the providers publish prices as HTML marketing pages. Pinned to a raw URL
# and hashed on every fetch so a card always records exactly what it came from.
SOURCE_URL = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/"
    "model_prices_and_context_window.json"
)

# The provider ids the router can actually route to. Upstream carries ~3400
# models across every provider it knows; narrowing keeps a card reviewable, and
# a provider the router cannot reach could never be priced anyway.
PROVIDERS = {
    "anthropic",
    "bedrock",
    "cohere_chat",
    "fireworks_ai",
    "gemini",
    "groq",
    "mistral",
    "openai",
    "openrouter",
    "together_ai",
    "typesafe",
    "vertex_ai-language-models",
    "xai",
}

# LiteLLM's JSON keys often include the transport/provider prefix even though
# `litellm_provider` already carries it. Chalk's resolved route contains the
# actual model id sent to the provider, without that transport prefix. Strip
# only these exact prefixes and preserve every remaining path segment -- for
# example `openrouter/anthropic/claude-sonnet-4.5` becomes the upstream
# `anthropic/claude-sonnet-4.5`, not merely `claude-sonnet-4.5`.
SOURCE_MODEL_PREFIXES = {
    "anthropic": ("anthropic/",),
    "bedrock": ("bedrock/",),
    "cohere_chat": ("cohere_chat/", "cohere/"),
    "fireworks_ai": ("fireworks_ai/", "fireworks/"),
    "gemini": ("gemini/",),
    "groq": ("groq/",),
    "mistral": ("mistral/",),
    "openai": ("openai/",),
    "openrouter": ("openrouter/",),
    "together_ai": ("together_ai/", "together/"),
    "typesafe": ("typesafe/",),
    "vertex_ai-language-models": (
        "vertex_ai-language-models/",
        "vertex_ai/",
        "vertex/",
    ),
    "xai": ("xai/",),
}

MODES = {
    "chat",
    "responses",
    "embedding",
    "evaluation",
    "image_generation",
    "image_edit",
    # OpenAI publishes moderation at an explicit $0. That zero is the price,
    # distinct from a missing price, which stays unpriced.
    "moderation",
}

# LiteLLM currently records OpenRouter's Jev Router alias as free, while
# OpenRouter publishes its fixed price as unknown. An unknown price must stay
# unpriced rather than being converted into a confidently wrong zero-cost card.
UNPRICED_MODELS = {("openrouter", "typesafe/jev-router")}

# Prices are stored per million tokens: the unit vendors quote, so a reviewer can
# compare a card against a pricing page without arithmetic. Rounded because
# per-token floats reconstitute as 2.9999999999999996 and would churn the diff.
PER_MTOK = 1_000_000
DECIMALS = 6

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
RATE_CARDS_DIR = REPO_ROOT / "prices" / "rate_cards"
MANIFEST = RATE_CARDS_DIR / "manifest.json"
LEGACY_RATE_CARDS = REPO_ROOT / "prices" / "rate_cards.json"
SCHEMA_VERSION = 3


def _per_mtok(per_token: float | int | None) -> float | None:
    if per_token is None:
        return None
    return round(float(per_token) * PER_MTOK, DECIMALS)


def canonical_model_id(provider: str, model_id: str) -> str:
    for prefix in SOURCE_MODEL_PREFIXES.get(provider, ()):
        if model_id.startswith(prefix):
            return model_id.removeprefix(prefix)
    return model_id


def normalize(raw: dict[str, Any]) -> dict[str, dict[str, float]]:
    """Upstream's per-token entries, as our per-MTok schema.

    Keyed `<provider>/<model>` because the same model is sold at different
    prices by different providers -- Bedrock and Anthropic both serve Claude --
    and a bare model id would silently resolve to whichever happened to be
    written last.

    A model missing an input or output price is skipped rather than defaulted:
    the router omits `llm.cost.*` for an unpriced model, which is recoverable,
    whereas a fabricated zero is indistinguishable from a free request.
    """
    models: dict[str, dict[str, float]] = {}
    model_priorities: dict[str, int] = {}
    for model_id, entry in raw.items():
        if not isinstance(entry, dict):
            continue
        provider = entry.get("litellm_provider")
        if provider not in PROVIDERS or entry.get("mode") not in MODES:
            continue

        prices: dict[str, float] = {}
        for destination, source in {
            "input_per_mtok": "input_cost_per_token",
            "output_per_mtok": "output_cost_per_token",
            "input_image_per_mtok": "input_cost_per_image_token",
            "output_image_per_mtok": "output_cost_per_image_token",
            "input_per_mpixel": "input_cost_per_pixel",
            "output_per_mpixel": "output_cost_per_pixel",
        }.items():
            price = _per_mtok(entry.get(source))
            if price is not None:
                prices[destination] = price

        for destination, source in {
            "input_per_image_usd": "input_cost_per_image",
            "output_per_image_usd": "output_cost_per_image",
        }.items():
            price = entry.get(source)
            if price is not None:
                prices[destination] = round(float(price), DECIMALS)

        # Chat/Responses requests may consume both token directions, so a
        # partial entry cannot safely price them. Embeddings and image models
        # legitimately expose only the dimensions they bill (input tokens,
        # generated images, pixels, or image tokens).
        if entry.get("mode") in {"chat", "responses", "evaluation", "moderation"} and not {
            "input_per_mtok",
            "output_per_mtok",
        }.issubset(prices):
            continue
        if not prices:
            continue

        # An absent cache field means LiteLLM does not tell us the cache rate.
        # It does not prove that cache tokens use the normal input rate. Omit
        # the field so runtime pricing can decline to estimate a cached request
        # instead of fabricating a price.
        cache_read = _per_mtok(entry.get("cache_read_input_token_cost"))
        cache_write = _per_mtok(entry.get("cache_creation_input_token_cost"))
        if cache_read is not None:
            prices["cache_read_per_mtok"] = cache_read
        if cache_write is not None:
            prices["cache_write_per_mtok"] = cache_write

        canonical_id = canonical_model_id(provider, model_id)
        if (provider, canonical_id) in UNPRICED_MODELS:
            continue
        # A bare source key already equals the model Chalk sends upstream and
        # is therefore more authoritative than a LiteLLM transport alias that
        # normalizes to the same id. This resolves the few duplicate aliases in
        # the source deterministically while same-confidence conflicts still
        # fail closed.
        priority = int(canonical_id == model_id)
        key = f"{provider}/{canonical_id}"
        previous = models.get(key)
        if previous is not None:
            previous_priority = model_priorities[key]
            if priority < previous_priority:
                continue
            if priority == previous_priority and previous != prices:
                raise ValueError(
                    f"conflicting LiteLLM prices normalize to {key}: "
                    f"{previous!r} != {prices!r}"
                )
        models[key] = prices
        model_priorities[key] = priority

    return dict(sorted(models.items()))


def sparse_history(full_snapshots: dict[str, Any]) -> dict[str, Any]:
    """Convert schema v1 full snapshots into model-centric sparse histories."""
    versions: list[dict[str, Any]] = []
    histories: dict[str, list[list[Any]]] = {}
    previous: dict[str, dict[str, float]] = {}
    missing = object()

    for version_index, card in enumerate(full_snapshots.get("cards", [])):
        effective_from = card["effective_from"]
        versions.append(
            {
                "effective_from": effective_from,
                "last_verified_at": card["last_verified_at"],
                "source": card["source"],
            }
        )
        current = card["models"]
        for model in sorted(set(previous) | set(current)):
            old_prices = previous.get(model, missing)
            new_prices = current.get(model, missing)
            if old_prices == new_prices:
                continue
            histories.setdefault(model, []).append(
                [version_index, None if new_prices is missing else new_prices]
            )
        previous = current

    return {
        "schema_version": SCHEMA_VERSION,
        "versions": versions,
        "models": dict(sorted(histories.items())),
    }


def load_existing() -> tuple[dict[str, Any], bool]:
    """Load the sharded catalog, migrating either legacy monolithic schema."""
    if MANIFEST.exists():
        manifest = json.loads(MANIFEST.read_text())
        schema_version = manifest.get("schema_version")
        if schema_version != SCHEMA_VERSION:
            raise SystemExit(
                f"{MANIFEST} is schema_version {schema_version}, "
                f"this script writes {SCHEMA_VERSION}"
            )

        providers = manifest.get("providers", [])
        if providers != sorted(set(providers)):
            raise SystemExit("provider shards in manifest must be unique and sorted")

        models: dict[str, list[list[Any]]] = {}
        for provider in providers:
            if provider not in PROVIDERS:
                raise SystemExit(f"unsupported provider shard in manifest: {provider}")
            shard_path = RATE_CARDS_DIR / f"{provider}.json"
            shard = json.loads(shard_path.read_text())
            if shard.get("provider") != provider:
                raise SystemExit(
                    f"{shard_path} declares provider {shard.get('provider')!r}"
                )
            for model, revisions in shard.get("models", {}).items():
                models[f"{provider}/{model}"] = revisions

        return {
            "schema_version": SCHEMA_VERSION,
            "versions": manifest["versions"],
            "models": dict(sorted(models.items())),
        }, False

    if LEGACY_RATE_CARDS.exists():
        document = json.loads(LEGACY_RATE_CARDS.read_text())
        schema_version = document.get("schema_version")
        if schema_version == 1:
            return sparse_history(document), True
        if schema_version == 2:
            document["schema_version"] = SCHEMA_VERSION
            return document, True
        raise SystemExit(
            f"{LEGACY_RATE_CARDS} is schema_version {schema_version}, "
            f"this script writes {SCHEMA_VERSION}"
        )

    return {"schema_version": SCHEMA_VERSION, "versions": [], "models": {}}, False


def models_by_provider(
    models: dict[str, list[list[Any]]],
) -> dict[str, dict[str, list[list[Any]]]]:
    shards: dict[str, dict[str, list[list[Any]]]] = {}
    for key, revisions in models.items():
        provider, separator, model = key.partition("/")
        if not separator or provider not in PROVIDERS:
            raise ValueError(f"invalid rate-card model key: {key}")
        shards.setdefault(provider, {})[model] = revisions
    return {
        provider: dict(sorted(provider_models.items()))
        for provider, provider_models in sorted(shards.items())
    }


def render_manifest(document: dict[str, Any], providers: list[str]) -> str:
    manifest = {
        "schema_version": SCHEMA_VERSION,
        "versions": document["versions"],
        "providers": providers,
    }
    return json.dumps(manifest, indent=2, sort_keys=False) + "\n"


def render_provider_shard(
    provider: str, models: dict[str, list[list[Any]]]
) -> str:
    """Render one complete model history per physical line."""
    lines = ["{", f'  "provider": {json.dumps(provider)},', '  "models": {']
    entries = list(models.items())
    for index, (model, revisions) in enumerate(entries):
        comma = "," if index + 1 < len(entries) else ""
        lines.append(
            f"    {json.dumps(model)}: "
            f"{json.dumps(revisions, separators=(', ', ': '))}{comma}"
        )
    lines.extend(["  }", "}"])
    return "\n".join(lines) + "\n"


def write_document(document: dict[str, Any]) -> None:
    shards = models_by_provider(document["models"])
    providers = list(shards)
    rendered = {
        "manifest.json": render_manifest(document, providers),
        **{
            f"{provider}.json": render_provider_shard(provider, models)
            for provider, models in shards.items()
        },
    }

    # The first schema-v3 write replaces the old file with a directory. Build
    # every shard before removing the legacy file so a rendering failure leaves
    # the checked-in catalog untouched.
    if LEGACY_RATE_CARDS.exists():
        temporary = pathlib.Path(
            tempfile.mkdtemp(prefix=".rate-cards-", dir=RATE_CARDS_DIR.parent)
        )
        for filename, contents in rendered.items():
            (temporary / filename).write_text(contents)
        LEGACY_RATE_CARDS.unlink()
        temporary.rename(RATE_CARDS_DIR)
        return

    RATE_CARDS_DIR.mkdir(parents=True, exist_ok=True)
    for filename, contents in rendered.items():
        path = RATE_CARDS_DIR / filename
        if not path.exists() or path.read_text() != contents:
            path.write_text(contents)

    expected = set(rendered)
    for provider in PROVIDERS:
        filename = f"{provider}.json"
        path = RATE_CARDS_DIR / filename
        if filename not in expected and path.exists():
            path.unlink()


def catalog_is_current(document: dict[str, Any], models: dict[str, Any]) -> bool:
    return current_models(document) == models


def current_models(document: dict[str, Any]) -> dict[str, dict[str, float]]:
    return {
        model: revisions[-1][1]
        for model, revisions in document.get("models", {}).items()
        if revisions and revisions[-1][1] is not None
    }


def append_version(
    document: dict[str, Any],
    models: dict[str, dict[str, float]],
    effective_from: str,
    digest: str,
) -> tuple[int, int, int]:
    previous = current_models(document)
    added = sorted(set(models) - set(previous))
    removed = sorted(set(previous) - set(models))
    changed = sorted(
        model
        for model in set(models) & set(previous)
        if models[model] != previous[model]
    )

    version_index = len(document["versions"])
    document["versions"].append(
        {
            "effective_from": effective_from,
            "last_verified_at": effective_from,
            "source": {"url": SOURCE_URL, "sha256": digest},
        }
    )
    histories = document["models"]
    for model in added + changed:
        histories.setdefault(model, []).append([version_index, models[model]])
    for model in removed:
        histories[model].append([version_index, None])
    document["models"] = dict(sorted(histories.items()))
    return len(changed), len(added), len(removed)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit 1 if refreshing would change the file, without writing",
    )
    args = parser.parse_args()

    response = httpx.get(SOURCE_URL, timeout=60.0, follow_redirects=True)
    response.raise_for_status()
    payload = response.content
    digest = hashlib.sha256(payload).hexdigest()
    models = normalize(json.loads(payload))
    if not models:
        raise SystemExit("upstream yielded no priced models; refusing to write")

    now = dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat()
    document, migrated = load_existing()

    if args.check:
        if not migrated and catalog_is_current(document, models):
            print("rate card catalog is up to date")
            return 0
        print("rate card catalog is stale", file=sys.stderr)
        return 1

    if catalog_is_current(document, models):
        if not migrated:
            print(
                f"{RATE_CARDS_DIR.relative_to(REPO_ROOT)}/: "
                f"unchanged; {len(models)} models"
            )
            return 0
        action = f"converted to provider shards; {len(models)} current models"
    else:
        changed, added, removed = append_version(
            document, models, now, digest
        )
        if len(document["versions"]) == 1:
            action = f"created sparse history with {added} models"
        else:
            action = (
                f"appended version: {changed} changed, "
                f"{added} added, {removed} removed"
            )

    write_document(document)
    print(f"{RATE_CARDS_DIR.relative_to(REPO_ROOT)}/: {action}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
