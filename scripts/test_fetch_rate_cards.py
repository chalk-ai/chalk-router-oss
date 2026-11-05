import json
import pathlib
import tempfile
import unittest
from unittest.mock import patch

from scripts import fetch_rate_cards
from scripts.fetch_rate_cards import normalize, render_catalog, write_catalog


def priced(provider: str, **extra: float) -> dict[str, object]:
    return {
        "litellm_provider": provider,
        "mode": "chat",
        "input_cost_per_token": 0.000001,
        "output_cost_per_token": 0.000002,
        **extra,
    }


class NormalizeTest(unittest.TestCase):
    def test_preserves_path_like_upstream_ids_while_removing_transport_prefixes(self) -> None:
        models = normalize(
            {
                "openrouter/anthropic/claude-sonnet-4.5": priced("openrouter"),
                "fireworks_ai/accounts/fireworks/models/deepseek-v3": priced(
                    "fireworks_ai"
                ),
                "bedrock/us.anthropic.claude-sonnet-4-5-v1:0": priced("bedrock"),
                "together_ai/meta-llama/Llama-3.3-70B-Instruct-Turbo": priced(
                    "together_ai"
                ),
                "vertex_ai/gemini-2.5-pro": priced("vertex_ai-language-models"),
            }
        )

        self.assertEqual(
            set(models),
            {
                "openrouter/anthropic/claude-sonnet-4.5",
                "fireworks_ai/accounts/fireworks/models/deepseek-v3",
                "bedrock/us.anthropic.claude-sonnet-4-5-v1:0",
                "together_ai/meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "vertex_ai-language-models/gemini-2.5-pro",
            },
        )

    def test_missing_cache_prices_remain_unknown(self) -> None:
        models = normalize(
            {
                "openai/gpt-test": priced("openai"),
                "anthropic/claude-test": priced(
                    "anthropic",
                    cache_read_input_token_cost=0.0000001,
                    cache_creation_input_token_cost=0.00000125,
                ),
            }
        )

        self.assertNotIn("cache_read_per_mtok", models["openai/gpt-test"])
        self.assertNotIn("cache_write_per_mtok", models["openai/gpt-test"])
        self.assertEqual(
            models["anthropic/claude-test"]["cache_read_per_mtok"], 0.1
        )
        self.assertEqual(
            models["anthropic/claude-test"]["cache_write_per_mtok"], 1.25
        )

    def test_bare_upstream_id_wins_over_conflicting_transport_alias(self) -> None:
        models = normalize(
            {
                "gemini/model": priced("gemini"),
                "model": {
                    **priced("gemini"),
                    "input_cost_per_token": 0.000003,
                },
            }
        )

        self.assertEqual(models["gemini/model"]["input_per_mtok"], 3.0)

    def test_embedding_with_only_an_input_rate_is_retained(self) -> None:
        models = normalize(
            {
                "text-embedding-test": {
                    "litellm_provider": "openai",
                    "mode": "embedding",
                    "input_cost_per_token": 0.00000002,
                }
            }
        )

        self.assertEqual(
            models["openai/text-embedding-test"], {"input_per_mtok": 0.02}
        )

    def test_evaluation_models_include_typesafe_aliases(self) -> None:
        models = normalize(
            {
                "typesafe/jev-latest": {
                    "litellm_provider": "typesafe",
                    "mode": "evaluation",
                    "input_cost_per_token": 0.000000042,
                    "output_cost_per_token": 0,
                },
                "openrouter/typesafe/jev-1.13": {
                    "litellm_provider": "openrouter",
                    "mode": "evaluation",
                    "input_cost_per_token": 0.000000042,
                    "output_cost_per_token": 0,
                },
            }
        )

        self.assertEqual(
            models,
            {
                "openrouter/typesafe/jev-1.13": {
                    "input_per_mtok": 0.042,
                    "output_per_mtok": 0.0,
                },
                "typesafe/jev-latest": {
                    "input_per_mtok": 0.042,
                    "output_per_mtok": 0.0,
                },
            },
        )

    def test_image_prices_retain_each_billing_dimension(self) -> None:
        models = normalize(
            {
                "image-test": {
                    "litellm_provider": "openai",
                    "mode": "image_generation",
                    "input_cost_per_token": 0.000005,
                    "input_cost_per_image_token": 0.00001,
                    "output_cost_per_image_token": 0.00004,
                    "input_cost_per_image": 0.011,
                    "output_cost_per_image": 0.022,
                    "input_cost_per_pixel": 0.0000001,
                    "output_cost_per_pixel": 0.0000002,
                }
            }
        )

        self.assertEqual(
            models["openai/image-test"],
            {
                "input_per_mtok": 5.0,
                "input_image_per_mtok": 10.0,
                "output_image_per_mtok": 40.0,
                "input_per_mpixel": 0.1,
                "output_per_mpixel": 0.2,
                "input_per_image_usd": 0.011,
                "output_per_image_usd": 0.022,
            },
        )

    def test_explicit_zero_moderation_price_is_kept(self) -> None:
        models = normalize(
            {
                "omni-moderation-latest": {
                    "litellm_provider": "openai",
                    "mode": "moderation",
                    "input_cost_per_token": 0,
                    "output_cost_per_token": 0,
                },
                "moderation-without-output": {
                    "litellm_provider": "openai",
                    "mode": "moderation",
                    "input_cost_per_token": 0,
                },
            }
        )

        self.assertEqual(
            models["openai/omni-moderation-latest"],
            {"input_per_mtok": 0.0, "output_per_mtok": 0.0},
        )
        self.assertNotIn("openai/moderation-without-output", models)

    def test_image_without_a_supported_price_is_skipped(self) -> None:
        models = normalize(
            {
                "image-test": {
                    "litellm_provider": "openai",
                    "mode": "image_generation",
                }
            }
        )

        self.assertNotIn("openai/image-test", models)


