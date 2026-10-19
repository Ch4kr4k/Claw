#!/usr/bin/env bash
# install-local.sh — Build claw in release mode and install it to ~/.local/bin.
#
# Usage:
#   scripts/install-local.sh            # build + install `claw`
#   scripts/install-local.sh --all      # also install claw-analog
#   scripts/install-local.sh --desktop  # also install the claw-desktop app
#   scripts/install-local.sh --debug    # use a debug build instead of release
#
# Environment:
#   PREFIX_BIN   install directory (default: ~/.local/bin)
#
set -euo pipefail

usage() {
    sed -n '2,11p' "$0" | sed 's/^# //; s/^#//'
}

PROFILE="release"
BINS=(claw)

for arg in "$@"; do
    case "$arg" in
        --all) BINS+=(claw-analog) ;;
        --desktop) BINS+=(claw-desktop) ;;
        --debug) PROFILE="debug" ;;
        --help|-h) usage; exit 0 ;;
        *)
            echo "error: unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUST_DIR="$REPO_ROOT/rust"
DEST="${PREFIX_BIN:-$HOME/.local/bin}"

cargo_args=(build --manifest-path "$RUST_DIR/Cargo.toml" --locked)
[[ "$PROFILE" == "release" ]] && cargo_args+=(--release)
for bin in "${BINS[@]}"; do
    cargo_args+=(--bin "$bin")
done

# Inject GIT_SHA so `claw version` reports provenance.
GIT_SHA="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || true)"
export GIT_SHA

echo "▶ Building ${BINS[*]} ($PROFILE) from $REPO_ROOT" >&2
cargo "${cargo_args[@]}"

mkdir -p "$DEST"
for bin in "${BINS[@]}"; do
    src="$RUST_DIR/target/$PROFILE/$bin"
    dest="$DEST"
    # A launcher script (e.g. ~/.local/bin/claw exec'ing ~/.local/lib/claw/claw)
    # must not be replaced by the binary: install next to what it runs instead.
    if [[ -z "${PREFIX_BIN:-}" && -f "$dest/$bin" ]] \
        && [[ "$(head -c 2 "$dest/$bin")" == "#!" ]]; then
        dest="$HOME/.local/lib/$bin"
        mkdir -p "$dest"
        echo "note: $DEST/$bin is a launcher script; installing the binary to $dest" >&2
    fi
    # `claw desktop` and the app menu entry run the claw-desktop beside the
    # real claw binary, so it goes there when claw sits behind a launcher.
    if [[ "$bin" == "claw-desktop" && -z "${PREFIX_BIN:-}" && -f "$DEST/claw" ]] \
        && [[ "$(head -c 2 "$DEST/claw")" == "#!" ]]; then
        dest="$HOME/.local/lib/claw"
        mkdir -p "$dest"
    fi
    install -m 0755 "$src" "$dest/$bin"
    echo "✓ Installed $dest/$bin" >&2
done

# The binary behind the launcher (~/.local/lib/claw) is not meant to be on PATH.
case ":$PATH:" in
    *":$DEST:"*) ;;
    *) [[ "$DEST" == "$HOME/.local/lib/claw" ]] || echo "note: $DEST is not on your PATH" >&2 ;;
esac
