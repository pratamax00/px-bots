# Arsitektur PX Bots

## Prinsip Desain

1. **Monorepo Build, Isolated Runtime**
   Semua bot berbagi satu Cargo Workspace (`Cargo.toml`, `Cargo.lock`, `crates/tg-core`, dan cache CI), tetapi menghasilkan binary independen di `bin/px-*`. Di server, setiap binary berjalan di bawah unit `systemd --user` terpisah dengan `runtime.env` dan file state sendiri. Kegagalan atau restart satu bot tidak pernah memengaruhi bot lainnya.

2. **Zero-Tokio Blocking Concurrency**
   Setiap bot menggunakan `ureq` + `rustls` (dengan `webpki-roots` bawaan) dan `std::thread` ringan untuk callback ack paralel, background download worker, serta periodic health checker. Pendekatan ini menjaga ukuran binary ~3 MB dan pemakaian RAM live ~2–5 MB RSS per bot.

3. **Standar UI Telegram Bot API 10.3+**
   - Semua layar utama memakai `Field | Value` dengan nilai penting di-highlight (`==...==`).
   - Navigasi tombol menggunakan `editMessageText` in-place sehingga chat tetap bersih tanpa spam pesan baru.
   - Info sekunder (path lengkap, panduan, rincian panjang, log error) ditempatkan di bawah pemisah `---` di dalam `<details><summary>...</summary>`.
   - Aksi cepat di dalam bubble menggunakan `<tg-button-row>` (`callback_data`, `url`, `copy_text`).
   - Jika `sendRichMessage` / `editRich` gagal, `tg-core` otomatis menurunkan pesan ke HTML rapi (`parse_mode: "HTML"`).

4. **Keamanan Konfigurasi**
   Tidak ada nilai rahasia atau spesifik host di dalam repositori. Semua variabel wajib/opsional dimuat melalui `EnvironmentFile=` pada unit `systemd`.
