# Security reporting verification

The owner selected `security@chalk.ai` for private reporting. On 2026-10-28,
read-only review of existing mail confirmed externally originated messages
addressed to that group were delivered to a Chalk maintainer, and existing
correspondence confirmed it as the security reporting channel. No test message
was sent. Correspondence, sender identities, headers, and report contents remain
private; they must not be included in this repository or public CI artifacts.

`SECURITY.md` names the Chalk security team as the triage contact and documents
the current pre-release support scope without promising response times or
backports. Before a supported release is tagged, its release notes must name
the supported version and upgrade path.

For release operations, the security team must maintain appropriate mailbox
membership, backup coverage, and private archive access. Mail delivery evidence
does not prove group archive permissions or current backup coverage. Record
those operational checks in private release evidence before public launch.

Keep the repository private until final release validation and explicit
publication authorization. This document does not authorize a visibility change.
