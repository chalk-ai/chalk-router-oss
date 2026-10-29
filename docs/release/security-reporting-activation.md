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

Read-only Google Groups verification on 2026-10-28 confirmed four recipients
subscribed to each message, including a group owner. Organization policy blocks
external archive access, membership is invitation-only, and external membership
is disabled. External email posting is enabled. The team must maintain recipient
coverage and monitor the spam moderation queue; no response-time SLA is implied.
Private operational evidence retains recipient details outside this repository.

Keep the repository private until final release validation and explicit
publication authorization. This document does not authorize a visibility change.
