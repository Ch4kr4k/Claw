# TODO

## Claw desktop app

**Built (2026-10-17):** `crates/claw-desktop` + `claw desktop-agent`, with a
custom JSON-lines protocol instead of ACP (see MANUAL "Desktop app" and
ARCHITECTURE "Desktop app"). Since then: Solarized theme, chat/terminal tabs,
multi-delete, incognito, collapsible sidebar, highlighted code, images,
Settings (skills, MCP, tools, scheduled tasks, notifications). Still open:
ACP for Zed, image attachments sent to vision models (attach only adds paths
now), diffs for edits (reuse `diff_view`), scheduled tasks while the app is
closed (a systemd timer running `claw -p`), a WebSocket gateway.

Original plan:

A native desktop window for claw, launched with `claw desktop` and from an
app-launcher entry (`.desktop` file + icon in `~/.local/share/applications`;
the session runs dwm, so it shows up in rofi/dmenu-style launchers).

Design (agreed 2026-10-16):

- **Backend: claw as an ACP agent.** Implement the stubbed `claw acp`
  (Agent Client Protocol, JSON-RPC over stdio): sessions, streamed text,
  tool-call updates, permission requests (from claw's approval prompts),
  cancel (claw's interrupt), model list and switching. The app runs
  `claw acp` as a child process, so the REPL, settings, sandbox,
  permissions, sessions and `/model` routing stay shared. Zed can host it
  too, which is the first test before any GUI exists.
- **Front end: Iced** (pure Rust, native window, wgpu), as a new crate in the
  workspace. Rejected: Tauri/Dioxus (WebKitGTK compile, 200-400 MB per
  window), egui (weak text selection for long chats), GPUI (unstable API),
  Slint (form-oriented).
- **Layout:** session sidebar; chat with streamed markdown, collapsible
  tool-call rows and diffs (reuse `diff_view` / `line_diff`); inline
  permission cards (Allow once / Always / Deny); prompt editor; status bar
  with model picker (local + API), context use, tok/s, llama-gpu health
  (`llama-gpu-switch status`) and Stop.
- **Machine access:** same as the terminal claw; an access-level picker
  (sandboxed / ask for destructive / full access) maps to the existing
  settings.
- **Later:** if several clients must share one live session or remote access
  is wanted, move from ACP over stdio to a small WebSocket gateway (as Hermes
  Desktop does). Screen control (xdotool + screenshots) would need a vision
  model.

Order: ACP server, then the Iced app (chat, input, approvals, status bar),
then polish (diffs, session resume, `claw desktop` command, launcher entry).

## Harnyx-style harness optimization (later)

Added 2026-10-19, deferred on purpose. An optimization loop around claw's
harness rather than the model: mine batches of failed tasks, have an
"engineer" model propose an executable patch to one of four lifecycle hooks
(initialize context, hint before a decision, validate/rewrite/veto an
action, recover after feedback), run the patch in a sandbox, rerun the same
tasks, and keep it only if task success measurably improves.

Prerequisites before starting:

- A repeatable task suite with automatic pass/fail checks (session logs in
  `.claw/sessions/` are the raw material; LeanGate's type-check results and
  `RepoSignals` failures can be graders).
- Explicit hook points in `runtime/src/conversation.rs` for the four
  lifecycle stages (today: system prompt, hooks, permission prompter, the
  LeanGate completion gate).
- A sandboxed runner (bwrap, or a worktree per candidate) so patched
  harnesses cannot touch the real workspace.