class CatalogTest(unittest.TestCase):
    def test_checked_in_catalog_is_valid_and_deterministically_rendered(self):
        contents = fetch_rate_cards.RATE_CARDS.read_text()
        document = json.loads(contents)
        self.assertEqual(document["schema_version"], 4)
        self.assertEqual(render_catalog(document["models"]), contents)

    def test_render_current_prices_preserves_zero_missing_rates_and_paths(self):
        models = {"openrouter/anthropic/claude": {"input_per_mtok": 0.0}}
        document = json.loads(render_catalog(models))
        self.assertEqual(document, {"schema_version": 4, "models": models})
        self.assertNotIn("output_per_mtok", document["models"]["openrouter/anthropic/claude"])

    def test_rejects_invalid_catalog_before_publication(self):
        for models in [{}, {"bare": {"input_per_mtok": 1}},
                       {"/a": {"input_per_mtok": 1}}, {"a/": {"input_per_mtok": 1}},
                       {"a/b": {}}, {"a/b": {"typo": 1}},
                       *({"a/b": {"input_per_mtok": rate}} for rate in [-1, float("nan"), float("inf"), True])]:
            with self.assertRaises(ValueError):
                render_catalog(models)

    def test_noop_check_mode_and_atomic_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = pathlib.Path(temporary) / "rate_cards.json"
            with patch.object(fetch_rate_cards, "RATE_CARDS", path):
                original = render_catalog({"openai/a": {"input_per_mtok": 1}})
                updated = render_catalog({"openai/b": {"input_per_mtok": 0}})
                self.assertFalse(write_catalog(original, check=True))
                self.assertFalse(path.exists())
                self.assertFalse(write_catalog(original))
                mtime = path.stat().st_mtime_ns
                self.assertTrue(write_catalog(original))
                self.assertEqual(path.stat().st_mtime_ns, mtime)
                self.assertFalse(write_catalog(updated, check=True))
                self.assertEqual(path.read_text(), original)
                with patch.object(fetch_rate_cards.os, "replace", side_effect=OSError("failed")):
                    with self.assertRaises(OSError):
                        write_catalog(updated)
                self.assertEqual(path.read_text(), original)
                self.assertEqual(list(path.parent.iterdir()), [path])
                self.assertFalse(write_catalog(updated))
                self.assertEqual(path.read_text(), updated)
                self.assertTrue(write_catalog(updated, check=True))


if __name__ == "__main__":
    unittest.main()
