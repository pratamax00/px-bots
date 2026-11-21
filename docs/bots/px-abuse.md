# PX-Abuse

Native Rust Telegram operator console for **Account-Factory (`af` CLI)** workflows, usable pool auditing, priority reordering, pending CPA import, proxy pool management, and monitored background generation jobs — built on `tg-core` for **Telegram Bot API 10.3+** (**Telegram-only**, ~3 MB RSS).

## Features

- **Generate Wizard (In-Place Edit):** Multi-step `Provider → Mode (Smoke / Batch / Mass Farm / Dry-Run) → Parameters → Preview → Confirmation Gate` with numeric chip selectors and custom text input.
- **Monitored Background Jobs:** Launches `af registrar smoke` jobs without blocking the polling loop, captures combined logs (mode `0600`, capped at 1 MB), updates the Telegram status card in-place every 5 seconds with parsed `pass / fail / target` progress, supports `/stop <id>` (`SIGTERM`), and flags honest `zero_pass` outcomes (`exit 0 ≠ farm pass`).
- **Operator Playbooks:**
  - `Quick Smoke` — 1-round verification using default smoke target.
  - `Fill Stock` — Standard batch refill (auto-import OFF).
  - `Post-Generate` — Pure-Rust scan of pending CPA files (`pending_cpa/xai-*.json`), pool health check, `○ / ✓` multi-select, `▶ 1-Tap` import, and batch import (up to 5 accounts with live progress).
  - `Cleanup Stock` — Archive preview + priority reorder plan + proxy status.
- **Pool, Usable & Reprio:** Classifies connections into `fresh`, `transient`, `quota`, `denied`, `other`, `unknown`, renders visual health bars, plans priority reordering (`fresh/transient` first, `quota/denied` last), backs up prior state, and applies updates directly to the router HTTP API in pure Rust.
- **Safe Archiving & Deletion:** Archives inactive accounts (`quota`, `denied`, `auth_dead`) locally first; router deletion (`/delete batch <id>`) requires explicit confirmation and validates manifest integrity.
- **Long-Output Delivery:** Any output exceeding 3,900 characters is automatically attached via `sendDocument` with a concise summary card.
- **Audit Log:** Appends structured JSONL audit records to `<JOBS_DIR>/audit.jsonl` (mode `0600`).

## Slash Commands

| Command | Description |
| :--- | :--- |
| `/start` | Open PX-Abuse home dashboard |
| `/status` | Usable pool classification & proxy overview |
| `/usable [full]` | Run `af usable scan` & report |
| `/pool` | Inspect active provider pool & priorities |
| `/proxy [sync]` | Proxy winners pool status or trigger sync |
| `/playbooks` | Open 1-tap operator playbooks |
| `/import <email>` | Import pending CPA via `--via-pilot` |
| `/probe [id\|email]` | Run pool auth test + usable class honesty check |
| `/archive [apply\|quick [apply]]` | Preview or archive quota/denied accounts |
| `/delete batch <id>` | Delete archived batch connections from router (confirmed) |
| `/smoke [dry\|run]` | Launch smoke dry-run or confirmed live job |
| `/ops <subcommand>` | `daily` · `cleanup` · `postsmoke` · `reprio [apply]` |
| `/jobs`, `/job <id>`, `/stop <id>` | Inspect or terminate background jobs |
| `/settings` | Adjust default provider, workers, targets, notify mode |
| `/help` | Show command reference |

## Configuration

Copy [`crates/px-abuse/.env.example`](../../crates/px-abuse/.env.example) to a private vault (`chmod 600`, outside the repo):

| Variable | Required | Description |
| :--- | :--- | :--- |
| `TG_TOKEN` | Yes | Telegram Bot API token |
| `OWNER_ID` | Yes | Numeric Telegram owner chat ID |
| `AF_BIN` | Yes | Absolute path to `af` CLI executable |
| `AF_ROOT` | Yes | Working directory for `af` CLI |
| `PENDING_CPA_ROOT` | No | Root directory containing `account_lists/farm/*/pending_cpa/xai-*.json` |
| `ROUTER_ENV_FILE` | No | Router `.env` containing `BASE_URL` and `INITIAL_PASSWORD` for reprio apply |
| `ROUTER_BACKUP_DIR` | No | Directory for pre-reprio JSON snapshots |
| `GEN_DOMAINS` | No | Comma-separated custom domains for domain-aware providers |
| `STATE_FILE` | No | Persistent JSON state path (default `./state/px-abuse.json`) |
| `JOBS_DIR` | No | Directory for job index, logs, and `audit.jsonl` (default `./state/px-abuse-jobs`) |

## Verify & Deploy

```bash
./scripts/deploy.sh px-abuse latest /path/to/px-abuse/runtime.env --restart
```
