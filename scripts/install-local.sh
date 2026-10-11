#!/usr/bin/env bash
# install-local.sh — Build claw in release mode and install it to ~/.local/bin.
#
# Usage:
#   scripts/install-local.sh            # build + install `claw`
#   scripts/install-local.sh --all      # also install claw-analog
#   scripts/install-local.sh --debug    # use a debug build instead of release
#
# Environment:
#   PREFIX_BIN   install directory (default: ~/.local/bin)
#
set -euo pipefail

usage() {
    sed -n '2,10p' "$0" | sed 's/^# //; s/^#//'
}

PROFILE="release"
BINS=(claw)

for arg in "$@"; do
    case "$arg" in
        --all) BINS=(claw claw-analog) ;;
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
    install -m 0755 "$src" "$DEST/$bin"
    echo "✓ Installed $DEST/$bin" >&2
done

case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "note: $DEST is not on your PATH" >&2 ;;
esac
