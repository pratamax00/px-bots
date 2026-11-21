# PX-Ops

Native Rust Telegram host operations and breakglass console built on `tg-core` for **Telegram Bot API 10.3+** (**Telegram-only**, ~3 MB RSS).

## Features

- **Live Host Dashboard (`/start`):** Native Linux `/proc` + `/sys` telemetry with visual progress bars for **CPU**, **RAM**, **Swap**, and **Disk /**, CPU temperature, maintenance mode status, quick `<tg-button>` pills, and collapsible **Core Services** overview.
- **Optional Heavy Apps Quick Toggle (`/apps`):** One-tap start/stop panel for heavy RAM-saving apps (`group: "apps"`) across `systemd-user`, `systemd-system`, and `docker` containers, automatically updating `manual_off` so intentionally parked apps never trigger false alerts.
- **Service Control (`/services`):** Paginated batch inspection of `systemd --user`, `sudo systemctl`, and `docker inspect` services with live RAM, PID, restarts, uptime, `journalctl` / `docker logs`, and confirmed `Start` / `Stop` / `Restart` (including safe delayed self-restart via `systemd-run`).
- **Host Server Panel (`/server`):** System info, Storage & Inodes, Network interfaces/routes/listening ports, Top CPU Processes, APT upgradable check, SMART disk health, and confirmed background `apt-get update && upgrade -y` with sudo/apt preflight.
- **9Router Control (`/router`):** Quick & Full health checks, `Providers` / `Nodes` / `Models` / `API Keys` / `Combos` browsers, and atomic `9router-catalog.json` refresh using pure-Rust SHA-256 `x-9r-cli-token` authentication.
- **Agent Sessions Control (`/sessions`):** Pure-Rust management of `sessions.json`, `session_modes.json`, and `session_compress.json` (individual or bulk `santai` / `hemat` / `fullpower` switching, compression thresholds, and live 9Router model overrides with reload flag).
- **Backup Manager (`/backup`):** Compressed `.tar.gz` creation with pure-Rust SHA-256 manifests, retention pruning, `Backup All`, integrity verification (`Verify SHA-256` + `tar -tzf`), and confirmed `Restore` with automatic `pre-restore` checkpoint.
- **Power Control (`/power`):** Confirmed host `Reboot` and **2-stage double-confirmed** `Shutdown`.
- **Background Alert Monitor:** Polls CPU load, RAM, Disk, Temperature, and core services on a configurable interval with transitional-state filtering, `unknown`-state retry, per-issue cooldown persistence, maintenance pause, and OPS Forum topic routing (`dm` / `ops` / `dual` with automatic owner DM fallback).
- **Emergency Tools & Shell:** Self-check (`sudo -n`, `NoNewPrivs=0`, APT write check), Hermes CLI version/doctor/profiles, downloadable Diagnostics Bundle (`0600`), recent `audit.jsonl`, `/exec <cmd>`, `/cwd`, and `/envkeys`.

## Configuration

Copy [`crates/px-ops/.env.example`](../../crates/px-ops/.env.example) to a private vault (`chmod 600`) and populate `OPS_CONFIG_DIR` (`chmod 700`) using the templates in [`crates/px-ops/examples/`](../../crates/px-ops/examples/):

- `services.json`
- `backup-targets.json`
- `thresholds.json`
- `ops-forum.json`

> **Important `systemd` Note:** Because `px-ops` executes host `sudo` operations (`apt`, `systemctl`, `smartctl`, `reboot`, `poweroff`), keep `NoNewPrivileges=false` and omit `ProtectSystem` / `ProtectHome` / `PrivateTmp` in its `systemd --user` unit.

## Verify & Deploy

```bash
./scripts/deploy.sh px-ops latest /path/to/px-ops/runtime.env --restart
```
