"""Exercise PR automation against local Git remotes and a fake gh, never GitHub."""

from __future__ import annotations

import base64
from contextlib import contextmanager, redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

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

def options(arguments):
    assert len(arguments) % 2 == 0, arguments
    return dict(zip(arguments[::2], arguments[1::2]))

if args[:2] == ["auth", "git-credential"]:
    assert args[2:] in (["get"], ["store"], ["erase"]), args
    request = dict(line.split("=", 1) for line in sys.stdin.read().splitlines() if "=" in line)
    # Record which host Git requested, without recording credentials.
    state.setdefault("auth_requests", []).append({
        "operation": args[2], "protocol": request.get("protocol"), "host": request.get("host"),
    })
    path.write_text(json.dumps(state))
    if args[2] == "get" and not os.environ.get("FAKE_GH_AUTH_FAIL"):
        print("username=x-access-token\npassword=" + os.environ["GH_TOKEN"] + "\n")
    raise SystemExit(0)

assert args[0] == "pr", args
action = args[1]
if action == "list":
    filters = options(args[2:])
    assert filters == {
        "--repo": "chalk-ai/chalk-router-oss", "--base": "main",
        "--head": "automation/update-rate-cards", "--state": "open",
        "--json": "number,url,body,isCrossRepository",
    }, filters
    print(json.dumps([pr for pr in state["prs"]
        if pr.get("repo", "chalk-ai/chalk-router-oss") == filters["--repo"]
        and pr.get("base", "main") == filters["--base"]
        and pr.get("head", "automation/update-rate-cards") == filters["--head"]
        and ("open" if pr.get("open", True) else "closed") == filters["--state"]]))
