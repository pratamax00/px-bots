# Runbook Deploy & Rollback

## Prasyarat

- Binary static dirilis otomatis oleh GitHub Actions ke GitHub Releases (`latest` untuk branch `main`, atau `vX.Y.Z` untuk rilis bertag).
- File konfigurasi `runtime.env` (mode `600`) berada di luar direktori repo.

## Deploy / Update Satu Bot

```bash
# Tarik binary terbaru, verifikasi SHA256, dan uji --check
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env

# Atau dengan restart unit systemd otomatis (tambahkan flag --restart)
./scripts/deploy.sh px-media latest /path/to/px-media/runtime.env --restart
```

## Contoh Unit `systemd --user`

```ini
[Unit]
Description=PX-Media Telegram Bot
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

## Rollback Cepat

1. Setiap update oleh `deploy.sh` menyimpan salinan binary sebelumnya di `bin/<bot>.prev`.
2. Untuk rollback ke binary sebelum update:

```bash
cp bin/px-media.prev bin/px-media
systemctl --user restart px-media.service
```

3. Atau tarik ulang versi tag tertentu:

```bash
./scripts/deploy.sh px-media v0.1.0 /path/to/px-media/runtime.env --restart
```
