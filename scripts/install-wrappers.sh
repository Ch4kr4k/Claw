#!/usr/bin/env bash
# install-wrappers.sh — Install the launcher scripts in scripts/wrappers/:
#   claw               ~/.local/bin/claw   (runs ~/.local/lib/claw/claw)
#   llama-gpu          ~/.local/bin/llama-gpu
#   llama-gpu-switch   ~/.local/bin/llama-gpu-switch  (fork <-> stock llama.cpp)
#   llama-models.ini   ~/.config/llama-gpu/models.ini  (`~/` becomes $HOME)
#   llama-gpu.service  ~/.config/systemd/user/llama-gpu.service
#
# Usage:
#   scripts/install-wrappers.sh             # install all five
#   scripts/install-wrappers.sh --no-llama  # only the claw launcher
#
# Environment:
#   PREFIX_BIN   directory for the launchers (default: ~/.local/bin)
#
# A file that differs from the repo copy is backed up to <file>.bak.<date>
# first. The llama-gpu service is not restarted: a running server keeps its
# settings until `systemctl --user restart llama-gpu`.
set -euo pipefail

usage() {
    sed -n '2,18p' "$0" | sed 's/^# //; s/^#//'
}

WITH_LLAMA=1
for arg in "$@"; do
    case "$arg" in
        --no-llama) WITH_LLAMA=0 ;;
        --help|-h) usage; exit 0 ;;
        *)
            echo "error: unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WRAPPERS="$REPO_ROOT/scripts/wrappers"
DEST="${PREFIX_BIN:-$HOME/.local/bin}"
UNIT_DIR="$HOME/.config/systemd/user"
STAMP="$(date +%Y-%m-%d-%H%M%S)"

# install_file <source> <target> <mode>
install_file() {
    local src="$1" target="$2" mode="$3"
    if [[ -e "$target" ]] && cmp -s "$src" "$target"; then
        echo "= $target is up to date" >&2
        return
    fi
    mkdir -p "$(dirname "$target")"
    if [[ -e "$target" ]]; then
        cp -p "$target" "$target.bak.$STAMP"
        echo "  backed up the old $target to $target.bak.$STAMP" >&2
    fi
    install -m "$mode" "$src" "$target"
    echo "✓ Installed $target" >&2
}

install_file "$WRAPPERS/claw" "$DEST/claw" 0755
if [[ ! -x "$HOME/.local/lib/claw/claw" ]]; then
    echo "note: ~/.local/lib/claw/claw is missing; build it with scripts/install-claw.sh" >&2
fi

if [[ "$WITH_LLAMA" == 1 ]]; then
    install_file "$WRAPPERS/llama-gpu" "$DEST/llama-gpu" 0755
    install_file "$WRAPPERS/llama-gpu-switch" "$DEST/llama-gpu-switch" 0755
    install_file "$WRAPPERS/llama-gpu.service" "$UNIT_DIR/llama-gpu.service" 0644
    # llama-server does not expand `~` in preset paths.
    models_ini="$(mktemp)"
    trap 'rm -f "$models_ini"' EXIT
    sed "s|= ~/|= $HOME/|" "$WRAPPERS/llama-models.ini" >"$models_ini"
    install_file "$models_ini" "$HOME/.config/llama-gpu/models.ini" 0644
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
        echo "  start/restart the server with: systemctl --user restart llama-gpu" >&2
    fi
fi

case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "note: $DEST is not on your PATH" >&2 ;;
esac
