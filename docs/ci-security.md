# Contribution CI and trusted deployment

`Router CI` runs PR checks on standard GitHub-hosted Ubuntu VMs. Each job gets a
fresh VM; its local Docker daemon is used only for digest-pinned, disposable
PostgreSQL containers. It is not a shared company Docker socket. No self-hosted
runner, cloud identity, registry authentication, serving environment decryption,
shared build cache, or repository secret is configured in the PR workflow.
Checkout uses a read-only token and does not persist Git credentials. Tests use
synthetic fixtures and a localhost database. Paid provider tests remain disabled.
The UI and mock-service checks also run on hosted runners.

The retired Buildkite check files deliberately remain with `on: {}` and a disabled
step. The sync tool updates existing pipelines to stop PR/branch triggers; removing
those files would not delete the pipelines. Deployment remains in separate,
main-only Buildkite pipelines. The sync workflow accepts only the upstream
repository's main branch, including manual dispatches. PR checks never produce
artifacts or caches consumed by deployment.

## Release gate: verify account settings

Repository configuration is not proof of Buildkite isolation. Its bootstrap job
checks out repository code before uploading the pipeline. Conditions in that
pipeline are not a boundary against a malicious PR, which can replace them.
Before accepting public contributions, the CI owner must record the following
verification in META-852:

1. After merging and syncing, both retired pipelines have GitHub PR, branch, and
   tag triggers disabled in Buildkite. Cancel queued/running builds from their
   former configuration. Replace required Buildkite check names in branch
   protection with the GitHub CI checks before retiring them.
2. Every privileged pipeline, including bootstrap and mock deployment, rejects
   fork/PR builds and arbitrary untrusted commits through provider settings,
   API permissions, and agent/cluster policy. A branch named `main` alone is not
   evidence that a commit is trusted. Restrict manual/API deployment access to
   trusted operators and reviewed commits on upstream protected main.
3. Public forks receive read-only GitHub tokens and no secrets. Require approval
   for outside contributors as appropriate; that approval does not make their
   code safe for a privileged runner. Protect main and workflow changes.
4. Run a real fork PR through the hosted checks, including PostgreSQL tests and
   schema/upgrade checks, and record its run URL and the verified account policy.
   Confirm it creates no Buildkite jobs and cannot access company identities,
   secrets, registries, networks, or persistent runners.

The October 26, 2026 review confirmed GitHub Actions is enabled. GitHub's public
fork-approval endpoint could not verify the eventual public configuration while
this repository was private. Buildkite account/agent settings and a public fork
run remain unverified; this change alone does not clear that release gate.

## Updating tool pins

Workflow actions and deployment plugins are pinned to reviewed commit SHAs;
nextest is installed at an exact crates.io version with its lockfile, Rust comes
from `rust-toolchain.toml`, and PostgreSQL uses an immutable manifest digest.
Review upstream changes before updating pins. Composite actions and Buildkite
plugins can install further tools: audit their transitive behavior when updating
them; they are used only on trusted deployment/sync paths.

References: [GitHub-hosted runner isolation](https://docs.github.com/en/actions/concepts/runners/github-hosted-runners)
and [nextest's locked source installation](https://nexte.st/docs/installation/from-source/).
