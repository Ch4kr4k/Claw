#!/usr/bin/env bash
# install-searxng.sh — Run SearXNG (a self-hosted meta search engine) in
# rootless podman for claw's WebSearch, on 127.0.0.1:8888, started at login.
#   scripts/searxng/settings.yml     -> ~/.config/searxng/settings.yml
#   scripts/searxng/searxng.container -> ~/.config/containers/systemd/
#
# claw uses it when it answers (CLAW_SEARXNG_URL, default
# http://127.0.0.1:8888; set CLAW_SEARXNG_URL=off to skip it).
# An existing settings.yml is kept (it holds the secret key).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/scripts/searxng"
CONF="$HOME/.config/searxng"
UNITS="$HOME/.config/containers/systemd"

command -v podman >/dev/null || { echo "error: podman is not installed" >&2; exit 1; }
mkdir -p "$CONF" "$UNITS"
if [[ ! -f "$CONF/settings.yml" ]]; then
    secret="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    sed "s/@SECRET_KEY@/$secret/" "$SRC/settings.yml" > "$CONF/settings.yml"
    chmod 0644 "$CONF/settings.yml"
    echo "✓ Wrote $CONF/settings.yml" >&2
else
    echo "note: keeping existing $CONF/settings.yml" >&2
fi
install -m 0644 "$SRC/searxng.container" "$UNITS/searxng.container"
echo "✓ Installed $UNITS/searxng.container" >&2

podman pull docker.io/searxng/searxng:latest
systemctl --user daemon-reload
systemctl --user restart searxng.service
for _ in $(seq 1 30); do
    if curl -fsS -m 2 "http://127.0.0.1:8888/search?q=test&format=json" >/dev/null 2>&1; then
        echo "✓ SearXNG answers on http://127.0.0.1:8888" >&2
        exit 0
    fi
    sleep 1
done
echo "warning: SearXNG did not answer yet; see: systemctl --user status searxng" >&2
