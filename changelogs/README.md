# Changelogs

[Documentation index](../docs/README.md) · [Design records](../rfp/README.md)

Each `YYYY-MM-DD/<who>-<topic>.md` file records a user-visible change at that
time. Later entries may supersede earlier behavior; use the guides for current
setup and operational requirements. Keep these records intact when reorganizing
documentation.

Browse the date directories in [changelogs/](./). To locate a topic locally:

```sh
rg -n 'authentication|quota|migration' changelogs -g '*.md'
```

For a new change, follow [AGENTS.md](../AGENTS.md): one bullet per file describing
what a router or admin UI user notices. Put file names and implementation notes
in the commit or PR description.
