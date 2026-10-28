# Activate the security reporting policy

`SECURITY.md` is a draft with explicit release blockers. It is not evidence of a
working reporting route. Before publishing it, the release owner must:

1. Choose supported versions and an update/backport policy.
2. Designate primary and backup triage owners for `security@chalk.ai`, the
   private reporting destination selected by the owner on 2026-10-26.
3. Verify that an external researcher can access the route, that reports remain
   private, and that the designated owners receive and can act on reports.
4. Replace every `RELEASE-BLOCKER` paragraph in `SECURITY.md` with the approved
   details and remove its draft banner. Record verification date and responsible
   owner in the private release evidence; do not include test-message contents
   or addresses that are not intended for public use.

A test submission sends a message to others: obtain explicit authorization
before sending it. Viewing configuration or a form does not prove delivery.

GitHub private vulnerability reporting is an option for the future public
repository. GitHub documents this feature for public repositories and describes
which administrators/security managers receive notifications:

- [Configure private vulnerability reporting](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository)
- [Researcher reporting flow](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/report-privately)

The current development repository was confirmed private on October 26, 2026.
Its private-vulnerability-reporting API read returned HTTP 404; that does not
prove an enabled reporting route. Do not change repository visibility to test
this feature. For a fresh public repository, configure and verify the route on
that exact destination as part of the explicitly authorized publication process.
A separately approved and verified security mailbox can also satisfy the route
requirement without relying on GitHub reporting being enabled beforehand.
