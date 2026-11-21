# PX-Mail

An owner-only Telegram TempMail operator built on `tg-core` (Telegram-only, Bot API 10.3+) with Cloudflare Worker + D1 state synchronization.

## Key Features

- **Rich Dashboard (`/start`, `/menu`):** Live mailbox counts (`24h` vs `Permanent`), configured domains, and D1 sync health.
- **Create & Bulk Generate (`/create`, `/generate`):** Create 24-hour or permanent mailboxes (custom or random local-part) or generate batches of up to 10 with one-tap `📋 Copy Address` / `📋 Copy All` pills.
- **Smart Inbox & Message Reader (`/inbox`):** Automatic 4–8 digit OTP extraction (skips years `19xx`/`20xx`) rendered as one-tap `📋 OTP <code>` copy pills, plus up to 4 clickable `<tg-button type="url">` action links extracted from HTML/text emails.
- **Cloudflare D1 Sync & Worker Push Compatibility:** Probes live mailbox status in batches of 100, auto-repairs stale JWTs via `/admin/claim`, preserves local state on transient outages, and matches the Worker's 64-bit FNV-1a `mailbox_ref` for push notification buttons (`inbox:msg:<ref>`).

## Configuration

See [crates/px-mail/.env.example](../../crates/px-mail/.env.example).
