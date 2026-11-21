# Architecture

## Design Principles

1. **Monorepo Build, Isolated Runtime**
   All bots share a single Cargo Workspace (`Cargo.toml`, `Cargo.lock`, `crates/tg-core`, and CI build cache), while compiling into independent static binaries under `bin/px-*`. On the server, each binary runs under its own `systemd --user` unit with an isolated state file and `runtime.env`. Restarting or crashing one bot never affects another.

2. **Zero-Tokio Blocking Concurrency**
   Each bot relies on `ureq` + `rustls` (bundled `webpki-roots`) and lightweight OS threads (`std::thread`) for parallel callback acknowledgements, background downloads, and periodic health checks. This keeps binary size around ~3 MB and live memory footprint around ~2–5 MB RSS per bot.

3. **Telegram Bot API 10.3+ UI Standard**
   - Primary cards use `Field | Value` GFM grids with key metrics highlighted (`==...==`).
   - Inline button navigation edits the existing message in-place (`editMessageText`) to avoid chat clutter.
   - Secondary information (full paths, guides, long breakdowns, error logs) sits below a `---` divider inside collapsible `<details><summary>...</summary>` blocks.
   - **Hybrid Minimalis UI (Zero Duplicates):** contextual actions and clickable table rows live inside the bubble using styled `<tg-button>` (`primary`, `success`, `danger`, `link`, `url`, `copy_text`), while bottom `inline_keyboard` holds only a slim non-duplicate navigation bar.
   - **Single Entry Command:** all bots register `/start` as the sole dashboard entry in `setMyCommands` (no duplicate `/menu` listing).
   - If rich rendering fails, `tg-core` automatically falls back to clean HTML (`parse_mode: "HTML"`).

4. **Strict Configuration Hygiene**
   No secrets or host-specific identifiers exist in the repository. All configuration is injected via `EnvironmentFile=` in `systemd`.
