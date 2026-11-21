# PX-Monitor

A modular, owner-only Telegram bot for monitoring InferHub API balance/usage/budgets alongside arbitrary global HTTP endpoints.

## Key Features

- **Live Dashboard:** Instant summary of InferHub account status & balance, global `MONITOR_TARGETS` health, polling interval, and alert threshold.
- **InferHub Module (enabled when `INFERHUB_API_KEY` is set):**
  - `/status` & `/check`: Account health, balance, pending funds, and native `- [x]` checks.
  - `/usage` & `/today`: 30-day summary, collapsible top-model breakdown, and today's cache-hit progress bar.
  - `/breakdown`: Per-model request, token, and cost breakdown.
  - `/budget` & `/setbudget <pct>`: Account-wide minimum discount with in-place 2-step confirmation.
  - `/keys`: Registered API keys table.
- **Global Targets Module (`MONITOR_TARGETS`):**
  - Format: `Name|URL;Name2|URL2`.
  - `/targets` probes all configured endpoints and reports live HTTP status codes and latency (ms).
- **Background Alerts & Recovery:** Independent checker thread sends rich alert and recovery notifications with state deduplication.

## Configuration

See [crates/px-monitor/.env.example](../../crates/px-monitor/.env.example).
