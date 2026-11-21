# Changelog

All notable changes to **PX Bots** are documented in this file following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-11-21

### Added
- **`px-ops`** crate (Phase 5 — Full 6-Bot Suite Complete!):
  - Full Rust rewrite of the host operations & breakglass Telegram console on `tg-core` (100% English, Telegram-only, Bot API 10.3+ rich UI, in-place editing, `/start` registered first, ~3 MB RSS).
  - Native Linux `/proc` + `/sys` dashboard with progress bars, heavy RAM-saver **Apps** 1-tap toggle (`systemd-user` / `systemd-system` / `docker`), paginated **Services** monitor & control with safe `systemd-run` self-restart, **Server** diagnostics & confirmed `apt` upgrade job, **9Router** health/lists/catalog sync with pure-Rust SHA-256 auth, **Agent Sessions** mode/compression/model control, **Backup** create/verify/restore with SHA-256 manifests & pre-restore checkpoints, confirmed **Reboot** & 2-stage **Shutdown**, background **Alert Monitor** with OPS Forum routing (`dm` / `ops` / `dual` + DM fallback), and `/exec` + `audit.jsonl`.
  - Added sanitized config templates in `crates/px-ops/examples/` and documentation in `docs/bots/px-ops.md`.
- **`tg-core`**: Added multi-surface `edit_in`, `present_in`, and `send_document_to` helpers to support Telegram forum group topics alongside owner DM.

## [0.4.0] - 2026-11-21

### Fixed
- **`tg-core` HTTP zero-hang hardening (fixes `px-mail` & `px-monitor` Cloudflare keep-alive stalls):**
  - Added explicit `.timeout_write(...)` and hard wall-clock `.timeout(...)` to `build_agent`, and introduced `build_api_agent(timeout_secs)` with `max_idle_connections(0)` so bursty API requests never reuse half-open sockets silently dropped by NAT or edge proxies during idle periods.
  - Split `TgClient` into dedicated `poll_agent` (continuous `getUpdates`) and `api_agent` (non-pooled interactive `send`/`edit`/`ack`/`sendDocument`) plus `Connection: close` on external API calls.
  - Switched `px-mail` (`MailApi`) and `px-monitor` (InferHub & target probes) to `build_api_agent`, logged `sync_d1` probe failures cleanly, and removed legacy env fallback keys from `px-mail`.

### Added
- **`px-abuse`** crate (Phase 4):
  - Full Rust rewrite of the Account-Factory (`af` CLI) Telegram operator console on `tg-core` (100% English, Telegram-only, Bot API 10.3+ rich UI, in-place editing, `/start` registered first).
  - Interactive multi-step Generate wizard (`Provider → Mode → Params → Preview → Confirmation Gate`), Operator Playbooks (`Quick Smoke`, `Fill Stock`, `Post-Generate`, `Cleanup Stock`), background job manager with 5-second live in-place progress updates & `SIGTERM` stop, honest `zero_pass` outcome surfacing, pure-Rust Pending CPA scanner with multi-select & batch import (max 5), pure-Rust router priority reorder (`Reprio`), archive/delete batch workflow, and `sendDocument` long-output delivery.
  - Added crate documentation in `docs/bots/px-abuse.md` and registered `px-abuse` in `.github/workflows/release.yml`.
- Added `TgClient::send_document` multipart file upload helper to `tg-core`.

## [0.3.0] - 2026-11-21

### Added
- **`px-mail`** crate (Phase 3):
  - Full Rust rewrite of the TempMail Telegram bot on `tg-core` (100% English, Telegram-only, Bot API 10.3+ rich UI, in-place editing).
  - Full Cloudflare Worker API parity (`/admin/new_address`, `/admin/inbox`, `/admin/mailboxes/probe`, `/admin/claim`, `/messages`, `/messages/:id`), FNV-1a 64-bit `mailbox_ref` parity for Worker push notifications (`inbox:msg:<ref>`), and seamless compatibility with existing `mailboxes.json`.
  - Features: 3-step `/create` (24h / Permanent, custom or random local-part, multi-domain), `/generate` bulk batch up to 10 with one-tap `Copy All`, `/emails` & `/inbox` with retention filters & pagination, automatic OTP extraction with `Copy OTP` pills, and clickable URL action pills inside message cards.
  - Added crate documentation in `docs/bots/px-mail.md` and registered `px-mail` in `.github/workflows/release.yml`.
- Registered `/start` as the first slash command in `setMyCommands` across **all** bots (`px-media`, `px-monitor`, `px-notes`, `px-mail`).

## [0.2.0] - 2026-11-21

### Added
- **`px-notes`** crate (Phase 2):
  - Full Rust rewrite of the local Markdown notes vault bot on top of `tg-core` (100% English, Telegram-only, Bot API 10.3+ rich UI, in-place editing).
  - Paginated vault browser (`/notes`), note reader (`/open`) with metadata grid & native `Copy Path` pill, in-place **Append Text** wizard, strict path-jailed **New Note** wizard (`/new`), full-text & path **Search** (`/search`) with UTF-8 safe snippets, and **Bookmarks** browser (`/links`) + **Add Link** wizard (`/link`) compatible with existing `links/<category>.md` files.
  - Added crate documentation in `docs/bots/px-notes.md` and registered `px-notes` in the musl GitHub Actions release pipeline.

## [0.1.1] - 2026-11-21

### Changed
- Standardized the entire project to **100% English**: all bot UI copy, button labels, slash command descriptions, alerts, progress strings, code comments, scripts, and documentation across `tg-core`, `px-media`, and `px-monitor`.
- Added the 100% English project rule to `AGENTS.md`.

## [0.1.0] - 2026-11-21

### Added
- Initial Rust Cargo Workspace monorepo (`px-bots`) targeting static `x86_64-unknown-linux-musl` binaries.
- Shared **`tg-core`** crate:
  - Blocking HTTP client via `ureq` + `rustls` (IPv4-first DNS resolver, zero-tokio).
  - `TgClient` with rich-first `sendRichMessage`, in-place `editMessageText`, automatic HTML fallback, and non-blocking `answerCallbackQuery` (1x RTT).
  - Bot API 10.3+ UI helpers: `field_grid`, canonical `table_markdown` (`|:---|:---|`), `hl` (`==...==`), `details_block`, native `<tg-button>` (`url`, `callback_data`, `copy_text`), `progress_bar`, and `stamp_wib`.
  - Atomic JSON state persistence (`0600`).
- **`px-media`**:
  - Full Rust rewrite of the media downloader with Bot API 10.3+ dashboard, live storage bar, smart Jellyfin status probe, active-folder green toggle button, `Recent` (`/recent`) browser, `Copy Path` / `Copy IP` pills, 1-line live progress replace with ETA, and rich completion/error reports.
- **`px-monitor`**:
  - Full Rust rewrite & modular expansion: live InferHub dashboard, in-place sub-menus (`Status`, `Usage`, `Today`, `Breakdown`, 2-step `Budget`, `Keys`), global HTTP endpoint probing via `MONITOR_TARGETS`, and non-blocking background alert + recovery checker.
- Full docs (`README.md`, `AGENTS.md`, `docs/`), `scripts/privacy-check.sh`, `scripts/deploy.sh`, and GitHub Actions release pipeline (`.github/workflows/release.yml`).
