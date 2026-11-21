# PX-Monitor

Bot Telegram modular (owner-only) untuk memantau balance/pemakaian/budget API InferHub sekaligus endpoint HTTP/API global.

## Fitur Utama

- **Dashboard Live:** Ringkasan status & balance InferHub, ringkasan endpoint `MONITOR_TARGETS`, interval polling, dan ambang batas alert.
- **Modul InferHub (opsional bila `INFERHUB_API_KEY` diset):**
  - `/status` & `/check`: Akun, balance, pending, health check `- [x]`.
  - `/usage` & `/today`: Ringkasan 30 hari, top model breakdown di `<details>`, dan cache hit visual bar hari ini.
  - `/breakdown`: Rincian per model.
  - `/budget` & `/setbudget <pct>`: Diskon minimum default dengan konfirmasi 2 langkah in-place.
  - `/keys`: Daftar API key terdaftar.
- **Modul Global Targets (`MONITOR_TARGETS`):**
  - Format: `Nama|URL;Nama2|URL2`.
  - `/targets` menampilkan tabel status HTTP & latency ms live.
- **Background Alert + Recovery:** Thread latar independen mengecek balance rendah, error API, dan target down/pulih dengan deduplikasi state.

## Konfigurasi

Lihat [crates/px-monitor/.env.example](../../crates/px-monitor/.env.example).
