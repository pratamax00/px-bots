# PX Bots

A Rust Cargo Workspace monorepo of ultra-lightweight, owner-only Telegram operator bots (**Telegram-only**, static `x86_64-unknown-linux-musl` binaries, **~2–5 MB RSS** per bot) built for **Telegram Bot API 10.3+**.

## Architecture

- **Shared Core (`crates/tg-core`):** Blocking HTTP via `ureq` + `rustls` (IPv4-first DNS resolver, zero-tokio), atomic offset persistence (`0600`), non-blocking `answerCallbackQuery` (1x RTT), rich-first `sendRichMessage` & in-place `editMessageText` with automatic HTML fallback, and Bot API 10.3+ UI primitives (`==highlight==`, canonical `|:---|:---|` GFM tables, `<details>`, native `<tg-button>` URL/callback/copy pills, progress bars, WIB timestamps).
- **Isolated Runtime:** The monorepo only unifies source code, `tg-core`, and CI builds. On the host server, each bot runs as an independent binary, process, `systemd --user` service, state file, and external `runtime.env`.
- **Zero Toolchain on Server:** All tests and static musl builds run in GitHub Actions (`.github/workflows/release.yml`) and publish verified binaries with `.sha256` checksums to GitHub Releases.

## Bot Suite (`px-*`)

| Binary | Status | Purpose | Docs |
| :--- | :--- | :--- | :--- |
| **`px-media`** | ✅ Active (`v1.0.0`) | Google Drive (`gdrive-dl`) & Pixeldrain media library downloader + Jellyfin scan | [docs/bots/px-media.md](docs/bots/px-media.md) |
| **`px-monitor`** | ✅ Active (`v1.0.0`) | InferHub balance/usage/budget monitor + modular global HTTP target probes | [docs/bots/px-monitor.md](docs/bots/px-monitor.md) |
| **`px-notes`** | ✅ Active (`v1.0.0`) | Local Markdown notes vault, bookmark browser, append editor, and full-text search | [docs/bots/px-notes.md](docs/bots/px-notes.md) |
| **`px-mail`** | ✅ Active (`v1.0.0`) | Ephemeral & permanent TempMail operator with Cloudflare D1 sync, OTP & link pills | [docs/bots/px-mail.md](docs/bots/px-mail.md) |
| **`px-abuse`** | ✅ Active (`v1.0.0`) | Account-factory operator, playbooks, pending CPA import, reprio, jobs, and audit log | [docs/bots/px-abuse.md](docs/bots/px-abuse.md) |
| **`px-ops`** | ✅ Active (`v1.0.0`) | Host operations & breakglass console, apps toggle, services, backup, 9Router, alerts | [docs/bots/px-ops.md](docs/bots/px-ops.md) |

## Quick Deploy

1. Create an external `runtime.env` (`chmod 600`, **outside this repository**) from each crate's `.env.example`.
2. Pull the static binary from GitHub Releases and verify its checksum:

```bash
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env --restart
./scripts/deploy.sh px-monitor latest /path/to/px-monitor/runtime.env --restart
```

3. Full documentation:
   - [Architecture & Runtime Isolation](docs/architecture.md)
   - [Deploy & Rollback Runbook](docs/deploy-runbook.md)
   - [Changelog](CHANGELOG.md)

## Security & Privacy

- 100% of configuration (bot tokens, `OWNER_ID`, API keys, private domains/IPs, host paths) must be provided via external environment variables.
- Run `./scripts/privacy-check.sh` before every `git push`.
