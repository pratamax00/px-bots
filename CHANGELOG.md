# Changelog

All notable changes to **PX Bots** are documented in this file following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and [Semantic Versioning](https://semver.org/).

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
