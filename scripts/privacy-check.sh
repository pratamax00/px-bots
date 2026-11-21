#!/usr/bin/env bash
# Gate anti-bocor sebelum push ke repo public.
# Memindai seluruh file yang di-track git terhadap pola token, IP privat, dan path home user.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

# Kumpulkan file tracked (atau seluruh file kerja non-ignored bila belum ada commit)
if git rev-parse --verify HEAD >/dev/null 2>&1; then
  FILES=$(git ls-files)
else
  FILES=$(git ls-files --cached --others --exclude-standard)
fi

FAIL=0

check_pattern() {
  local label="$1"
  local regex="$2"
  local hits
  hits=$(echo "$FILES" | xargs -r grep -nE "$regex" -- 2>/dev/null | grep -v "^scripts/privacy-check.sh:" || true)
  if [ -n "$hits" ]; then
    echo "[BLOCKED] Terdeteksi pola $label:"
    echo "$hits"
    FAIL=1
  fi
}

check_pattern "Telegram Bot Token" '[0-9]{8,12}:AA[A-Za-z0-9_-]{30,}'
check_pattern "GitHub Token" '(ghp_|gho_|ghu_|github_pat_)[A-Za-z0-9_]{20,}'
check_pattern "Generic API Key" '(nvapi-|sk-[A-Za-z0-9]{20,}|ih_[A-Za-z0-9_-]{16,})'
check_pattern "Private LAN IP (192.168/10.x)" '192\.168\.[0-9]{1,3}\.[0-9]{1,3}'
check_pattern "Tailscale CGNAT IP (100.64-127)" '100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}'
check_pattern "Hardcoded /home/<user> path" '/home/[a-zA-Z0-9._-]+/'

if [ "$FAIL" -ne 0 ]; then
  echo ""
  echo "PRIVACY CHECK GAGAL! Bersihkan temuan di atas sebelum git push."
  exit 1
fi

echo "PRIVACY CHECK OK: 0 temuan sensitif di seluruh file repo."
