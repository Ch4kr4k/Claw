#!/usr/bin/env bash
# install-claw.sh — Build claw, install the binary behind the launcher, and
# install the launchers: the usual way to update claw on this machine.
#
#   1. scripts/install-local.sh with PREFIX_BIN=~/.local/lib/claw, so the
#      binary lands where the claw launcher runs it from, never over the
#      launcher in ~/.local/bin;
#   2. scripts/install-wrappers.sh for ~/.local/bin/claw, llama-gpu, the
#      llama-gpu systemd unit and its model list
#      (~/.config/llama-gpu/models.ini, from scripts/wrappers/llama-models.ini);
#   3. the desktop app: claw-desktop next to the claw binary, `claw-desktop` on
#      PATH, and an app-launcher entry with an icon (start it with
#      `claw desktop`, `claw-desktop` or from rofi/dmenu).
#
# Usage:
#   scripts/install-claw.sh              # release build + wrappers
#   scripts/install-claw.sh --debug      # debug build
#   scripts/install-claw.sh --no-llama   # skip llama-gpu, its unit and model list
#   scripts/install-claw.sh --no-desktop # skip the desktop app
#
# Other arguments go to install-local.sh (e.g. --all).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
local_args=()
wrapper_args=()
with_desktop=1
for arg in "$@"; do
    case "$arg" in
        --no-llama) wrapper_args+=("$arg") ;;
        --no-desktop) with_desktop=0 ;;
        --help|-h) sed -n '2,20p' "$0" | sed 's/^# //; s/^#//'; exit 0 ;;
        *) local_args+=("$arg") ;;
    esac
done

[[ "$with_desktop" == 1 ]] && local_args+=(--desktop)
PREFIX_BIN="$HOME/.local/lib/claw" "$REPO_ROOT/scripts/install-local.sh" "${local_args[@]}"
# PREFIX_BIN above was only for the binary; the launchers use their default.
env -u PREFIX_BIN "$REPO_ROOT/scripts/install-wrappers.sh" "${wrapper_args[@]}"

if [[ "$with_desktop" == 1 ]]; then
    # dmenu_run lists PATH; rofi and app menus read the .desktop entry.
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications" \
        "$HOME/.local/share/icons/hicolor/scalable/apps"
    ln -sf "$HOME/.local/lib/claw/claw-desktop" "$HOME/.local/bin/claw-desktop"
    install -m 0644 "$REPO_ROOT/scripts/wrappers/claw.svg" \
        "$HOME/.local/share/icons/hicolor/scalable/apps/claw.svg"
    sed "s|@BIN@|$HOME/.local/lib/claw/claw-desktop|" \
        "$REPO_ROOT/scripts/wrappers/claw-desktop.desktop" \
        >"$HOME/.local/share/applications/claw-desktop.desktop"
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
    echo "✓ Installed the claw desktop app (claw desktop, claw-desktop, or the app launcher)" >&2
fi
