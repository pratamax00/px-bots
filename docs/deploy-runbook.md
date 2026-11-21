# Deploy & Rollback Runbook

## Prerequisites

- Static musl binaries are built automatically by GitHub Actions and published to GitHub Releases (`latest` for `main`, or `vX.Y.Z` for tagged releases).
- Each bot requires an external `runtime.env` (`chmod 600`) stored outside this repository.

## Deploy or Update a Bot

```bash
# Download binary, verify SHA256, and run --check
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env

# Download, verify, and restart systemd user unit
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env --restart
```

## Example `systemd --user` Unit

```ini
[Unit]
Description=PX-Media Telegram Bot (Rust static binary)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=%h/apps/bots/px-bots
EnvironmentFile=%h/secrets/apps/px-media/runtime.env
ExecStart=%h/apps/bots/px-bots/bin/px-media
Restart=on-failure
RestartSec=5
MemoryMax=200M

[Install]
WantedBy=default.target
```

## Instant Rollback

1. Every deployment saves the previous binary as `bin/<bot>.prev`.
2. Restore the previous binary immediately:

```bash
cp bin/px-media.prev bin/px-media
systemctl --user restart px-media.service
```

3. Or pin any tagged release:

```bash
./scripts/deploy.sh px-media v0.1.1 /path/to/px-media/runtime.env --restart
```
