# AGENTS.md — PX Bots Monorepo

Canonical engineering guide for AI agents working on this repository.

## Core Rules

1. **Public Repository — Zero Personal or Internal Data.**
   - Never commit tokens, API keys, chat IDs, private LAN/Tailscale IPs (`192.168.*`, `100.*`), personal domains, emails, or `/home/<user>` paths.
   - All runtime configuration lives in external vault files (`runtime.env`, mode `600`).
   - Run `./scripts/privacy-check.sh` and confirm **0 findings** before every `git push`.

2. **100% English Project Convention.**
   - All bot UI text, button labels, command descriptions, log messages, code comments, scripts, and documentation in this repository must be written in clear, concise **English**.

3. **Telegram-Only & Bot API 10.3+ Rich UI Standard.**
   - Build all bots on `crates/tg-core` (`TgClient`, `field_grid`, `table_markdown`, `hl`, `details_block`, `tg_button_row`, `stamp_wib`).
   - Button interactions must edit the existing message in-place (`tg.present(msg_id, ...)`) rather than spamming new bubbles.
   - Long-running progress (e.g., downloads) must update a single message in-place on one concise progress line.

4. **Zero Rust Toolchain on Target Server.**
   - Compile, test, and package exclusively in GitHub Actions (`.github/workflows/release.yml`).
   - On the host, only pull release binaries via `scripts/deploy.sh` (SHA256 verified + `--version` + `--check`).

5. **SemVer & Changelog Discipline.**
   - Update `CHANGELOG.md` (Keep a Changelog) and bump workspace `version` in `Cargo.toml` + push a `vX.Y.Z` tag for every release.
   - When adding a new binary crate, register it in `.github/workflows/release.yml`, `README.md`, and `docs/bots/<name>.md`.
