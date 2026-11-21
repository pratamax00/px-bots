# PX Bots

Monorepo Rust Cargo Workspace berisi armada bot operator Telegram pribadi (mode **Telegram-only**, static binary `x86_64-unknown-linux-musl`, ultra-ringan **~2–5 MB RSS** per bot) dengan standar antarmuka **Telegram Bot API 10.3+**.

## Arsitektur

- **Shared Core (`crates/tg-core`):** HTTP `ureq` + `rustls` (IPv4-first, zero-tokio), long-polling offset atomik (`0600`), non-blocking `answerCallbackQuery` (1x RTT), `sendRichMessage` & in-place `editMessageText` dengan fallback HTML otomatis, serta primitif UI Bot API 10.3+ (`==highlight==`, tabel GFM `|:---|:---|`, `<details>`, `<tg-button>` URL/callback/copy_text, progress bar visual, WIB stamp).
- **Isolasi Runtime:** Monorepo hanya menyatukan kode & pipeline build CI. Di server, setiap bot berjalan sebagai binary mandiri, proses mandiri, unit `systemd --user` mandiri, state mandiri, dan `runtime.env` mandiri di luar repo.
- **Zero Toolchain di Server:** Seluruh compile & unit test berjalan di GitHub Actions (`.github/workflows/release.yml`) dan dirilis sebagai static binary lengkap dengan checksum `.sha256`.

## Daftar Bot (`px-*`)

| Binary | Status | Fungsi | Dokumentasi |
| :--- | :--- | :--- | :--- |
| **`px-media`** | ✅ Aktif (`v0.1.0`) | Unduh file Google Drive (`gdrive-dl`) & Pixeldrain ke library media + scan Jellyfin | [docs/bots/px-media.md](docs/bots/px-media.md) |
| **`px-monitor`** | ✅ Aktif (`v0.1.0`) | Monitor balance/usage/budget InferHub + endpoint HTTP global modular | [docs/bots/px-monitor.md](docs/bots/px-monitor.md) |
| **`px-notes`** | 🔜 Fase 2 | Manajemen catatan lokal, draft, dan pencarian cepat | — |
| **`px-mail`** | 🔜 Fase 3 | TempMail Telegram + sync Cloudflare D1 | — |
| **`px-abuse`** | 🔜 Fase 4 | Operator account-factory, playbooks, dan jobs | — |
| **`px-ops`** | 🔜 Fase 5 | Breakglass kontrol server, service, apps toggle, dan alerts | — |

## Quick Start & Deploy

1. Siapkan file `runtime.env` (mode `600`, **di luar direktori repo**) berdasarkan `.env.example` di masing-masing crate.
2. Tarik static binary dari GitHub Releases dan verifikasi checksum:

```bash
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env
./scripts/deploy.sh px-monitor latest /path/to/px-monitor/runtime.env
```

3. Cek dokumentasi lengkap:
   - [Arsitektur & Isolasi](docs/architecture.md)
   - [Runbook Deploy & Rollback](docs/deploy-runbook.md)
   - [Changelog](CHANGELOG.md)

## Keamanan & Privasi

- 100% konfigurasi (token bot, `OWNER_ID`, API key, domain, IP, path) wajib berasal dari environment variable eksternal.
- Jalankan `./scripts/privacy-check.sh` sebelum setiap `git push`.
