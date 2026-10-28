# Security policy

> Draft — not ready for publication. The owner must resolve the release blockers
> below and verify private report delivery before this policy is published.

## Supported versions

RELEASE-BLOCKER: The owner must name the supported release series or commits and
state the update/backport policy. This draft does not promise support for any
version or establish a response-time commitment.

## Reporting a vulnerability

Do not post exploit details, credentials, customer data, or private logs in public
issues, pull requests, discussions, or comments.

Report vulnerabilities privately to [security@chalk.ai](mailto:security@chalk.ai),
the reporting address selected by the owner.

RELEASE-BLOCKER: Verify that this mailbox accepts external reports and delivers
them privately to the designated triage owners before publishing this policy.

Include the affected version or commit, deployment configuration, a minimal
reproduction using synthetic data, expected and observed behavior, and the
potential impact. Redact tokens, personal data, customer names, and signed URLs.
Test only systems you own or are explicitly authorized to test.

## Triage and disclosure

RELEASE-BLOCKER: Name the responsible triage team and its backup/escalation route.
Confirm that both can receive the private report and coordinate a fix and
advisory. Publish acknowledgment/update expectations only after the owner agrees
to them; no service-level commitment is established by this draft.

The finalized process should coordinate disclosure with the reporter, assess
which versions are affected, and provide upgrade or mitigation instructions when
a fix is available. Reports should remain in the verified private channel during
triage.
