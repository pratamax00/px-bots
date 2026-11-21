#!/usr/bin/env bash
# Tarik static binary dari GitHub Releases, verifikasi SHA256, uji --check, dan opsional restart service.
# Pemakaian: ./scripts/deploy.sh <bot> [tag=latest] [env_file] [--restart]
set -euo pipefail

BOT="${1:-}"
TAG="${2:-latest}"
ENV_FILE="${3:-}"
RESTART="${4:-}"

if [ -z "$BOT" ]; then
  echo "Pemakaian: $0 <px-media|px-monitor|...> [tag=latest] [env_file] [--restart]"
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${PX_BOTS_REPO:-}"
if [ -z "$REPO" ]; then
  REMOTE_URL="$(git -C "$ROOT" remote get-url origin 2>/dev/null || true)"
  REPO="$(echo "$REMOTE_URL" | sed -E 's#.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$#\1#')"
fi
if [ -z "$REPO" ]; then
  echo "Gagal mendeteksi REPO GitHub. Set env PX_BOTS_REPO=owner/repo."
  exit 1
fi

BIN_DIR="$ROOT/bin"
mkdir -p "$BIN_DIR"
ASSET="${BOT}-linux-x86_64"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TMP_BIN="$BIN_DIR/${BOT}.new"
TMP_SHA="$BIN_DIR/${BOT}.sha256"

echo "Mengunduh ${ASSET} (${TAG}) dari ${REPO}..."
curl -fsSL "${BASE_URL}/${ASSET}" -o "$TMP_BIN"
curl -fsSL "${BASE_URL}/${ASSET}.sha256" -o "$TMP_SHA"
chmod 755 "$TMP_BIN"

EXPECTED="$(awk '{print $1}' "$TMP_SHA")"
ACTUAL="$(sha256sum "$TMP_BIN" | awk '{print $1}')"
if [ "$EXPECTED" != "$ACTUAL" ]; then
  rm -f "$TMP_BIN"
  echo "ERROR: SHA256 tidak cocok! expected=${EXPECTED} actual=${ACTUAL}"
  exit 1
fi
echo "SHA256 OK: ${ACTUAL:0:16}..."

"$TMP_BIN" --version
if [ -n "$ENV_FILE" ] && [ -f "$ENV_FILE" ]; then
  bash -c 'set -a && source "$1" && set +a && "$2" --check' _ "$ENV_FILE" "$TMP_BIN"
fi

if [ -f "$BIN_DIR/$BOT" ]; then
  cp "$BIN_DIR/$BOT" "$BIN_DIR/${BOT}.prev"
fi
mv "$TMP_BIN" "$BIN_DIR/$BOT"
echo "Terpasang: $BIN_DIR/$BOT"

if [ "$RESTART" = "--restart" ]; then
  systemctl --user restart "${BOT}.service"
  systemctl --user is-active "${BOT}.service"
fi
