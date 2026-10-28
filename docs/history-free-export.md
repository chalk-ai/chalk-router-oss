# Prepare a history-free source snapshot

Keep the development repository private. Editing PR descriptions, deleting a
branch, or force-pushing main does not establish removal of GitHub edit history,
retained PR references, cached objects, attachments, or copies held elsewhere.
See [GitHub's removal guidance](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/removing-sensitive-data-from-a-repository).

For a fresh public repository, prepare a new local repository from an explicitly
reviewed commit instead of cloning or pushing the development repository:

```sh
python3 scripts/export-source-snapshot.py \
  --source /path/to/private-checkout \
  --ref REVIEWED_COMMIT_SHA \
  --output /path/to/quarantine/new-source \
  --evidence /path/to/private-review/export-manifest.json
```

Output and evidence must not exist; their parent directories must already exist.
The evidence is private and must remain outside the export. The script copies
only committed tree entries, preserves bytes/modes and confined relative symlinks,
rejects submodules/escaping links, and creates one root commit with a generic
identity/message. It verifies the tree ID against the source, a single main ref,
and no remotes or parent history. It never reads or copies GitHub discussions,
legacy refs, old commit objects, author identities, untracked files, or working-tree
edits. It does not push, publish, rewrite the source, or change repository visibility.

**This is history isolation, not content sanitization.** Current source may still
contain encrypted secret bundles, company deployment configuration, internal
links, or confidential data. Those files are deliberately not silently rewritten
or excluded by this tool. First prepare a reviewed source-only release commit
(META-861), then run this tool and the exact-artifact checks in META-867. Do not
publish its output merely because the history checks passed.

Before publication, verify the exact exported tree and all refs/objects with
TruffleHog and the confidential-name/identifier review; inspect packaged images,
source archives, license/third-party rights, and secure-default runtime checks.
If creating a new GitHub repository, do not import legacy issues, PRs, discussions,
actions logs/artifacts, wiki, release attachments, tags, branches, or mirrors.
Inspect destination metadata after creation and obtain explicit publication
authorization. Retain the original repository privately until release verification
is complete. Preserving its existing identity instead requires separate GitHub
edit-history/cache/reference cleanup and cannot be approved by this script.

Run the isolation regression tests with:

```sh
python3 -m unittest discover -s scripts -p test_export_source_snapshot.py -v
```
