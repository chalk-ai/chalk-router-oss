"""Exercise PR automation against local Git remotes and a fake gh, never GitHub."""

from __future__ import annotations

from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts import fetch_rate_cards
from scripts import update_rate_cards_pr as bot


FAKE_GH = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

path = Path(os.environ["GH_STATE"])
state = json.loads(path.read_text())
args = sys.argv[1:]
state["calls"].append(args[:2])
action = args[1]
if action == "list":
    print(json.dumps([pr for pr in state["prs"] if pr.get("open", True)]))
elif action == "create":
    pr = {"number": len(state["prs"]) + 1, "url": "https://example.test/pr/1",
          "body": Path(args[args.index("--body-file") + 1]).read_text(),
          "isCrossRepository": False, "open": True}
    state["prs"].append(pr)
    print(pr["url"])
elif action in {"edit", "close"}:
    pr = next(pr for pr in state["prs"] if pr["number"] == int(args[2]))
    if action == "edit":
        pr["body"] = Path(args[args.index("--body-file") + 1]).read_text()
    else:
        pr["open"] = False
else:
    raise SystemExit(f"unexpected gh call: {args}")
path.write_text(json.dumps(state))
'''


class UpdateRateCardsPrTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.remote = self.directory / "remote.git"
        self.root = self.directory / "checkout"
        seed = self.directory / "seed"
        seed.mkdir()
        self.git(seed, "init", "-b", "main")
        self.git(seed, "config", "user.name", "Test")
        self.git(seed, "config", "user.email", "test@example.test")
        self.git(seed, "config", "commit.gpgsign", "false")
        (seed / "prices").mkdir()
        self.original = {"openai/test-model": {"input_per_mtok": 1, "output_per_mtok": 2}}
        (seed / bot.CATALOG_PATH).write_text(fetch_rate_cards.render_catalog(self.original))
        (seed / "README.md").write_text("Keep unrelated files.\n")
        self.git(seed, "add", ".")
        self.git(seed, "commit", "-m", "Initial catalog")
        self.git(self.directory, "clone", "--bare", str(seed), str(self.remote))
        self.git(self.directory, "clone", str(self.remote), str(self.root))
        self.git(self.root, "config", "commit.gpgsign", "false")
        # The production URL resolves to a local bare repo for every fetch/push.
        self.git(self.root, "config", f"url.{self.remote}.insteadOf", bot.REMOTE_URL)
        self.base = self.git(self.root, "rev-parse", "HEAD")
        self.state_path = self.directory / "gh.json"
        self.save_state({"prs": [], "calls": []})
        binary_dir = self.directory / "bin"
        binary_dir.mkdir()
        executable = binary_dir / "gh"
        executable.write_text(FAKE_GH)
        executable.chmod(0o755)
        self.environment = patch.dict(os.environ, {
            "PATH": f"{binary_dir}{os.pathsep}{os.environ['PATH']}",
            "GH_STATE": str(self.state_path), "GH_TOKEN": "offline-test-token",
        })
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def git(self, root: Path, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=root, check=True, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        ).stdout.strip()

    def state(self) -> dict:
        return json.loads(self.state_path.read_text())

    def save_state(self, state: dict) -> None:
        self.state_path.write_text(json.dumps(state))

    def candidate(self, price: int = 3) -> dict:
        models = {"openai/test-model": {"input_per_mtok": price, "output_per_mtok": 2}}
        (self.root / bot.CATALOG_PATH).write_text(fetch_rate_cards.render_catalog(models))
        return models

    def update(self, *, dry_run: bool = False) -> None:
        with redirect_stdout(io.StringIO()):
            bot.update(self.root, dry_run=dry_run)

    def branch_sha(self) -> str:
        return self.git(self.remote, "rev-parse", f"refs/heads/{bot.BOT_BRANCH}")

    def assert_no_branch(self) -> None:
        self.assertEqual(self.git(self.remote, "for-each-ref", f"refs/heads/{bot.BOT_BRANCH}"), "")

    def test_unchanged_does_not_open_pr_or_push(self) -> None:
        self.update()
        self.assert_no_branch()
        self.assertEqual(self.state()["prs"], [])

    def test_change_opens_pr_with_only_catalog_changes(self) -> None:
        models = self.candidate()
        (self.root / "README.md").write_text("Unrelated local changes must not be published.\n")
        self.update()
        sha = self.branch_sha()
        self.assertEqual(self.git(self.remote, "rev-parse", "main"), self.base)
        self.assertEqual(self.git(self.remote, "diff", "--name-only", self.base, sha), bot.CATALOG_PATH)
        actual = json.loads(self.git(self.remote, "show", f"{sha}:{bot.CATALOG_PATH}"))
        self.assertEqual(actual["models"], models)
        prs = self.state()["prs"]
        self.assertEqual(len(prs), 1)
        self.assertIn(bot.MARKER, prs[0]["body"])
        self.assertIn("Models with changed rates: 1", prs[0]["body"])
        self.assertEqual(len(self.git(self.root, "worktree", "list", "--porcelain").split("worktree ")) - 1, 1)

    def test_later_change_updates_same_pr(self) -> None:
        self.candidate()
        self.update()
        first_sha = self.branch_sha()
        self.candidate(4)
        self.update()
        self.assertNotEqual(self.branch_sha(), first_sha)
        self.assertEqual(len(self.state()["prs"]), 1)
        self.assertIn(["pr", "edit"], self.state()["calls"])

    def test_identical_rerun_preserves_commit_and_pr(self) -> None:
        self.candidate()
        self.update()
        sha = self.branch_sha()
        self.update()
        self.assertEqual(self.branch_sha(), sha)
        calls = self.state()["calls"]
        self.assertEqual(calls.count(["pr", "create"]), 1)
        self.assertNotIn(["pr", "edit"], calls)

    def test_recovers_after_push_without_pr(self) -> None:
        self.candidate()
        self.update()
        sha = self.branch_sha()
        self.save_state({"prs": [], "calls": []})
        self.update()
        self.assertEqual(self.branch_sha(), sha)
        self.assertEqual(len(self.state()["prs"]), 1)

    def test_rebases_existing_proposal_when_main_prices_change(self) -> None:
        self.candidate()
        self.update()
        other = self.directory / "other-checkout"
        self.git(self.directory, "clone", str(self.remote), str(other))
        main_models = {"openai/test-model": {"input_per_mtok": 2, "output_per_mtok": 2}}
        (other / bot.CATALOG_PATH).write_text(fetch_rate_cards.render_catalog(main_models))
        self.git(other, "add", bot.CATALOG_PATH)
        self.git(other, "-c", "user.name=Test", "-c", "user.email=test@example.test",
                 "-c", "commit.gpgsign=false", "commit", "-m", "Independent price update")
        self.git(other, "push", "origin", "main")
        latest_main = self.git(self.remote, "rev-parse", "main")
        self.update()
        self.assertEqual(self.git(self.remote, "rev-parse", f"{self.branch_sha()}^"), latest_main)
        self.assertEqual(len(self.state()["prs"]), 1)

    def test_invalid_catalog_fails_before_any_remote_operations(self) -> None:
        (self.root / bot.CATALOG_PATH).write_text('{"schema_version":4,"models":{"openai/test":{"input_per_mtok":-1}}}')
        with self.assertRaisesRegex(ValueError, "invalid rate"):
            self.update()
        self.assertEqual(self.state()["calls"], [])
        self.assert_no_branch()

    def test_ignores_fork_pr_with_same_branch_name(self) -> None:
        self.save_state({"prs": [{"number": 99, "body": "Contributor PR", "isCrossRepository": True}], "calls": []})
        self.candidate()
        self.update()
        self.assertEqual(len(self.state()["prs"]), 2)
        self.assertNotIn(["pr", "edit"], self.state()["calls"])

    def test_refuses_unrecognized_pr(self) -> None:
        self.save_state({"prs": [{"number": 1, "body": "Human PR", "isCrossRepository": False}], "calls": []})
        self.candidate()
        with self.assertRaisesRegex(ValueError, "not owned"):
            self.update()
        self.assert_no_branch()

    def test_reverted_upstream_closes_obsolete_bot_pr(self) -> None:
        self.candidate()
        self.update()
        sha = self.branch_sha()
        (self.root / bot.CATALOG_PATH).write_text(fetch_rate_cards.render_catalog(self.original))
        self.update()
        self.assertFalse(self.state()["prs"][0]["open"])
        self.assertEqual(self.branch_sha(), sha)

    def test_dry_run_needs_no_token_and_makes_no_remote_changes(self) -> None:
        self.candidate()
        with patch.dict(os.environ, {"GH_TOKEN": ""}):
            self.update(dry_run=True)
        self.assert_no_branch()
        self.assertEqual(self.state()["calls"], [])

    def test_missing_token_fails_without_remote_changes(self) -> None:
        self.candidate()
        with patch.dict(os.environ, {"GH_TOKEN": ""}):
            with self.assertRaisesRegex(ValueError, "RATE_CARD_GITHUB_TOKEN"):
                self.update()
        self.assert_no_branch()
        self.assertEqual(self.state()["calls"], [])

    def test_concurrent_branch_change_is_not_overwritten(self) -> None:
        self.candidate()
        self.update()
        self.candidate(4)
        original_run = bot.run

        def race(root: Path, *args: str) -> str:
            if args[0] == "git" and "push" in args:
                self.git(self.remote, "update-ref", f"refs/heads/{bot.BOT_BRANCH}", self.base)
            return original_run(root, *args)

        with patch.object(bot, "run", side_effect=race):
            with self.assertRaises(subprocess.CalledProcessError):
                self.update()
        self.assertEqual(self.branch_sha(), self.base)
        self.assertEqual(len(self.git(self.root, "worktree", "list", "--porcelain").split("worktree ")) - 1, 1)


if __name__ == "__main__":
    unittest.main()
