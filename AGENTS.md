# AGENTS.md — modulex-mcp

Guidance for AI agents contributing to this repository.

## Ground Rules

1. Never commit directly to `main` — all changes land through pull
   requests with CI gates.
2. Run the full check suite before pushing: `cargo fmt --check`,
   `cargo clippy -- -D warnings`, `cargo test` (the pre-push hook
   enforces this; never bypass it).
3. Secrets are referenced, never stored — routines carry credential
   *references* only.
4. Every bug fix includes a regression test that failed before the fix.

## Crate README Rule

Every crate in this workspace gets its own `README.md` — crates.io renders
it as the crate's front page, and `cargo package` fails if a declared
`readme` file is missing.

1. **Existence:** a new crate lands with a `README.md` in its crate root
   (short: what it is, what it does, license).
2. **Freshness:** every version bump of a crate includes a review of that
   crate's README. Update it to match the released behavior — new features,
   changed CLI flags, removed APIs. If a bump PR leaves the README
   untouched, the PR body must say why.

Treat a version bump without a README review as an incomplete change, the
same way a bug fix without a regression test is incomplete.
