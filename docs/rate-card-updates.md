# Scheduled rate-card updates

The `Update rate cards` GitHub Actions workflow checks LiteLLM daily at 08:17 UTC.
It uses the existing `scripts/fetch_rate_cards.py` generator to produce the
schema-v4 `prices/rate_cards.json` catalog used by the router. The generator and
PR helper are tested before each fetch; malformed prices fail the run.

If prices changed, `scripts/update_rate_cards_pr.py` pushes only that JSON file
to `automation/update-rate-cards` and opens a PR against `main`. Later runs update
the same branch and PR. Identical prices produce no new commits or PRs. If the
latest prices revert to what is already on `main`, the bot closes its obsolete
proposal. Reserve this branch for the updater; it may replace its history.

Review and merge remain manual. Merging updates the public GitHub URL that
routers already fetch. This workflow does not change router refresh settings:
the default interval of zero still means a fetch at startup, with no periodic
background refresh. Updating the private router's embedded fallback is a
separate synchronization step.

## Enable the workflow

1. Merge the workflow and scripts into `main` in `chalk-ai/chalk-router-oss`.
2. Create an organization-approved fine-grained GitHub token for the automation
   account, limited to this repository, with **Contents: read and write** and
   **Pull requests: read and write**. The account must have repository access.
3. Save it as the Actions repository secret `RATE_CARD_GITHUB_TOKEN`. Use an
   expiration and rotate the secret when renewing the token. If your organization
   requires approval for tokens, approve it before running the workflow.
4. In **Actions → Update rate cards → Run workflow**, select `main` to try a run.
   Scheduled runs start after the workflow is on the default branch. GitHub may
   delay scheduled runs or disable schedules after extended repository inactivity.

The workflow's built-in `GITHUB_TOKEN` stays read-only. A separate bot credential
allows PR creation and pushes to trigger the normal PR checks. It is passed only
to the final PR step, after the generator and tests finish, and is never stored
in a remote URL. If your organization uses a GitHub App instead, mint an
installation token in the workflow and pass that token as `GH_TOKEN` to this step.
See GitHub's [workflow trigger documentation](https://docs.github.com/en/actions/how-tos/writing-workflows/choosing-when-your-workflow-runs/triggering-a-workflow).

The workflow runs only in the canonical repository on `main`. No bot credential
is exposed to ordinary pull-request CI. It does not merge PRs or provision cloud
storage, and needs no Buildkite pipeline.

## Local validation

With Python 3.12 and `httpx==0.28.1` installed, run:

```sh
python -m unittest scripts.test_fetch_rate_cards scripts.test_update_rate_cards_pr -v
```

The PR-helper tests use temporary local Git repositories and a fake `gh` command;
they never contact GitHub or require credentials. They cover repeated runs,
updates to an existing PR, recovery after a push, and concurrent branch edits.

To inspect a real generated update locally without pushing or opening a PR:

```sh
git fetch origin main
python scripts/fetch_rate_cards.py
python -m scripts.update_rate_cards_pr --dry-run
git diff -- prices/rate_cards.json
```

The generator writes the local catalog; `--dry-run` compares it with the local
`origin/main` ref and makes no network requests or GitHub changes. Running the
helper without `--dry-run` requires `GH_TOKEN` and publishes to the canonical
repository, so use the workflow for normal operation.
