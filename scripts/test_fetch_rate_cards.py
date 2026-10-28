import json
import pathlib
import tempfile
import unittest
from unittest.mock import patch

from scripts import fetch_rate_cards
from scripts.fetch_rate_cards import (
    append_version,
    catalog_is_current,
    current_models,
    models_by_provider,
    normalize,
    render_provider_shard,
    sparse_history,
)


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


class CheckModeTest(unittest.TestCase):
    def test_catalog_check_materializes_the_latest_model_revisions(self) -> None:
        models = {"openai/gpt-test": {"input_per_mtok": 1.0}}
        document = {
            "schema_version": 3,
            "versions": [
                {
                    "effective_from": "2020-01-01T00:00:00+00:00",
                    "last_verified_at": "2020-01-01T00:00:00+00:00",
                }
            ],
            "models": {
                "openai/gpt-test": [[0, {"input_per_mtok": 1.0}]]
            },
        }

        self.assertTrue(catalog_is_current(document, models))
        self.assertFalse(catalog_is_current(document, {"openai/other": {}}))

    def test_full_snapshots_convert_to_sparse_changes_and_tombstones(self) -> None:
        document = sparse_history(
            {
                "schema_version": 1,
                "cards": [
                    {
                        "effective_from": "2020-01-01T00:00:00+00:00",
                        "last_verified_at": "2020-01-02T00:00:00+00:00",
                        "source": {"url": "source", "sha256": "one"},
                        "models": {
                            "openai/changed": {"input_per_mtok": 1.0},
                            "openai/removed": {"input_per_mtok": 2.0},
                            "openai/unchanged": {"input_per_mtok": 3.0},
                        },
                    },
                    {
                        "effective_from": "2020-02-01T00:00:00+00:00",
                        "last_verified_at": "2020-02-01T00:00:00+00:00",
                        "source": {"url": "source", "sha256": "two"},
                        "models": {
                            "openai/added": {"input_per_mtok": 4.0},
                            "openai/changed": {"input_per_mtok": 5.0},
                            "openai/unchanged": {"input_per_mtok": 3.0},
                        },
                    },
                ],
            }
        )

        self.assertEqual(document["schema_version"], 3)
        self.assertEqual(len(document["versions"]), 2)
        self.assertEqual(len(document["models"]["openai/unchanged"]), 1)
        self.assertEqual(document["models"]["openai/removed"][-1], [1, None])
        self.assertEqual(
            current_models(document),
            {
                "openai/added": {"input_per_mtok": 4.0},
                "openai/changed": {"input_per_mtok": 5.0},
                "openai/unchanged": {"input_per_mtok": 3.0},
            },
        )

    def test_new_version_appends_only_changed_models(self) -> None:
        document = {
            "schema_version": 3,
            "versions": [
                {
                    "effective_from": "2020-01-01T00:00:00+00:00",
                    "last_verified_at": "2020-01-01T00:00:00+00:00",
                }
            ],
            "models": {
                "openai/changed": [[0, {"input_per_mtok": 1.0}]],
                "openai/unchanged": [[0, {"input_per_mtok": 2.0}]],
            },
        }

        counts = append_version(
            document,
            {
                "openai/added": {"input_per_mtok": 3.0},
                "openai/changed": {"input_per_mtok": 4.0},
                "openai/unchanged": {"input_per_mtok": 2.0},
            },
            "2020-02-01T00:00:00+00:00",
            "digest",
        )

        self.assertEqual(counts, (1, 1, 0))
        self.assertEqual(len(document["models"]["openai/unchanged"]), 1)
        self.assertEqual(len(document["models"]["openai/changed"]), 2)
        self.assertEqual(len(document["models"]["openai/added"]), 1)

    def test_provider_shards_drop_only_the_provider_prefix(self) -> None:
        shards = models_by_provider(
            {
                "gemini/models/gemini-test": [[0, {"input_per_mtok": 1.0}]],
                "openrouter/anthropic/claude-test": [
                    [0, {"input_per_mtok": 2.0}]
                ],
            }
        )

        self.assertEqual(
            set(shards["gemini"]),
            {"models/gemini-test"},
        )
        self.assertEqual(
            set(shards["openrouter"]),
            {"anthropic/claude-test"},
        )

    def test_provider_shard_renders_each_model_history_on_one_line(self) -> None:
        rendered = render_provider_shard(
            "openai",
            {
                "changed": [
                    [0, {"input_per_mtok": 1.0}],
                    [2, {"input_per_mtok": 2.0}],
                ],
                "removed": [[0, {"input_per_mtok": 3.0}], [1, None]],
            },
        )

        model_lines = [line for line in rendered.splitlines() if line.startswith("    \"")]
        self.assertEqual(len(model_lines), 2)
        self.assertIn(
            '"changed": [[0, {"input_per_mtok": 1.0}], '
            '[2, {"input_per_mtok": 2.0}]]',
            model_lines[0],
        )
        self.assertIn(
            '"removed": [[0, {"input_per_mtok": 3.0}], [1, null]]',
            model_lines[1],
        )

    def test_monolith_migrates_to_provider_shards_without_changing_history(self) -> None:
        legacy_document = {
            "schema_version": 2,
            "versions": [
                {"effective_from": "2020-01-01T00:00:00+00:00"},
                {"effective_from": "2020-02-01T00:00:00+00:00"},
            ],
            "models": {
                "gemini/models/gemini-test": [
                    [0, {"input_per_mtok": 1.0}]
                ],
                "openai/removed": [[0, {"input_per_mtok": 2.0}], [1, None]],
            },
        }
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            legacy = root / "rate_cards.json"
            directory = root / "rate_cards"
            legacy.write_text(json.dumps(legacy_document))

            with (
                patch.object(fetch_rate_cards, "LEGACY_RATE_CARDS", legacy),
                patch.object(fetch_rate_cards, "RATE_CARDS_DIR", directory),
                patch.object(fetch_rate_cards, "MANIFEST", directory / "manifest.json"),
            ):
                document, migrated = fetch_rate_cards.load_existing()
                self.assertTrue(migrated)
                fetch_rate_cards.write_document(document)
                loaded, migrated = fetch_rate_cards.load_existing()

            self.assertFalse(migrated)
            self.assertFalse(legacy.exists())
            self.assertEqual(loaded, document)
            self.assertTrue((directory / "gemini.json").exists())
            self.assertTrue((directory / "openai.json").exists())


if __name__ == "__main__":
    unittest.main()
