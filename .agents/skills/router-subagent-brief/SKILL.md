---
name: router-subagent-brief
description: Emit the standard preamble for a subagent brief that implements a scoped change in chalk-router (worktree and build rules, reading limits, code-owner constraints, verification gate, commit and report format). Use when delegating implementation work to a cheaper model; fill in the scope and items, keep the preamble verbatim.
---

# router-subagent-brief

A brief has three parts: this preamble (verbatim, with the placeholders
filled), a **Scope** section, and numbered **Items**. Facts you have already
verified go in the items (file:line anchors, the helper's signature, the test
file whose pattern to copy, the list of struct-literal sites to update); each
verified fact saves the agent several tool calls. Judgment stays with the agent.
Scope by intent ("edit `types.rs` only for fields the translation needs"), not
by a file whitelist that the agent cannot cross when the work requires it.

Size a brief for 60–90 minutes of agent time. Split larger work into two
agents rather than one long one: agents past ~150 tool calls stall or drift.

## Preamble (copy, fill the ⟨⟩ placeholders)

```
You are implementing ⟨PR or task name⟩ in chalk-router (Rust/Axum LLM proxy; the
crate and binary are `openai-server`). You are in an isolated git worktree the
harness created for you. Run `pwd` first; every path you read or edit must be
under that worktree root. Never touch the main checkout or other worktrees.

FIRST COMMANDS: ⟨e.g. `git reset --hard <base-branch>` if continuing someone
else's branch⟩ then `git branch -m ⟨branch-name⟩`.

## Repo rules (AGENTS.md)
- Gate: `cargo fmt -p openai-server`, exactly `cargo clippy --locked -p openai-server -- -D warnings`
  (not --all-targets), `cargo nextest run -p openai-server`. Postgres tests skip
  unless TEST_DATABASE_URL is set; run them with `-j1`.
- User-visible changes get `changelogs/⟨YYYY-MM-DD⟩/⟨who⟩-<topic>.md`: one bullet
  per file saying what a user notices, no file names.
- Use the shared helpers listed in AGENTS.md ("Shared helpers") instead of
  hand-rolling; read that section before starting.
- No migrations, no proto changes, never edit ui/src/gen/. If an item needs one,
  skip it and say so in the report.

## Build rules
- Run `just seed-target` once before any cargo command so this worktree has its
  own `target/` (cloned from the main checkout). Do not set CARGO_TARGET_DIR to a
  directory another worktree uses.
- Pass the Bash tool `timeout: 600000` for cargo commands; if one times out,
  re-run it once. Use `cargo check --locked -p openai-server --tests` while
  iterating; run the full nextest only at item boundaries.

## Reading rules (hard limits; your context is small)
- Search with `rg -n` (and `rg --files` to list files), never `grep -r` or
  `find`. `rg -n` first, then read only the matched region; at most 250 lines per
  read; never read a whole file over 300 lines. The large files are listed in
  AGENTS.md ("Working with agents").
- Never read prices/, tests/fixtures/, ui/src/gen/, ui/node_modules/,
  ui/package-lock.json, Cargo.lock, target/, schema.sql.
- Inline test names: `awk '/#\[cfg\(test\)\]/{f=1} f' <file> | rg -n "fn "`.
- Keep bash output small (head, rg -n, sed -n 'a,bp'); scope searches to src/ tests/ ui/src.
- Line numbers in the items may be off by a few lines; confirm with rg.
- After about 150 tool calls, commit what compiles, run the gate, and report
  what is left rather than continuing.

## Code-owner constraints (do not reopen)
- In handlers/routing.rs `try_with_fallbacks_if`, the reservation fold must not
  skip unknown-reservation routes; unknown stays conservative.
- Do not use `finish_failed()` to reconcile partial or non-clean-end streams.
- Do not remove lease expiration or Drop-based cleanup in quota.rs.
- Do not touch `set_key_daily_budget` or its CTE in budget.rs.

## Commits
One commit per item, `git add <specific paths>` (never `git add -A`; nothing
under target/ or .env). Message: one-line summary, blank line, 2–5 lines of
why. End every commit message with this exact trailer line:
Co-Authored-By: ⟨model name⟩ <noreply@anthropic.com>
Do NOT push. Do NOT open PRs.

## Verification (all must pass before you report)
⟨the gate commands; add the -j1 DB command with its TEST_DATABASE_URL when
stores/quota/budgets/usage are touched⟩
Never delete or weaken a test to get green; if a test encodes behaviour you
intentionally changed, update its expectation and list it in the report.

## Report (under 60 lines)
1. Worktree path and branch.
2. `git log --oneline <base>..HEAD`.
3. Per item: done / partial / skipped, one line of what changed, and any
   behaviour change a reviewer must look at.
4. Last 3 lines of each verification command, verbatim.
5. Anything unresolved.
```

## Scope and Items (write per brief)

- **Scope**: which directories the agent may edit and which sibling work is
  concurrently touching nearby files (so it keeps edits away from those regions).
- **Items**: numbered, each with the finding in one sentence, the anchors, the
  fix, and the tests to add. Mark optional items as optional and say what to
  skip when time runs short.
