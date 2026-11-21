# PX-Media

An owner-only Telegram bot for downloading Google Drive (`gdrive-dl`) and Pixeldrain (`/u/<id>`) files into your media library with optional Jellyfin library refresh.

## Key Features

- **Rich Dashboard:** Live status, active destination folder (green toggle button edited in-place), library item counts, visual storage bar (`df`), and real-time Jellyfin reachability check (`Online` / `Off`).
- **1-Line In-Place Progress:** Updates a single progress card every 6–8 seconds with a visual progress bar, transfer speed (MB/s), and ETA — zero message spam.
- **Integrity & Permissions:** Rejects empty files or HTML error pages, then applies `chmod 664` on downloaded files and `chmod 775` on target directories.
- **Recent Browser (`/recent`):** Lists the 6 most recent items in the active folder with a native `<tg-button type="copy_text">` pill to copy the folder path.
- **Smart Jellyfin Scan:** Probes the Jellyfin port before issuing `/Library/Refresh` so offline/parked instances return a clean status card instead of a raw connection error.

## Configuration

See [crates/px-media/.env.example](../../crates/px-media/.env.example).
