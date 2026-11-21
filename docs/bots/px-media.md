# PX-Media

Bot Telegram pribadi (owner-only) untuk mengunduh file Google Drive (`gdrive-dl`) dan Pixeldrain (`/u/<id>`) langsung ke folder library media, dilengkapi scan library Jellyfin.

## Fitur Utama

- **Dashboard Rich:** Status, tujuan aktif (tombol hijau edit di tempat), ringkasan jumlah item library, visual bar storage (`df`), dan status real-time Jellyfin (`Online` / `Off`).
- **Progres 1-Line In-Place:** Mengedit 1 pesan progres yang sama tiap 6–8 detik lengkap dengan progress bar, kecepatan MB/s, dan ETA.
- **Verifikasi File & Permission:** Mendeteksi file kosong atau halaman error HTML, otomatis `chmod 664` file & `chmod 775` folder tujuan.
- **Menu `🕒 Terbaru` (`/recent`):** Menampilkan 6 item terbaru di folder aktif + tombol `<tg-button type="copy_text">` untuk salin path.
- **Smart Scan Jellyfin:** Mengecek port Jellyfin lebih dulu agar menampilkan status ramah saat layanan sedang dimatikan.

## Konfigurasi

Lihat [crates/px-media/.env.example](../../crates/px-media/.env.example).
