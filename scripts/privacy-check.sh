#!/usr/bin/env bash
# Pre-push anti-leak scanner for the public repository.
# Scans all tracked files for bot tokens, API keys, private IPs, and hardcoded home paths.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

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
    echo "[BLOCKED] Detected $label:"
    echo "$hits"
    FAIL=1
  fi
}

check_pattern "Telegram Bot Token" '[0-9]{8,12}:AA[A-Za-z0-9_-]{30,}'
check_pattern "GitHub Token" '(ghp_|gho_|ghu_|github_pat_)[A-Za-z0-9_]{20,}'
check_pattern "Generic API Key" '(nvapi-|sk-[A-Za-z0-9]{20,}|ih_[A-Za-z0-9_-]{16,})'
check_pattern "Private LAN IP (192.168.x.x)" '192\.168\.[0-9]{1,3}\.[0-9]{1,3}'
check_pattern "Tailscale CGNAT IP (100.64-127)" '100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}'
check_pattern "Hardcoded /home/<user> path" '/home/[a-zA-Z0-9._-]+/'

if [ "$FAIL" -ne 0 ]; then
  echo ""
  echo "PRIVACY CHECK FAILED! Clean up all findings before pushing."
  exit 1
fi

echo "PRIVACY CHECK OK: 0 sensitive findings across all tracked files."