elif action == "create":
    values = options(args[2:])
    assert values["--repo"] == "chalk-ai/chalk-router-oss", values
    assert values["--base"] == "main", values
    assert values["--head"] == "automation/update-rate-cards", values
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
        # Isolate config before the very first Git command. Reject all network
        # transports by default even if a test accidentally uses a public URL.
        environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        environment.update({
            "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_ALLOW_PROTOCOL": "file", "GIT_TERMINAL_PROMPT": "0",
        })
        self.git_environment = patch.dict(os.environ, environment, clear=True)
        self.git_environment.start()
        self.addCleanup(self.git_environment.stop)
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

    def update(self, *, dry_run: bool = False, remote_url: str | None = None, catalog_path: Path | None = None) -> None:
        with redirect_stdout(io.StringIO()):
            bot.update(self.root, dry_run=dry_run, remote_url=remote_url or str(self.remote), catalog_path=catalog_path)

    @contextmanager
    def authenticated_remote(self):
        """Serve real Git smart HTTP on loopback, requiring auth for pushes."""
        self.git(self.remote, "config", "http.receivepack", "true")
        backend = str(Path(self.git(self.root, "--exec-path")) / "git-http-backend")
        authorization = "Basic " + base64.b64encode(b"x-access-token:offline-test-token").decode()
        project_root = str(self.directory)

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def handle_git(self):
                url = urlsplit(self.path)
                push = url.path.endswith("/git-receive-pack") or parse_qs(url.query).get("service") == ["git-receive-pack"]
                if push and self.headers.get("Authorization") != authorization:
                    self.send_response(401)
                    self.send_header("WWW-Authenticate", 'Basic realm="offline-git-test"')
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                response = subprocess.run(
                    [backend], input=body, check=True, capture_output=True,
                    env={**os.environ, "GIT_PROJECT_ROOT": project_root, "GIT_HTTP_EXPORT_ALL": "1",
                         "PATH_INFO": url.path, "QUERY_STRING": url.query, "REQUEST_METHOD": self.command,
                         "CONTENT_TYPE": self.headers.get("Content-Type", ""), "CONTENT_LENGTH": str(len(body)),
                         "REMOTE_USER": "test" if push else ""},
                ).stdout
                headers, body = response.split(b"\r\n\r\n", 1)
                headers = dict(line.decode().split(": ", 1) for line in headers.split(b"\r\n"))
                self.send_response(int(headers.pop("Status", "200 OK").split()[0]))
                for key, value in headers.items():
                    if key.lower() != "content-length":
                        self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = handle_git
            do_POST = handle_git

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch.dict(os.environ, {"GIT_ALLOW_PROTOCOL": "file:http", "NO_PROXY": "127.0.0.1"}):
                yield f"http://127.0.0.1:{server.server_port}/remote.git"
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

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

    def test_network_transports_are_blocked_by_default(self) -> None:
        self.candidate()
        for remote_url in ("https://127.0.0.1:1/blocked.git", "ssh://git@127.0.0.1:1/blocked.git"):
            with self.subTest(remote_url=remote_url):
                with self.assertRaises(subprocess.CalledProcessError) as raised:
                    self.update(remote_url=remote_url)
                self.assertIn("not allowed", raised.exception.stderr)
        self.assert_no_branch()

    def test_inherited_global_push_rewrite_cannot_escape_tests(self) -> None:
        decoy = self.directory / "decoy.git"
        self.git(self.directory, "init", "--bare", str(decoy))
        hostile_config = self.directory / "global.gitconfig"
        hostile_config.write_text(f'[url "{decoy}"]\n\tpushInsteadOf = {bot.REMOTE_URL}\n')
        # Exercise the actual setUp() in a child inheriting a hostile config.
        subprocess.run(
            [sys.executable, "-S", "-m", "unittest",
             "scripts.test_update_rate_cards_pr.UpdateRateCardsPrTests.test_change_opens_pr_with_only_catalog_changes"],
            cwd=Path(__file__).resolve().parent.parent, check=True, capture_output=True, text=True,
            env={**os.environ, "GIT_CONFIG_GLOBAL": str(hostile_config)},
        )
        self.assertEqual(self.git(decoy, "for-each-ref"), "")

    def test_unrelated_and_closed_prs_are_not_selected(self) -> None:
        self.save_state({"prs": [
            {"number": 10, "body": "Different base", "base": "release", "isCrossRepository": False},
            {"number": 11, "body": "Different head", "head": "other-branch", "isCrossRepository": False},
            {"number": 12, "body": "Closed PR", "open": False, "isCrossRepository": False},
        ], "calls": []})
        self.candidate()
        self.update()
        self.assertEqual(len(self.state()["prs"]), 4)
        self.assertIn(bot.MARKER, self.state()["prs"][-1]["body"])

    def test_http_push_invokes_gh_credential_helper(self) -> None:
        self.candidate()
        with self.authenticated_remote() as remote_url:
            self.update(remote_url=remote_url)
        self.branch_sha()
        requests = self.state()["auth_requests"]
        self.assertTrue(any(request["operation"] == "get" and request["protocol"] == "http"
                            and request["host"].startswith("127.0.0.1:") for request in requests))

    def test_failed_http_credentials_prevent_push_and_pr_creation(self) -> None:
        self.candidate()
        with self.authenticated_remote() as remote_url:
            with patch.dict(os.environ, {"FAKE_GH_AUTH_FAIL": "1"}):
                with self.assertRaises(subprocess.CalledProcessError):
                    self.update(remote_url=remote_url)
        self.assert_no_branch()
        self.assertEqual(self.state()["prs"], [])
        self.assertTrue(self.state()["auth_requests"])

    def test_artifact_is_validated_and_only_its_catalog_is_published(self) -> None:
        artifact = self.directory / "downloaded-artifact.json"
        models = {"openai/from-artifact": {"input_per_mtok": 9}}
        artifact.write_text(fetch_rate_cards.render_catalog(models))
        self.update(catalog_path=artifact)
        actual = json.loads(self.git(self.remote, "show", f"{self.branch_sha()}:{bot.CATALOG_PATH}"))
        self.assertEqual(actual["models"], models)
        self.assertEqual(json.loads((self.root / bot.CATALOG_PATH).read_text())["models"], self.original)

    def test_invalid_artifact_fails_before_remote_operations(self) -> None:
        artifact = self.directory / "downloaded-artifact.json"
        artifact.write_text('{"schema_version":4,"models":{}}')
        with self.assertRaisesRegex(ValueError, "must contain"):
            self.update(catalog_path=artifact)
        self.assert_no_branch()
        self.assertEqual(self.state()["calls"], [])

    def test_publisher_imports_without_site_packages(self) -> None:
        source_root = Path(__file__).resolve().parent.parent
        subprocess.run(
            [sys.executable, "-S", "-c", "from scripts import update_rate_cards_pr; import sys; assert 'httpx' not in sys.modules"],
            cwd=source_root, check=True, capture_output=True, text=True,
        )


if __name__ == "__main__":
    unittest.main()
