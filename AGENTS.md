# AGENTS.md — PX Bots Monorepo

Panduan kerja kanonik untuk agent AI / Hermes saat membaca, mengembangkan, atau merilis bot di monorepo ini.

## Aturan Wajib

1. **Repo ini bersifat PUBLIC — nol data pribadi/internal di kode atau git history.**
   - Jangan pernah menaruh token, API key, chat ID, IP privat (`192.168.*`, `100.*`), domain pribadi, email, atau path `/home/<user>` di file yang di-track git.
   - Semua konfigurasi sensitif disimpan di vault eksternal (`runtime.env` mode `600`).
   - Wajib jalankan `./scripts/privacy-check.sh` dan pastikan hasil **0 temuan** sebelum `git push`.

2. **Mode Telegram-Only & UI Bot API 10.3+ Wajib Konsisten.**
   - Seluruh bot memakai primitif dari `crates/tg-core` (`TgClient`, `field_grid`, `table_markdown`, `hl`, `details_block`, `tg_button_row`, `stamp_wib`).
   - Klik tombol inline wajib mengedit pesan lama in-place (`tg.present(msg_id, ...)`) — jangan spam bubble baru.
   - Progres operasi panjang (seperti unduhan) wajib memperbarui 1 pesan secara in-place.

3. **Zero Rust Toolchain di Server Target.**
   - Compile, test, dan packaging dilakukan di GitHub Actions (`.github/workflows/release.yml`).
   - Di server target hanya dilakukan pull binary dari GitHub Releases + verifikasi SHA256 + `--version` + `--check`.

4. **Versioning & Changelog.**
   - Setiap perubahan fitur/bug/migrasi bot baru wajib memperbarui `CHANGELOG.md` (Keep a Changelog) dan menaikkan versi SemVer di `Cargo.toml` workspace + membuat git tag `vX.Y.Z`.
   - Saat menambah crate binary baru ke workspace, tambahkan pula ke loop packaging di `.github/workflows/release.yml`, tabel `README.md`, dan dokumen di `docs/bots/<nama>.md`.

5. **Isolasi Runtime & Operasi Service.**
   - Setiap bot berjalan dengan binary, unit `systemd --user`, file state, dan `runtime.env` sendiri.
   - Selalu backup unit/binary lama sebelum cutover.
