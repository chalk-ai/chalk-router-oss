# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities privately to [security@chalk.ai](mailto:security@chalk.ai).
The Chalk security team receives reports at this address and coordinates triage
with the Chalk Router maintainers. Do not post exploit details, credentials,
customer data, or private logs in public issues, pull requests, or discussions.

Include the affected version or commit, deployment configuration, a minimal
reproduction using synthetic data, expected and observed behavior, and potential
impact. Redact tokens, personal data, customer names, and signed URLs. Test only
systems you own or are explicitly authorized to test.

## Supported versions

This repository has not published a supported release series yet. During this
pre-release period, report findings against the latest `main` commit and include
the exact commit identifier. Security fixes are developed on `main`.

When a release is published, its release notes must identify the supported
version and upgrade instructions. No older-version backport or response-time
commitment is established by this policy.

## Triage and disclosure

The Chalk security team coordinates assessment, remediation, and disclosure
with the reporter and maintainers. Reports should stay in the private reporting
channel during triage. Advisories should identify affected versions and provide
upgrade or mitigation instructions when a fix is available.

If you need to follow up, reply to the original private email thread so the
team retains the report context. Do not publish sensitive details merely to
request a status update.
