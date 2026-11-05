#!/usr/bin/env -S uv run --script
#
# /// script
# requires-python = ">=3.12"
# dependencies = ["httpx"]
# ///
"""Refresh prices/rate_cards.json from LiteLLM's public price map.

Writes a complete current-price catalog for review, embedding, and public hosting.
Unchanged content is a no-op. --check reports changes without writing.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import pathlib
import sys
import tempfile
from typing import Any

import httpx

# LiteLLM's database is the only machine-readable, broadly-maintained source for
# this; the providers publish prices as HTML marketing pages. Pinned to a raw URL
# and transformed deterministically into the router's current-price schema.
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
RATE_CARDS = REPO_ROOT / "prices" / "rate_cards.json"
SCHEMA_VERSION = 4
RATE_FIELDS = {
    "input_per_mtok", "output_per_mtok", "cache_read_per_mtok", "cache_write_per_mtok",
    "input_per_image_usd", "output_per_image_usd", "input_per_mpixel", "output_per_mpixel",
    "input_image_per_mtok", "output_image_per_mtok",
}


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



def render_catalog(models: dict[str, dict[str, float]]) -> str:
    if not models:
        raise ValueError("catalog must contain priced models")
    lines = ["{", f'  "schema_version": {SCHEMA_VERSION},', '  "models": {']
    items = sorted(models.items())
    for index, (key, prices) in enumerate(items):
        provider, separator, model = key.partition("/")
        if not separator or not provider or not model or any(c.isspace() for c in key):
            raise ValueError(f"invalid rate-card model key: {key}")
        if not prices or not set(prices).issubset(RATE_FIELDS):
            raise ValueError(f"invalid rate fields for {key}")
        if any(isinstance(rate, bool) or not isinstance(rate, (int, float)) or
               not math.isfinite(rate) or rate < 0 for rate in prices.values()):
            raise ValueError(f"invalid rate for {key}")
        comma = "," if index + 1 < len(items) else ""
        lines.append(f"    {json.dumps(key)}: {json.dumps(prices, sort_keys=True)}{comma}")
    return "\n".join([*lines, "  }", "}", ""])


def write_catalog(contents: str, check: bool = False) -> bool:
    """Atomic complete replacement; return whether the file already matched."""
    if RATE_CARDS.exists() and RATE_CARDS.read_text() == contents:
        return True
    if check:
        return False
    RATE_CARDS.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=RATE_CARDS.parent,
                                         prefix=".rate-cards-", delete=False) as target:
            temporary = pathlib.Path(target.name)
            target.write(contents)
        os.replace(temporary, RATE_CARDS)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="report changes without writing")
    args = parser.parse_args()
    response = httpx.get(SOURCE_URL, timeout=60.0, follow_redirects=True)
    response.raise_for_status()
    models = normalize(response.json())
    contents = render_catalog(models)
    unchanged = write_catalog(contents, check=args.check)
    if args.check and not unchanged:
        print("rate card catalog is stale", file=sys.stderr)
        return 1
    print(f"{RATE_CARDS.name}: {'unchanged' if unchanged else 'updated'}; {len(models)} models")
    return 0


if __name__ == "__main__":
    sys.exit(main())
