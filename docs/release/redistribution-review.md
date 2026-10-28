# Redistribution review — pending owner approval

The project is **not approved for public distribution** by this inventory.
The owner selected Apache-2.0 for chalk-router on 2026-10-26; the root LICENSE
and first-party package declarations record that decision. Redistribution rights
for bundled and linked code still require the separate review below.

## Known unresolved decisions

| Component | Evidence at the reviewed dependency revision | Required decision |
| --- | --- | --- |
| Router source | Owner selected Apache-2.0; root LICENSE and package metadata added | License choice resolved; retain applicable copyright and third-party notices |
| chalk-federated-auth | Cargo.lock pins `4f12f0b8e768a7206b2fa6cd7efea54372164f15`; package declares `UNLICENSED`; no license/notice file in that checkout | Obtain explicit redistribution permission/license or replace the dependency before release |
| Copied Chalk protobufs | Repository instructions identify `proto/chalk/auth` and `proto/chalk/utils` as copies from another repository | Confirm rights for the copied definitions, imports, and generated bindings; do not silently alter provenance |
| Other first-party source and generated assets | Source ownership is not established by package metadata | Owner review of release contents and contributor rights |

Public repository visibility is not recorded as evidence of permission. The auth
dependency is included in the runtime graph; removing it from a report would not
remove it from the program.

## Third-party evidence to retain and review

The patched `vendor/reqsign-google` declares Apache-2.0. Its upstream LICENSE and
NOTICE are retained alongside `SECURITY-PATCH.md`, which identifies modifications.
The gRPC health protobuf retains its Apache-2.0 copyright/license header. Preserve
these records and have the release reviewer determine the complete notice set.

`license-inventory.json` records declared licenses for all 472 packages returned
by router Cargo metadata, 176 UI lockfile package entries, 368 documentation npm
entries and 36 documentation Rust packages at the recorded input hashes.
It includes development and platform-specific records, not just shipped runtime
code. License expressions are copied verbatim, with no automatic compatibility
or permission verdict. No missing UI or documentation npm package license declaration was found in
this snapshot. The router and documentation extractor root crates declare Apache-2.0,
consistent with the owner license decision.
This is not a compliance conclusion.

Regenerate with locked dependencies available locally:

```sh
python3 scripts/license-inventory.py
python3 scripts/license-inventory.py --check
```

The generator runs Cargo offline and does not contact providers, download license
texts, change lockfiles, or inspect credentials. Fetch dependencies separately if
Cargo reports a missing cache entry. It excludes absolute local paths.

## Final release review

Before publication, the authorized reviewer must record the auth dependency disposition, copied-source permission, and approved
third-party notices in the repository. Recreate the inventory after integrating
the security and documentation PRs. The independent documentation tools are now included in the declaration inventory.
Also review container base images and OS packages, generated assets, and the exact
binary/UI bundles: their complete shipped contents and notices remain outside
this inventory's scope. Keep the final
artifact hashes and decision record with the release evidence.

Until those decisions are recorded, neither a clean secret scan nor passing CI
satisfies the redistribution gate.
