# PX-Notes

An owner-only Telegram Markdown vault manager, bookmark browser, and full-text search bot built on `tg-core` (Telegram-only, Bot API 10.3+).

## Key Features

- **Rich Dashboard (`/menu`):** Live vault stats (note count, total bytes, saved links), recently modified files, and quick in-bubble pills.
- **Paginated Vault Browser (`/notes`):** Browse all `.md` files 8 per page with full-width click-to-open buttons edited in-place.
- **Note Viewer & Append (`/open <path>`):** Renders Markdown preview with size/lines/updated metadata, native `📋 Copy Path` pill, and an inline **✏️ Append Text** flow.
- **Safe Note Creation (`/new [path]`):** Enforces a strict vault path jail (rejects absolute paths, backslashes, and `..`), auto-creates subdirectories, and prevents accidental overwrites.
- **Bookmarks Browser & Saver (`/links`, `/link`):** Reads and appends `links/{api-sellers,otp-sms,proxies,tools,misc}.md` with category filters and clickable `<tg-button type="url">` pills.
- **Full-Text Search (`/search <query>`):** Case-insensitive search across paths and file contents with UTF-8 safe snippets.

## Configuration

See [crates/px-notes/.env.example](../../crates/px-notes/.env.example).
