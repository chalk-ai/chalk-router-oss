# Chalk Router API documentation

Local, source-derived Starlight site. Start at the rendered home page or
[`src/content/docs/maintenance.md`](src/content/docs/maintenance.md) for framework
rationale, generation boundaries, and update rules.

From the repository root:

```sh
just docs-install
just docs-generate
just docs-dev
just docs-check
just docs-test-examples
```

`just docs-preview` serves the built site. Both servers bind loopback. No service
account, router process, database, hosted service, or provider credential is needed
for site generation/build. Node 22.23.4+, npm, Rust/Cargo and just are needed
for the full documentation check. Router example tests use the repository's normal
Rust dependencies. Generated references are committed so `npm ci && npm run build`
inside this directory works with only Node/npm.

Edit guides and the behavior overlay; regenerate references instead of editing them.
The initial security audit is main `246837f1` (2026-10-26), before PRs #353/#354 merge.
