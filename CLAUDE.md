# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Detected stack
- Languages: Rust (canonical `rust/` workspace); Python stdlib for the `src/` porting workspace and `tests/`.
- Frameworks: none detected from the supported starter markers.

## Verification
- Formatting from repo root: `scripts/fmt.sh --check` (apply with `scripts/fmt.sh`). Do not run `cargo fmt --manifest-path rust/Cargo.toml`.
- From `rust/`: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.
  - `-D warnings` currently fails on pre-existing lints in `rusty-claude-cli` (main.rs, setup_wizard.rs, tests/output_format_contract.rs). Make sure your change adds no new ones; CI runs clippy without `-D warnings`.
  - Faster loops: `cargo test -p <crate>`, e.g. `cargo test -p rusty-claude-cli --bin claw` for REPL/editor tests.
- Python side: `python -m unittest discover -s tests`.
- `src/` and `tests/` are both present; update both surfaces together when Python behavior changes.

## Repository shape
- `rust/` contains the Rust workspace (12 crates) and the `claw` binary (crate `rusty-claude-cli`). See `ARCHITECTURE.md` for the crate map and request lifecycle.
- `src/` is the companion Python porting/parity workspace; `tests/` validates it.
- `docs/` holds topic guides and g0XX gate verification maps; `scripts/` holds fmt, install and roadmap helpers.
- Hierarchical `AGENTS.md` files (root, `rust/`, `src/`, each main crate) carry per-area code maps and conventions; read the nearest one before editing.

## Documentation map
- `MANUAL.md`: user manual for the interactive CLI (keys, permissions, slash commands, undo, settings, troubleshooting).
- `PROVIDERS.md`: provider setup and switching (routing rules, keys, Ollama vs cloud).
- `ARCHITECTURE.md`: crates, request lifecycle, interrupts, providers, permissions, state on disk.
- `USAGE.md`: task-oriented command and flag reference; `rust/README.md`: crate-level details.
- When you change user-visible behavior (keys, slash commands, settings, env vars, permission rules), update `MANUAL.md`, plus `ARCHITECTURE.md` if the design moved.

## Working agreement
- Prefer small, reviewable changes and keep generated bootstrap files aligned with actual repo workflows.
- Keep shared defaults in `.claw.json`; reserve `.claw/settings.local.json` for machine-local overrides.
- Do not overwrite existing `CLAUDE.md` content automatically; update it intentionally when repo workflows change.
