# CLAUDE.md

This file provides guidance to Claw Code (clawcode.dev) when working with code in this repository.

## Detected stack
- Languages: Rust.
- Frameworks: none detected from the supported starter markers.

## Verification
- From the repository root, run Rust formatting with `scripts/fmt.sh` (or `scripts/fmt.sh --check` for CI-style checks). From this `rust/` directory, the equivalent command is `../scripts/fmt.sh`. Root-level `cargo fmt --manifest-path rust/Cargo.toml` is not the supported formatting command.
- From this `rust/` directory, run Rust verification with `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
- `-D warnings` currently fails on pre-existing lints in `rusty-claude-cli`; add no new ones. For quick loops use `cargo test -p <crate>` (REPL, editor and renderer tests: `cargo test -p rusty-claude-cli --bin claw`).

## Where things are
- `crates/rusty-claude-cli` builds the `claw` binary: `main.rs` (args, REPL loop, provider/tool adapters), `input.rs` (line editor), `render.rs` (streaming markdown), `repl_commands.rs` (REPL-only slash commands).
- `crates/runtime` holds the agent loop (`conversation.rs`), permissions, prompt assembly, sessions, snapshots, background jobs and the turn-time key watcher (`turn_keys.rs`).
- `crates/api` holds provider clients and the HTTP layer; `crates/tools` the tool registry; `crates/documents` Office/PDF/notebook I/O.
- See `AGENTS.md` here and in each crate, and `../ARCHITECTURE.md` for the full picture. User-facing behavior is documented in `../MANUAL.md`; keep it in sync.

## Working agreement
- Prefer small, reviewable changes and keep generated bootstrap files aligned with actual repo workflows.
- Keep shared defaults in `.claw.json`; reserve `.claw/settings.local.json` for machine-local overrides.
- Do not overwrite existing `CLAUDE.md` content automatically; update it intentionally when repo workflows change.
