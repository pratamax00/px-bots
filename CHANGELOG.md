# Changelog

Semua perubahan penting pada monorepo **PX Bots** dicatat di sini mengikuti format [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) dan [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-21

### Added
- Fondasi monorepo Rust Cargo Workspace (`px-bots`) dengan target static binary `x86_64-unknown-linux-musl`.
- Shared core crate **`tg-core`**:
  - HTTP client `ureq` + `rustls` (IPv4-first resolver, zero-tokio).
  - `TgClient` dengan `sendRichMessage`, in-place `editMessageText`, fallback HTML otomatis, dan non-blocking `answerCallbackQuery` (1x RTT).
  - Primitif UI Bot API 10.3+: `field_grid`, `table_markdown` (`|:---|:---|`), `hl` (`==...==`), `details_block`, native `<tg-button>` (`url`, `callback_data`, `copy_text`), `progress_bar`, dan `stamp_wib`.
  - Persistensi state JSON atomik ber-permission `0600`.
- **`px-media`**:
  - Rebrand penuh dari downloader media lama ke Rust + `tg-core`.
  - Dashboard Bot API 10.3+ dengan visual storage bar, smart status Jellyfin real-time, tombol folder aktif hijau edit di tempat, menu `🕒 Terbaru` (`/recent`), tombol `📋 Salin Path` / `📋 Salin IP`.
  - Unduhan Google Drive (`gdrive-dl`) & Pixeldrain (`/u/<id>`) dengan progres 1 baris in-place replace + ETA serta laporan akhir full rich.
- **`px-monitor`**:
  - Rebrand penuh & perluasan global dari monitor InferHub lama ke Rust + `tg-core`.
  - Dashboard live + sub-menu in-place edit (`Status`, `Usage`, `Hari Ini`, `Breakdown`, `Budget` 2-step confirm, `Keys`).
  - Modul global `🌐 Targets` (`/targets`) lewat env `MONITOR_TARGETS` untuk memantau endpoint HTTP/API tambahan beserta latency.
  - Background periodic checker non-blocking dengan alert rich + notifikasi recovery otomatis.
- Dokumentasi awal (`README.md`, `AGENTS.md`, `docs/architecture.md`, `docs/deploy-runbook.md`, `docs/bots/`), skrip `scripts/privacy-check.sh` & `scripts/deploy.sh`, dan workflow GitHub Actions `.github/workflows/release.yml`.
