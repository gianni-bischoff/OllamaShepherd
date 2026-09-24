#!/usr/bin/env bash
# Ollama Shepherd — Linux installer
#
#   curl -fsSL https://raw.githubusercontent.com/gianni-bischoff/OllamaShepherd/master/install.sh | bash
#
# Optional environment overrides:
#   INSTALL_DIR=/usr/local/bin   where to put the binary   (default: ~/.local/bin)
#   VERSION=v0.2.2               pin a specific release    (default: latest)
set -euo pipefail

REPO="gianni-bischoff/OllamaShepherd"
ASSET="ollama-shepherd-x86_64-unknown-linux-gnu.tar.gz"
VERSION="${VERSION:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

log()  { printf '\033[1;36m▸\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

for tool in curl tar sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || die "missing dependency: $tool"
done

[ "$(uname -m)" = "x86_64" ] || die "unsupported architecture: $(uname -m) (only x86_64 builds are published)"

BASE="https://github.com/$REPO/releases"
if [ "$VERSION" = "latest" ]; then
  URL="$BASE/latest/download/$ASSET"
else
  URL="$BASE/download/$VERSION/$ASSET"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

log "downloading $(basename "$ASSET") ${VERSION}…"
curl -fsSL "$URL" -o "$TMP/$ASSET" || die "download failed — is there a release asset for linux x86_64?"

log "verifying checksum…"
curl -fsSL "$URL.sha256" -o "$TMP/checksums" || die "checksum file download failed"
(cd "$TMP" && sha256sum -c checksums --status) || die "checksum mismatch — aborting"

log "installing to $INSTALL_DIR…"
tar xzf "$TMP/$ASSET" -C "$TMP"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$TMP/ollama-shepherd" "$INSTALL_DIR/ollama-shepherd"

ok "installed ollama-shepherd $("$INSTALL_DIR/ollama-shepherd" --version | awk '{print $NF}')"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) log "note: $INSTALL_DIR is not in your PATH."
     echo "       add:  export PATH=\"$INSTALL_DIR:\$PATH\"  (e.g. in ~/.bashrc)" ;;
esac

echo
ok "run it:  ollama-shepherd"
echo "   update later with:  ollama-shepherd update"