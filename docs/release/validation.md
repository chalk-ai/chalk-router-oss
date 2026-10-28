# Combined release validation

This branch combines the open security, distribution and documentation changes
for testing. It does not merge the component PRs, approve redistribution, deploy
software, or authorize publication. Treat it as a draft release candidate until
all gates below have evidence for the exact artifact being published.

## Reproducible checks

- Repository Rust gate: formatting, exact clippy, nextest. PostgreSQL suites need
  a disposable database; local tests without one do not exercise their DB bodies.
- `python3 scripts/check-anonymous-admin.py --binary <built-router>` starts an
  isolated process with synthetic settings. It probes all 47 router admin RPCs
  and an unknown RPC via HTTP/2 gRPC and HTTP/1.1 gRPC-Web, checks legacy admin/debug
  prefixes, and checks authenticated model listing. It is an anonymous boundary
  test; the JWT grant/tenant matrix remains in the Rust integration suite.
- UI: install locked dependencies, generate Next types, typecheck, run tests,
  production build, then `npm run test:auth`. The smoke test uses synthetic
  credentials and a loopback backend, not provider or production credentials.
- `just docs-check` verifies source freshness, builds the API site and checks
  links and source line bounds. Regenerate after source/proto changes.
- `python3 scripts/check-container-hardening.py` validates rendered containment,
  writable mounts and explicit ingress opt-ins. Hosted image jobs separately
  exercise actual read-only/no-capabilities startup and database migration.
- `python3 scripts/check-public-distribution.py` rejects private asset filenames.
  It is not a content scanner. `python3 scripts/test_export_source_snapshot.py`
  verifies history-free export behavior using synthetic repositories.

The combined local run passed 1,584 Rust tests; PostgreSQL and paid live tests
were environment gated. The API site built 40 pages and checked 3,972 links/assets.
The anonymous process check passed 96 RPC probes. Built UI checks passed all 30
BFF routes plus session, forwarding, CSRF and missing-config cases. Hosted results
for the combined commit must be checked separately; green component PRs do not
prove their combination works on Linux.

## Exact artifact evidence

Create a fresh quarantine using `scripts/export-source-snapshot.py`, recording
its source commit, exported tree/root commit and per-file hashes in PRIVATE
release evidence outside the export. Never export the development Git directory,
old branches, tags, PR refs or GitHub discussion history.

Run TruffleHog with verification disabled against the exact export's Git history
and files and against built UI/docs/image artifacts. Preserve raw results
privately, review findings without copying credential values into tickets, and
record the scan version, exit codes, hashes and disposition. A detector match is
not automatically a production secret, and zero findings are not proof of absence.

Review the exact artifacts against the approved confidential-name/identifier
inventory, retaining known historical names absent from the active environment
list. Review generic-word/model-name matches in context. Active environment data
does not cover archived/deleted customers or unknown aliases; an owner must review
that gap. A later code/build change invalidates evidence for the changed artifact.

## Unresolved release gates

- Owner-selected project license and permission for the explicitly UNLICENSED
  auth dependency, copied sources and final notices: see
  [redistribution review](redistribution-review.md).
- Supported versions, responsible triage owner and verified private report
  delivery: see [reporting activation](security-reporting-activation.md).
- Exact source, Git, packaged image/UI/docs and metadata scans, followed by
  review of any findings and completion of combined Linux/PostgreSQL checks.
- Do not import private PR/issue comments, edit history, signed attachments,
  CI logs or old integration settings into the fresh public repository.
  Sanitizing current descriptions does not purge historical revisions/caches.
  Existing Buildkite account/fork/agent settings are not established by source
  changes; the public destination must use the credential-free CI independently.
- Validate default telemetry behavior using the content-disabled integration
  checks and rendered configuration. No company exporter is selected and content
  capture is off by default. Opt-in exporters/content capture need separate review.
- Obtain explicit authorization for publication only after the remaining release
  decisions and exact-artifact checks are complete. Do not change visibility as
  a way to test readiness.

## Linux image-layer scanning

The backend and UI/Caddy image jobs scan their locally built image IDs with
checksum-pinned TruffleHog 3.99.7, verification disabled. Every new detector finding
fails the scan pending review; scanner failures cannot count as clean results.
CI retains seven-day redacted reports with source commit, image ID, file/layer
location and candidate fingerprints. It does not upload raw candidate values.
The local Darwin UI findings do not clear findings in a different Linux image.
Six synthetic scanner tests check error handling and candidate redaction.

The scanner wrapper also rejects logged extraction/reading errors and missing or
zero-byte completion statistics, even if the scanner process exits zero. The
initial local Darwin libvips scan logged a decoding error and is incomplete;
reviewing its four detector matches does not clear that coverage gap. Each Linux
job scans a saved archive of the exact local image ID and records the archive hash.

Reviewed dependency findings are recorded in `scripts/reviewed-image-findings.json`.
Each entry requires an exact detector, absolute image path and candidate fingerprint,
with dated rationale, evidence and a review deadline. Findings remain visible in the
report with their disposition; a reviewed-finding result is not a zero-finding scan.
The initial entries cover libvips HTML entity-table bytes and the exact public Yarn
README build-status badge token. Public provenance alone is not a blanket exemption
for keys or credentials. No credential verification or revocation is claimed.
Changed candidates/paths, expired reviews and scanner errors continue to fail.

Backend image finding review additionally identified Debian package-description/file
checksums, GnuTLS upstream cryptographic self-test constants, and compiler-generated
anonymous-symbol hashes in the router ELF `.strtab` section. Exact-byte matching and
section offsets were checked against the CI binary; review entries record its hash.
The temporary encrypted diagnostic capture was removed after this investigation.
These are exact candidate/path reviews, not exemptions for binary files or private-key
detectors. Any new candidate or scanner error still blocks the image scan.
