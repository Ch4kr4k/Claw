# Claw Code release notes

## Unreleased

- Added editable slash-command settings, permission rules, automatic memory controls, and sandbox toggles.
- Added MCP server enable/disable and connection refresh commands.
- Added local artifact discovery and reference prompts for designs, slides, diagrams, plugin authoring, and Anthropic API work.
- Added Rust memory diagnostics, editor launch, and VS Code/Cursor terminal setup commands.
- Added Claude Code style diffs: edits show `Added N lines, removed M lines` and numbered, syntax-coloured lines on full-width red/green rows; write and edit tools now report real hunks with context (Myers diff) instead of the whole file.
- Added the tools Monitor, EnterWorktree/ExitWorktree, ScheduleWakeup (self-paced `/loop`), ListAgents, SendMessage (resumes this session's sub-agents), PushNotification, ReportFindings, SendFeedback (local drafts), SendUserFile, EndConversation and WaitForMcpServers.
- The LSP tool now runs real language servers (rust-analyzer, pyright, typescript-language-server, gopls, clangd, …; `lspServers` in settings for others) instead of returning a placeholder.
- CronCreate tasks now run: in-session cron schedules (local time) fire as turns when the prompt is idle; TaskOutput/TaskStop also reach background shells and monitors.
- Added `/fast`, `/login`, `/logout`, `/auto-mode-setup`, `/schedule` (crontab routines running `claw -p`), `/ultrareview` and `/autofix-pr`; `/ultraplan` now runs a planning turn and writes `.claw/plans/<task>.md`.
- Clicking the "Jump to bottom" hint now jumps to the end, like Ctrl+End.
- Added the diff panel (full-screen, at least 110 columns): uncommitted changes right of the conversation, with its own scrolling and ✕; `/diffpanel [on|off]`, `CLAW_DIFF_PANEL=0`. Narrower windows show a dismissible "Resize your terminal" notice.
- Fixed context-window recovery: it now continues the failed request instead of sending the prompt again (which redid the same work and overflowed every round), cuts oversized tool results (scaled to the model's window; `CLAW_OVERFLOW_TOOL_RESULT_CHARS` / `overflowToolResultChars` override), skips rounds that change nothing, and reads llama.cpp's window size correctly. Its cut uses `CLAW_CONTEXT_WINDOW` for `local/` models.
- Fixed Esc stopping working for the rest of a turn after a signal (Ctrl+C, a window resize) interrupted the key watcher's read.

This file records Claw changes. It is not the Anthropic Claude Code changelog.
