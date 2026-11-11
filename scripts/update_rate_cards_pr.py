#!/usr/bin/env python3
"""Open or update the rate-card PR. Run from the repo with `python -m scripts.update_rate_cards_pr`."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from scripts import fetch_rate_cards

ROOT = Path(__file__).resolve().parent.parent
REPOSITORY = "chalk-ai/chalk-router-oss"
REMOTE_URL = f"https://github.com/{REPOSITORY}.git"
BASE_BRANCH = "main"
BOT_BRANCH = "automation/update-rate-cards"
CATALOG_PATH = "prices/rate_cards.json"
MARKER = "<!-- rate-card-updater -->"
TITLE = "Update model rate cards"
BASE_REF = "refs/remotes/rate-card-updater/main"
BOT_REF = "refs/remotes/rate-card-updater/update"


def run(root: Path, *args: str) -> str:
    return subprocess.run(
        args, cwd=root, check=True, text=True, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, env={**os.environ, "GIT_TERMINAL_PROMPT": "0"},
    ).stdout.strip()


def validate_catalog(contents: str) -> dict:
    catalog = json.loads(contents)
    if not isinstance(catalog, dict) or catalog.get("schema_version") != fetch_rate_cards.SCHEMA_VERSION:
        raise ValueError("expected a current-schema rate-card catalog")
    models = catalog.get("models")
    if not isinstance(models, dict) or not all(isinstance(rates, dict) for rates in models.values()):
        raise ValueError("expected a model-to-rates map")
    # Reuse the generator's provider/model, rate-field, and numeric validation.
    fetch_rate_cards.render_catalog(models)
    return models


def read_card(root: Path, ref: str) -> str:
    return run(root, "git", "show", f"{ref}:{CATALOG_PATH}") + "\n"


def pr_body(before: dict, after: dict) -> str:
    old, new = set(before), set(after)
    changed = sum(before[key] != after[key] for key in old & new)
    return (
        f"{MARKER}\n\n"
        "Refresh the public router catalog from "
        f"[LiteLLM's price map]({fetch_rate_cards.SOURCE_URL}).\n\n"
        f"- Added models: {len(new - old)}\n"
        f"- Removed models: {len(old - new)}\n"
        f"- Models with changed rates: {changed}\n\n"
        "Review the price changes before merging. Merging publishes the catalog "
        "at the router's configured GitHub URL. Routers pick it up on their next "
        "configured fetch (by default, at startup).\n\n"
        "This PR is maintained automatically; later runs may replace its changes. "
        "Merging remains manual.\n"
    )


def write_branch(root: Path, base: str, contents: str, expected_sha: str, remote_url: str) -> None:
    with tempfile.TemporaryDirectory(prefix="rate-card-update-") as directory:
        checkout = Path(directory) / "checkout"
        run(root, "git", "worktree", "add", "--detach", str(checkout), base)
        try:
            (checkout / CATALOG_PATH).write_text(contents)
            run(checkout, "git", "add", "--", CATALOG_PATH)
            run(
                checkout, "git", "-c", "user.name=rate-card-updater[bot]",
                "-c", "user.email=rate-card-updater@users.noreply.github.com",
                "commit", "-m", TITLE,
            )
            # The token stays in GH_TOKEN, rather than in Git config or a URL.
            # A lease rejects concurrent edits to the bot branch.
            run(
                checkout, "git", "-c", "credential.helper=", "-c",
                "credential.helper=!gh auth git-credential", "push",
                f"--force-with-lease=refs/heads/{BOT_BRANCH}:{expected_sha}",
                remote_url, f"HEAD:refs/heads/{BOT_BRANCH}",
            )
        finally:
            run(root, "git", "worktree", "remove", "--force", str(checkout))


def update(
    root: Path = ROOT, *, dry_run: bool = False, catalog_path: Path | None = None,
    remote_url: str = REMOTE_URL,
) -> None:
    contents = (catalog_path if catalog_path is not None else root / CATALOG_PATH).read_text()
    candidate = validate_catalog(contents)
    if dry_run:
        before = validate_catalog(read_card(root, "origin/main"))
        print("Catalog is unchanged." if before == candidate else pr_body(before, candidate))
        return

    if not os.environ.get("GH_TOKEN"):
        raise ValueError("GH_TOKEN is required; configure the RATE_CARD_GITHUB_TOKEN workflow secret")

    run(root, "git", "fetch", "--no-tags", remote_url, f"+refs/heads/{BASE_BRANCH}:{BASE_REF}")
    base = run(root, "git", "rev-parse", BASE_REF)
    before = validate_catalog(read_card(root, base))
    prs = json.loads(run(
        root, "gh", "pr", "list", "--repo", REPOSITORY, "--base", BASE_BRANCH,
        "--head", BOT_BRANCH, "--state", "open", "--json", "number,url,body,isCrossRepository",
    ))
    # A contributor's fork may have a branch with the same name.
    prs = [pr for pr in prs if not pr["isCrossRepository"]]
    if len(prs) > 1 or any(MARKER not in (pr.get("body") or "") for pr in prs):
        raise ValueError("refusing to modify a PR that is not owned by the rate-card updater")
    existing = prs[0] if prs else None
    if before == candidate:
        if existing:
            run(
                root, "gh", "pr", "close", str(existing["number"]), "--repo", REPOSITORY,
                "--comment", "Closing this automated proposal because the latest generated prices already match main.",
            )
        print("Catalog is unchanged; no update needed.")
        return

    expected_sha = ""
    current = None
    needs_rebase = False
    if run(root, "git", "ls-remote", "--heads", remote_url, f"refs/heads/{BOT_BRANCH}"):
        run(root, "git", "fetch", "--no-tags", remote_url, f"+refs/heads/{BOT_BRANCH}:{BOT_REF}")
        expected_sha = run(root, "git", "rev-parse", BOT_REF)
        current = validate_catalog(read_card(root, expected_sha))
        # If main's prices changed independently, rebuild the proposal on that
        # catalog so an otherwise identical upstream update can still merge.
        needs_rebase = validate_catalog(read_card(root, f"{expected_sha}^")) != before
    if current != candidate or needs_rebase:
        write_branch(root, base, contents, expected_sha, remote_url)
    elif existing:
        print(f"Existing PR already has these prices: {existing['url']}")
        return

    with tempfile.TemporaryDirectory(prefix="rate-card-pr-") as directory:
        body_file = Path(directory) / "body.md"
        body_file.write_text(pr_body(before, candidate))
        if existing:
            run(
                root, "gh", "pr", "edit", str(existing["number"]), "--repo", REPOSITORY,
                "--body-file", str(body_file),
            )
            print(f"Updated {existing['url']}")
        else:
            print(run(
                root, "gh", "pr", "create", "--repo", REPOSITORY, "--base", BASE_BRANCH,
                "--head", BOT_BRANCH, "--title", TITLE, "--body-file", str(body_file),
            ))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="compare with local origin/main without network or writes")
    parser.add_argument("--catalog", type=Path, help="read the generated JSON artifact from this path")
    args = parser.parse_args()
    try:
        update(dry_run=args.dry_run, catalog_path=args.catalog)
    except subprocess.CalledProcessError as error:
        print(f"Rate-card update failed: {error.stderr.strip()}", file=sys.stderr)
        return 1
    except (ValueError, OSError) as error:
        print(f"Rate-card update failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
