use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tg_core::bot::TgClient;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, format_duration, hl, human_bytes, progress_bar, stamp_wib,
    table_markdown, tail_str, tg_button_row, tg_cb_button, tg_cb_button_styled, tg_copy_button, tg_url_button,
    truncate_str,
};
use tg_core::{log_error, log_info, log_warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PIXELDRAIN_API: &str = "https://pixeldrain.com/api/file";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
const TARGET_KEYS: [&str; 3] = ["films", "series", "anime"];

// ── configuration (100% env-only) ───────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    state_file: PathBuf,
    gdrive_dl: String,
    dir_films: String,
    dir_series: String,
    dir_anime: String,
    jellyfin_url: String,
    jellyfin_token: String,
    link_filebrowser: String,
    link_jellyfin_lan: String,
    link_jellyfin_ts: String,
    server_ip: String,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let tg_token = env::var("TG_TOKEN")
            .or_else(|_| env::var("TELEGRAM_BOT_TOKEN"))
            .map_err(|_| "TG_TOKEN environment variable is required".to_string())?;
        if tg_token.trim().is_empty() {
            return Err("TG_TOKEN is empty".to_string());
        }
        let owner_raw = env::var("OWNER_ID")
            .or_else(|_| env::var("TELEGRAM_OWNER_ID"))
            .map_err(|_| "OWNER_ID environment variable is required".to_string())?;
        let owner_id = owner_raw
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("Invalid OWNER_ID: {e}"))?;

        Ok(Self {
            tg_token,
            owner_id,
            state_file: PathBuf::from(
                env::var("STATE_FILE").unwrap_or_else(|_| "./state/px-media.json".to_string()),
            ),
            gdrive_dl: env::var("GDRIVE_DL").unwrap_or_else(|_| "gdrive-dl".to_string()),
            dir_films: env::var("DIR_FILMS")
                .unwrap_or_else(|_| "/mnt/storage/media/films".to_string()),
            dir_series: env::var("DIR_SERIES")
                .unwrap_or_else(|_| "/mnt/storage/media/series".to_string()),
            dir_anime: env::var("DIR_ANIME")
                .unwrap_or_else(|_| "/mnt/storage/media/anime".to_string()),
            jellyfin_url: env::var("JELLYFIN_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8096".to_string()),
            jellyfin_token: env::var("JELLYFIN_TOKEN").unwrap_or_default(),
            link_filebrowser: env::var("LINK_FILEBROWSER").unwrap_or_default(),
            link_jellyfin_lan: env::var("LINK_JELLYFIN_LAN").unwrap_or_default(),
            link_jellyfin_ts: env::var("LINK_JELLYFIN_TS").unwrap_or_default(),
            server_ip: env::var("SERVER_IP").unwrap_or_default(),
        })
    }

    fn target_path(&self, folder: &str) -> Option<&str> {
        match folder {
            "films" => Some(self.dir_films.as_str()),
            "series" => Some(self.dir_series.as_str()),
            "anime" => Some(self.dir_anime.as_str()),
            _ => None,
        }
    }
}

fn target_label(folder: &str) -> &'static str {
    match folder {
        "films" => "🎬 Films",
        "series" => "📺 Series",
        "anime" => "🌸 Anime",
        _ => "📁 Other",
    }
}

// ── state ───────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Pending {
    folder: String,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fid: Option<String>,
}

fn default_kind() -> String {
    "gdrive".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    pending: Option<Pending>,
}

// ── app ─────────────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    tg: Arc<TgClient>,
    state: Mutex<BotState>,
    busy: Mutex<HashSet<String>>,
}

impl App {
    fn new(cfg: Config) -> Arc<Self> {
        let tg = TgClient::new(cfg.tg_token.clone(), cfg.owner_id);
        let initial_state: BotState = load_json(&cfg.state_file);
        Arc::new(Self {
            cfg,
            tg,
            state: Mutex::new(initial_state),
            busy: Mutex::new(HashSet::new()),
        })
    }

    fn persist_state(&self, s: &BotState) {
        save_json_atomic(&self.cfg.state_file, s);
    }

    fn kb_menu(&self) -> Value {
        json!({
            "inline_keyboard": [[
                button("🕒 Recent", "act:recent", "primary"),
                button("📡 Scan", "act:scan", "primary"),
                button("🔗 Links", "act:links", "primary"),
            ]]
        })
    }

    fn menu_text(&self) -> String {
        let active_folder = { self.state.lock().unwrap().folder.clone() };
        let busy_count = { self.busy.lock().unwrap().len() };
        let jf_up = jellyfin_online(&self.cfg.jellyfin_url);

        let mut counts = [0usize; 3];
        let mut detail_rows = Vec::new();
        for (idx, k) in TARGET_KEYS.iter().enumerate() {
            if let Some(p) = self.cfg.target_path(k) {
                let c = fs::read_dir(p)
                    .map(|rd| rd.filter_map(Result::ok).count())
                    .unwrap_or(0);
                counts[idx] = c;
                detail_rows.push(vec![
                    target_label(k).to_string(),
                    format!("{c} items"),
                    format!("`{p}`"),
                ]);
            }
        }

        let status_val = if busy_count == 0 {
            hl("🟢 Ready")
        } else {
            hl(&format!("⬇️ Downloading ({busy_count})"))
        };
        let target_val = active_folder
            .as_deref()
            .map(|k| hl(target_label(k)))
            .unwrap_or_else(|| "Not selected".to_string());
        let library_val = hl(&format!(
            "{} Films · {} Series · {} Anime",
            counts[0], counts[1], counts[2]
        ));
        let disk_val = disk_usage_visual(&self.cfg.dir_films);
        let jf_val = if jf_up {
            hl("🟢 Online")
        } else {
            "⏸️ Parked (RAM saver)".to_string()
        };

        let metrics = field_grid(&[
            ("Status", status_val),
            ("Target", target_val),
            ("Library", library_val),
            ("Storage", disk_val),
            ("Jellyfin", jf_val),
        ]);

        let style_for = |k: &str| {
            if active_folder.as_deref() == Some(k) {
                "success"
            } else {
                "primary"
            }
        };
        let mut pills = vec![
            tg_cb_button_styled(target_label("films"), "pick:films", style_for("films")),
            tg_cb_button_styled(target_label("series"), "pick:series", style_for("series")),
            tg_cb_button_styled(target_label("anime"), "pick:anime", style_for("anime")),
            tg_cb_button_styled("🔄 Refresh", "act:menu", "primary"),
        ];
        if !self.cfg.link_filebrowser.is_empty() {
            pills.push(tg_url_button("📂 Files", &self.cfg.link_filebrowser));
        }

        let prompt_line = match active_folder.as_deref() {
            Some(k) => format!(
                "Send a **Google Drive** or **Pixeldrain** link to download into **{}**.",
                target_label(k)
            ),
            None => "Pick a target folder below, then send a download link.".to_string(),
        };

        let detail_body = format!(
            "{}\n\n- A **green** folder button marks the active destination.\n- Download progress updates live on a single line in-place.",
            table_markdown(&["Folder", "Items", "Path"], &detail_rows)
        );

        [
            format!("**📦 PX-Media** · {}", hl(&format!("v{VERSION}"))),
            String::new(),
            metrics,
            String::new(),
            prompt_line,
            format!("_{}_", stamp_wib()),
            String::new(),
            tg_button_row(&pills),
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Library Paths & Guide", &detail_body),
        ]
        .join("\n")
    }

    fn recent_text(&self) -> String {
        let folder = {
            self.state
                .lock()
                .unwrap()
                .folder
                .clone()
                .unwrap_or_else(|| "anime".to_string())
        };
        let path_str = self
            .cfg
            .target_path(&folder)
            .unwrap_or(self.cfg.dir_anime.as_str());
        let recent = list_recent_files(Path::new(path_str), 6);

        let body = if recent.is_empty() {
            "_Folder is empty._".to_string()
        } else {
            let rows_data: Vec<Vec<String>> = recent
                .iter()
                .map(|(name, size, _)| {
                    vec![
                        format!("`{}`", truncate_str(name, 30)),
                        if *size > 0 {
                            hl(&human_bytes(*size))
                        } else {
                            "📁 dir".into()
                        },
                    ]
                })
                .collect();
            table_markdown(&["Recent Item", "Size"], &rows_data)
        };

        let pills = tg_button_row(&[
            tg_copy_button("📋 Copy Path", path_str),
            tg_cb_button("🔄 Refresh", "act:recent"),
            tg_cb_button("📁 Menu", "act:menu"),
        ]);

        [
            format!("**🕒 PX-Media · Recent** · {}", hl(target_label(&folder))),
            String::new(),
            body,
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Folder Path", &format!("`{path_str}`")),
        ]
        .join("\n")
    }

    fn links_text(&self) -> String {
        let mut service_rows = Vec::new();
        let mut pills = Vec::new();

        if !self.cfg.link_filebrowser.is_empty() {
            service_rows.push(vec!["Filebrowser".into(), self.cfg.link_filebrowser.clone()]);
            pills.push(tg_url_button("📂 Filebrowser", &self.cfg.link_filebrowser));
        }
        if !self.cfg.link_jellyfin_lan.is_empty() {
            service_rows.push(vec!["Jellyfin LAN".into(), self.cfg.link_jellyfin_lan.clone()]);
            pills.push(tg_url_button("🎬 Jellyfin LAN", &self.cfg.link_jellyfin_lan));
        }
        if !self.cfg.link_jellyfin_ts.is_empty() {
            service_rows.push(vec!["Jellyfin TS".into(), self.cfg.link_jellyfin_ts.clone()]);
            pills.push(tg_url_button("🌐 Jellyfin TS", &self.cfg.link_jellyfin_ts));
        }
        if !self.cfg.server_ip.is_empty() {
            pills.push(tg_copy_button("📋 Copy IP", &self.cfg.server_ip));
        }

        let mut path_rows = Vec::new();
        for k in TARGET_KEYS {
            if let Some(p) = self.cfg.target_path(k) {
                path_rows.push(vec![target_label(k).to_string(), format!("`{p}`")]);
            }
        }
        let mut notes = vec![
            table_markdown(&["Folder", "Path"], &path_rows),
            String::new(),
            "- Filebrowser is reachable externally via tunnel.".to_string(),
            "- Use **LAN** on your local network; **Tailscale** when away.".to_string(),
        ];
        if !self.cfg.server_ip.is_empty() {
            notes.push(format!("- LAN Host: `{}`", self.cfg.server_ip));
        }

        let main_table = if service_rows.is_empty() {
            "No media service URLs configured in environment.".to_string()
        } else {
            table_markdown(&["Service", "URL"], &service_rows)
        };

        let mut blocks = vec![
            format!("**🔗 PX-Media · Access Links** · {}", hl("Media Services")),
            String::new(),
            main_table,
            String::new(),
            format!("_{}_", stamp_wib()),
        ];
        if !pills.is_empty() {
            blocks.push(String::new());
            blocks.push(tg_button_row(&pills));
        }
        blocks.push(String::new());
        blocks.push("---".to_string());
        blocks.push(String::new());
        blocks.push(details_block("Paths & Notes", &notes.join("\n")));
        blocks.join("\n")
    }

    fn jellyfin_scan(&self) -> String {
        if self.cfg.jellyfin_token.is_empty() {
            return format!(
                "**📡 PX-Media · Jellyfin Scan** · {}\n\n`JELLYFIN_TOKEN` is not configured.\n\n_{}_",
                hl("⏸️ Disabled"),
                stamp_wib()
            );
        }
        if !jellyfin_online(&self.cfg.jellyfin_url) {
            let tbl = field_grid(&[
                ("Status", hl("⏸️ Jellyfin Off")),
                ("Endpoint", format!("`{}`", self.cfg.jellyfin_url)),
            ]);
            return [
                "**📡 PX-Media · Jellyfin Scan**".to_string(),
                String::new(),
                tbl,
                String::new(),
                "Jellyfin is currently parked to save RAM. Start it via **PX-Ops → Apps** when ready to watch or scan.".to_string(),
                format!("_{}_", stamp_wib()),
            ]
            .join("\n");
        }

        let url = format!("{}/Library/Refresh", self.cfg.jellyfin_url.trim_end_matches('/'));
        let auth = format!("MediaBrowser Token=\"{}\"", self.cfg.jellyfin_token);
        let t0 = Instant::now();
        match self.tg.agent().post(&url).set("Authorization", &auth).call() {
            Ok(resp) => {
                let status = resp.status();
                let ms = t0.elapsed().as_millis();
                if status == 200 || status == 204 {
                    let tbl = field_grid(&[
                        ("Status", hl("✅ Triggered")),
                        ("Response", hl(&format!("HTTP {status} · {ms} ms"))),
                        ("Endpoint", format!("`{}`", self.cfg.jellyfin_url)),
                    ]);
                    format!(
                        "**📡 PX-Media · Jellyfin Scan**\n\n{tbl}\n\nLibrary refresh is running in the background. New posters and metadata will appear shortly.\n\n_{}_",
                        stamp_wib()
                    )
                } else {
                    format!(
                        "**📡 PX-Media · Jellyfin Scan** · {}\n\nJellyfin replied with HTTP `{status}` ({ms} ms).\n\n_{}_",
                        hl("⚠️ Warning"),
                        stamp_wib()
                    )
                }
            }
            Err(e) => {
                let msg = format!("{e}");
                format!(
                    "**📡 PX-Media · Jellyfin Scan** · {}\n\n_{}_\n\n---\n\n{}",
                    hl("❌ Failed"),
                    stamp_wib(),
                    details_block("Error Details", &format!("`{}`", truncate_str(&msg, 240)))
                )
            }
        }
    }

    fn pixeldrain_info(&self, fid: &str) -> Value {
        let url = format!("{PIXELDRAIN_API}/{fid}/info");
        match self.tg.agent().get(&url).set("User-Agent", "Mozilla/5.0").call() {
            Ok(resp) => resp.into_json::<Value>().unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    }
}

// ── domain helpers ──────────────────────────────────────────────────────────
fn drive_id(s: &str) -> Option<String> {
    static RE_FULL: OnceLock<Regex> = OnceLock::new();
    static RE_URL: OnceLock<Regex> = OnceLock::new();

    let s = s.trim();
    let re_full = RE_FULL.get_or_init(|| Regex::new(r"^[A-Za-z0-9_-]{20,}$").unwrap());
    if re_full.is_match(s) {
        return Some(s.to_string());
    }
    let re_url = RE_URL.get_or_init(|| Regex::new(r"(?:/d/|id=)([A-Za-z0-9_-]{20,})").unwrap());
    re_url.captures(s).map(|c| c[1].to_string())
}

fn pixeldrain_id(s: &str) -> Option<(String, String)> {
    static RE_PD: OnceLock<Regex> = OnceLock::new();
    let re = RE_PD.get_or_init(|| Regex::new(r"pixeldrain\.com/([ul])/([A-Za-z0-9]+)").unwrap());
    re.captures(s.trim())
        .map(|c| (c[1].to_string(), c[2].to_string()))
}

fn format_progress_line(bytes: u64, total: Option<u64>, elapsed_secs: u64) -> String {
    let speed_bps = if elapsed_secs > 0 {
        bytes as f64 / elapsed_secs as f64
    } else {
        0.0
    };
    let speed_mb = speed_bps / 1024.0 / 1024.0;
    let dur = format_duration(elapsed_secs);
    match total {
        Some(tot) if tot > 0 => {
            let pct = ((bytes as f64 / tot as f64) * 100.0).clamp(0.0, 100.0);
            let bar = progress_bar(bytes as f64 / tot as f64, 10);
            let eta = if speed_bps > 1.0 && tot > bytes {
                format!(
                    "ETA {}",
                    format_duration(((tot - bytes) as f64 / speed_bps) as u64)
                )
            } else {
                dur
            };
            format!(
                "📥 {bar} {} · {} · `{speed_mb:.1} MB/s` · `{eta}`",
                hl(&format!("{pct:.0}%")),
                hl(&format!("{} / {}", human_bytes(bytes), human_bytes(tot)))
            )
        }
        _ => format!(
            "📥 {} · {} · `{dur}`",
            hl(&human_bytes(bytes)),
            hl(&format!("{speed_mb:.1} MB/s"))
        ),
    }
}

fn disk_usage_visual(path: &str) -> String {
    if let Ok(o) = Command::new("df")
        .args(["-h", "--output=used,size,pcent", path])
        .output()
    {
        if o.status.success() {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = text.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let pct_num = parts[2].trim_end_matches('%').parse::<f64>().unwrap_or(0.0);
                    let bar = progress_bar(pct_num / 100.0, 8);
                    return format!("{bar} {} · {}/{}", hl(parts[2]), parts[0], parts[1]);
                }
            }
        }
    }
    "—".to_string()
}

fn jellyfin_online(url: &str) -> bool {
    let stripped = url
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host_port = stripped.split('/').next().unwrap_or("");
    let addr_str = if host_port.contains(':') {
        host_port.to_string()
    } else if url.starts_with("https://") {
        format!("{host_port}:443")
    } else {
        format!("{host_port}:80")
    };
    if let Ok(mut addrs) = addr_str.to_socket_addrs() {
        if let Some(addr) = addrs.next() {
            return TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok();
        }
    }
    false
}

fn list_recent_files(dir: &Path, limit: usize) -> Vec<(String, u64, i64)> {
    let mut items = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                let size = if meta.is_file() { meta.len() } else { 0 };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                items.push((name, size, mtime));
            }
        }
    }
    items.sort_by(|a, b| b.2.cmp(&a.2));
    items.truncate(limit);
    items
}

fn looks_complete(path: &Path) -> (bool, String) {
    let size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return (false, "file not found".to_string()),
    };
    if size == 0 {
        return (false, "file is 0 bytes".to_string());
    }
    let mut buf = [0u8; 512];
    let n = match File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => 0,
    };
    let head_str = String::from_utf8_lossy(&buf[..n]);
    let trimmed = head_str.trim_start().to_ascii_lowercase();
    let prefix200: String = trimmed.chars().take(200).collect();
    if trimmed.starts_with("<!doctype html")
        || trimmed.starts_with("<html")
        || prefix200.contains("<head")
    {
        return (false, "file contains an HTML page".to_string());
    }
    (true, format!("{size} bytes"))
}

fn is_archive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".rar") || lower.ends_with(".zip") || lower.ends_with(".7z")
}

// ── keyboards ───────────────────────────────────────────────────────────────
fn kb_confirm(folder: &str) -> Value {
    json!({
        "inline_keyboard": [[
            button(&format!("✅ Download to {}", target_label(folder)), "go:yes", "success"),
            button("❌ Cancel", "go:no", "danger"),
        ]]
    })
}

fn kb_after() -> Value {
    json!({
        "inline_keyboard": [
            [
                button("📡 Scan Jellyfin", "act:scan", "success"),
                button("🕒 Recent", "act:recent", "primary"),
            ],
            [
                button("🔗 Access Links", "act:links", "primary"),
                button("📁 Dashboard", "act:menu", "primary"),
            ],
        ]
    })
}

fn kb_recent() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Dashboard", "act:menu", "primary"),
            button("🔄 Refresh", "act:recent", ""),
        ]]
    })
}

fn kb_links() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Dashboard", "act:menu", "primary"),
            button("🔄 Refresh", "act:links", ""),
        ]]
    })
}

fn kb_back_menu() -> Value {
    json!({
        "inline_keyboard": [[button("📁 Dashboard", "act:menu", "primary")]]
    })
}

// ── rich download cards ─────────────────────────────────────────────────────
fn build_progress_card(
    source: &str,
    folder: &str,
    file_name: &str,
    dest: &Path,
    bytes: u64,
    total: Option<u64>,
    elapsed_secs: u64,
) -> String {
    let line = if bytes == 0 {
        format!("📥 {} · `{}`", hl("Connecting…"), format_duration(elapsed_secs))
    } else {
        format_progress_line(bytes, total, elapsed_secs)
    };
    let tbl = field_grid(&[
        ("File", format!("`{}`", truncate_str(file_name, 40))),
        ("Folder", target_label(folder).to_string()),
        ("Source", source.to_string()),
    ]);
    [
        format!("**⬇️ PX-Media · Downloading** · {}", hl("Running")),
        String::new(),
        line,
        String::new(),
        tbl,
        String::new(),
        format!("_{}_", stamp_wib()),
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Destination", &format!("Path: `{}`", dest.display())),
    ]
    .join("\n")
}

fn build_done_card(
    source: &str,
    folder: &str,
    fname: &str,
    full_path: &Path,
    size: u64,
    elapsed_secs: u64,
    extra_notes: &[String],
) -> String {
    let avg_mb_s = if elapsed_secs > 0 {
        size as f64 / elapsed_secs as f64 / 1024.0 / 1024.0
    } else {
        size as f64 / 1024.0 / 1024.0
    };
    let tbl = field_grid(&[
        ("Status", hl("✅ Complete")),
        ("File", format!("`{}`", truncate_str(fname, 42))),
        ("Size", hl(&human_bytes(size))),
        (
            "Speed",
            format!(
                "{} · {}",
                hl(&format!("{avg_mb_s:.1} MB/s")),
                hl(&format_duration(elapsed_secs))
            ),
        ),
        ("Folder", target_label(folder).to_string()),
        ("Source", source.to_string()),
    ]);
    let path_str = full_path.to_string_lossy();
    let pills = tg_button_row(&[
        tg_cb_button("📡 Scan", "act:scan"),
        tg_copy_button("📋 Copy Path", &path_str),
        tg_cb_button("📁 Menu", "act:menu"),
    ]);
    let mut notes = vec![format!("Path: `{path_str}`")];
    notes.extend(extra_notes.iter().cloned());

    [
        "**✅ PX-Media · Download Complete**".to_string(),
        String::new(),
        tbl,
        String::new(),
        "- [x] Verified non-empty & non-HTML payload".to_string(),
        "- [x] Permissions applied (`664` / `775`) for Jellyfin".to_string(),
        String::new(),
        format!("_{}_", stamp_wib()),
        String::new(),
        pills,
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Path & Notes", &notes.join("\n")),
    ]
    .join("\n")
}

fn build_error_card(
    source: &str,
    folder: &str,
    reason: &str,
    hint: &str,
    err_log: &str,
    elapsed_secs: u64,
) -> String {
    let tbl = field_grid(&[
        ("Status", hl("❌ Failed")),
        ("Reason", reason.to_string()),
        ("Folder", target_label(folder).to_string()),
        ("Source", format!("{source} · {}", format_duration(elapsed_secs))),
    ]);
    [
        "**❌ PX-Media · Download Failed**".to_string(),
        String::new(),
        tbl,
        String::new(),
        hint.to_string(),
        format!("_{}_", stamp_wib()),
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Error Log", &format!("```\n{err_log}\n```")),
    ]
    .join("\n")
}

// ── subprocess runner (6h timeout, pipe-deadlock safe) ──────────────────────
enum CmdOutcome {
    Finished { code: i32, stdout: String, stderr: String },
    TimedOut,
    SpawnError(String),
}

fn run_cmd_with_timeout(program: &str, args: &[String], timeout: Duration) -> CmdOutcome {
    let mut child = match Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return CmdOutcome::SpawnError(format!("{e}")),
    };
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_h = thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = out_pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let err_h = thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = err_pipe {
            let _ = p.read_to_string(&mut s);
        }
        s
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return CmdOutcome::Finished {
                    code: status.code().unwrap_or(-1),
                    stdout: out_h.join().unwrap_or_default(),
                    stderr: err_h.join().unwrap_or_default(),
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CmdOutcome::TimedOut;
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(e) => {
                let _ = child.kill();
                return CmdOutcome::SpawnError(format!("{e}"));
            }
        }
    }
}

fn snapshot_dir(dest: &Path) -> HashMap<String, (u64, u128)> {
    let mut snap = HashMap::new();
    if let Ok(rd) = fs::read_dir(dest) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    snap.insert(name, (meta.len(), mtime));
                }
            }
        }
    }
    snap
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < total {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    stop.load(Ordering::Relaxed)
}

// ── download workers ────────────────────────────────────────────────────────
fn run_download(app: Arc<App>, folder: String, url: String, status_msg_id: Option<i64>) {
    let Some(dest_str) = app.cfg.target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    app.busy.lock().unwrap().insert(folder.clone());

    let started = Instant::now();
    let start_card =
        build_progress_card("Google Drive", &folder, "Preparing file…", &dest, 0, None, 0);
    let live_msg_id = app.tg.present(status_msg_id, &start_card, None);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let dest_watch = dest.clone();
    let folder_watch = folder.clone();
    let before = snapshot_dir(&dest);
    let before_watch = before.clone();

    let watcher = thread::spawn(move || {
        let mut last_bytes: u64 = 0;
        while !sleep_interruptible(&stop_watch, Duration::from_secs(8)) {
            let current = snapshot_dir(&dest_watch);
            let mut active: Option<(String, u64)> = None;
            for (name, (sz, mtime)) in &current {
                let changed = before_watch
                    .get(name)
                    .map(|(old_sz, old_mt)| old_sz != sz || old_mt != mtime)
                    .unwrap_or(true);
                if changed && active.as_ref().map(|(_, s)| *sz > *s).unwrap_or(true) {
                    active = Some((name.clone(), *sz));
                }
            }
            if let Some((name, bytes)) = active {
                if bytes > last_bytes {
                    last_bytes = bytes;
                    let card = build_progress_card(
                        "Google Drive",
                        &folder_watch,
                        &name,
                        &dest_watch,
                        bytes,
                        None,
                        started.elapsed().as_secs(),
                    );
                    app_watch.tg.present(live_msg_id, &card, None);
                }
            }
        }
    });

    let args = vec![url, "-o".to_string(), dest.to_string_lossy().into_owned()];
    let outcome = run_cmd_with_timeout(&app.cfg.gdrive_dl, &args, DOWNLOAD_TIMEOUT);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let elapsed = started.elapsed().as_secs();

    match outcome {
        CmdOutcome::Finished { code, stdout, stderr } => {
            let after = snapshot_dir(&dest);
            let mut changed: Vec<(&String, &(u64, u128))> = after
                .iter()
                .filter(|(n, meta)| before.get(*n) != Some(*meta))
                .collect();
            changed.sort_by_key(|(_, (sz, _))| *sz);
            let fname = changed.last().map(|(n, _)| (*n).clone());

            let (ok_file, reason) = match &fname {
                Some(n) => looks_complete(&dest.join(n)),
                None => (false, "no new or modified file detected".to_string()),
            };
            if ok_file {
                let fname = fname.unwrap();
                let f = dest.join(&fname);
                let size = fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
                let _ = Command::new("chmod").args(["664", &f.to_string_lossy()]).status();
                let _ = Command::new("chmod").args(["775", &dest.to_string_lossy()]).status();

                let mut notes = Vec::new();
                if is_archive(&fname) {
                    notes.push("📦 Archive detected (`.rar`/`.zip`/`.7z`).".to_string());
                }
                if code != 0 {
                    notes.push(format!(
                        "⚠️ Exit code `{code}`, but payload verified intact ({reason})."
                    ));
                }
                let card = build_done_card("Google Drive", &folder, &fname, &f, size, elapsed, &notes);
                app.tg.present(live_msg_id, &card, Some(kb_after()));
            } else {
                let raw_err = if !stderr.trim().is_empty() {
                    stderr.trim()
                } else {
                    stdout.trim()
                };
                let err = if raw_err.is_empty() {
                    "no stderr output".to_string()
                } else {
                    tail_str(raw_err, 600)
                };
                let hint = if code == 0 && fname.is_none() {
                    "No file changes detected — the file may already be complete in the target folder."
                } else {
                    "Possible causes: private link, Google Drive quota exceeded, or deleted file."
                };
                let card = build_error_card("Google Drive", &folder, &reason, hint, &err, elapsed);
                app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
            }
        }
        CmdOutcome::TimedOut => {
            let card = build_error_card(
                "Google Drive",
                &folder,
                "6-hour timeout",
                "Download was terminated after exceeding the 6-hour limit.",
                "timeout",
                elapsed,
            );
            app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("download failed: {e}"));
            let card = build_error_card(
                "Google Drive",
                &folder,
                "Failed to spawn gdrive-dl",
                "Check the `GDRIVE_DL` executable path in your environment.",
                &truncate_str(&e, 240),
                elapsed,
            );
            app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
        }
    }

    app.busy.lock().unwrap().remove(&folder);
}

fn run_pixeldrain(app: Arc<App>, folder: String, fid: String, status_msg_id: Option<i64>) {
    let Some(dest_str) = app.cfg.target_path(&folder) else {
        return;
    };
    let dest = PathBuf::from(dest_str);
    app.busy.lock().unwrap().insert(folder.clone());

    let started = Instant::now();
    let info = app.pixeldrain_info(&fid);
    let name = info
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| fid.clone());
    let expected_size = info.get("size").and_then(Value::as_u64).filter(|&s| s > 0);
    let target = dest.join(&name);

    let start_card =
        build_progress_card("Pixeldrain", &folder, &name, &dest, 0, expected_size, 0);
    let live_msg_id = app.tg.present(status_msg_id, &start_card, None);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_watch = Arc::clone(&stop);
    let app_watch = Arc::clone(&app);
    let target_watch = target.clone();
    let dest_watch = dest.clone();
    let label_watch = name.clone();
    let folder_watch = folder.clone();

    let watcher = thread::spawn(move || {
        let mut last_bytes: u64 = 0;
        while !sleep_interruptible(&stop_watch, Duration::from_secs(6)) {
            if let Ok(meta) = fs::metadata(&target_watch) {
                let bytes = meta.len();
                if bytes > last_bytes {
                    last_bytes = bytes;
                    let card = build_progress_card(
                        "Pixeldrain",
                        &folder_watch,
                        &label_watch,
                        &dest_watch,
                        bytes,
                        expected_size,
                        started.elapsed().as_secs(),
                    );
                    app_watch.tg.present(live_msg_id, &card, None);
                }
            }
        }
    });

    let args = vec![
        "-fL".to_string(),
        "--retry".to_string(),
        "3".to_string(),
        "--retry-delay".to_string(),
        "5".to_string(),
        "-C".to_string(),
        "-".to_string(),
        "-o".to_string(),
        target.to_string_lossy().into_owned(),
        format!("{PIXELDRAIN_API}/{fid}?download"),
    ];
    let outcome = run_cmd_with_timeout("curl", &args, DOWNLOAD_TIMEOUT);

    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let elapsed = started.elapsed().as_secs();

    match outcome {
        CmdOutcome::Finished { code, stderr, .. } => {
            if code == 0 {
                let _ = Command::new("chmod").args(["664", &target.to_string_lossy()]).status();
                let _ = Command::new("chmod").args(["775", &dest.to_string_lossy()]).status();
                let final_size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                let mut notes = Vec::new();
                if is_archive(&name) {
                    notes.push("📦 Archive detected (`.rar`/`.zip`/`.7z`).".to_string());
                }
                let card =
                    build_done_card("Pixeldrain", &folder, &name, &target, final_size, elapsed, &notes);
                app.tg.present(live_msg_id, &card, Some(kb_after()));
            } else {
                let err = if stderr.trim().is_empty() {
                    "no stderr output".to_string()
                } else {
                    tail_str(stderr.trim(), 600)
                };
                let card = build_error_card(
                    "Pixeldrain",
                    &folder,
                    &format!("curl exit {code}"),
                    "Possible causes: deleted file, expired link, or rate-limit.",
                    &err,
                    elapsed,
                );
                app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
            }
        }
        CmdOutcome::TimedOut => {
            let card = build_error_card(
                "Pixeldrain",
                &folder,
                "6-hour timeout",
                "Download was terminated after exceeding the 6-hour limit.",
                "timeout",
                elapsed,
            );
            app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
        }
        CmdOutcome::SpawnError(e) => {
            log_error(&format!("pixeldrain download failed: {e}"));
            let card = build_error_card(
                "Pixeldrain",
                &folder,
                "Failed to spawn curl",
                "Verify `curl` is installed on the host.",
                &truncate_str(&e, 240),
                elapsed,
            );
            app.tg.present(live_msg_id, &card, Some(kb_back_menu()));
        }
    }

    app.busy.lock().unwrap().remove(&folder);
}

// ── handlers ────────────────────────────────────────────────────────────────
fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let data = cb.get("data").and_then(Value::as_str).unwrap_or_default().trim();
    let chat_id = cb.pointer("/message/chat/id").and_then(Value::as_i64).unwrap_or(0);
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);

    if !app.tg.is_owner(chat_id) {
        app.tg.ack(cb_id, "Owner access only.");
        return;
    }

    if let Some(folder) = data.strip_prefix("pick:") {
        if app.cfg.target_path(folder).is_none() {
            app.tg.ack(cb_id, "Unknown folder");
            return;
        }
        {
            let mut st = app.state.lock().unwrap();
            st.folder = Some(folder.to_string());
            st.pending = None;
            app.persist_state(&st);
        }
        app.tg.ack(cb_id, &format!("Target: {}", target_label(folder)));
        app.tg.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
    } else if data.starts_with("go:") {
        if data == "go:no" {
            {
                let mut st = app.state.lock().unwrap();
                st.pending = None;
                app.persist_state(&st);
            }
            app.tg.ack(cb_id, "Cancelled");
            app.tg.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
            return;
        }
        let pending = {
            let mut st = app.state.lock().unwrap();
            let p = st.pending.take();
            app.persist_state(&st);
            p
        };
        let Some(p) = pending else {
            app.tg.ack(cb_id, "Nothing pending");
            app.tg.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
            return;
        };
        let not_ready = p.folder.is_empty()
            || (p.kind == "gdrive" && p.url.as_deref().unwrap_or("").is_empty())
            || (p.kind == "pixeldrain" && p.fid.as_deref().unwrap_or("").is_empty());
        if not_ready {
            app.tg.ack(cb_id, "Nothing pending");
            app.tg.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
            return;
        }
        if app.busy.lock().unwrap().contains(&p.folder) {
            app.tg.ack(cb_id, "Folder busy");
            let card = format!(
                "**⚠️ PX-Media · Folder Busy** · {}\n\n{} already has an active download. Wait for it to finish.\n\n_{}_",
                hl("Queued"),
                target_label(&p.folder),
                stamp_wib()
            );
            app.tg.present(msg_id, &card, Some(kb_back_menu()));
            return;
        }
        app.tg.ack(cb_id, "Starting…");
        let app_clone = Arc::clone(app);
        if p.kind == "pixeldrain" {
            let fid = p.fid.unwrap_or_default();
            thread::spawn(move || run_pixeldrain(app_clone, p.folder, fid, msg_id));
        } else {
            let url = p.url.unwrap_or_default();
            thread::spawn(move || run_download(app_clone, p.folder, url, msg_id));
        }
    } else if data == "act:recent" {
        app.tg.ack(cb_id, "Recent files");
        app.tg.present(msg_id, &app.recent_text(), Some(kb_recent()));
    } else if data == "act:links" {
        app.tg.ack(cb_id, "Access links");
        app.tg.present(msg_id, &app.links_text(), Some(kb_links()));
    } else if data == "act:scan" {
        app.tg.ack(cb_id, "Checking Jellyfin…");
        app.tg.present(msg_id, &app.jellyfin_scan(), Some(kb_after()));
    } else {
        app.tg.ack(cb_id, "Refreshed");
        app.tg.present(msg_id, &app.menu_text(), Some(app.kb_menu()));
    }
}

fn handle_message(app: &Arc<App>, msg: &Value) {
    let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64).unwrap_or(0);
    if !app.tg.is_owner(chat_id) {
        return;
    }
    let text = msg.get("text").and_then(Value::as_str).unwrap_or_default().trim();
    if text.is_empty() {
        return;
    }

    if text.starts_with('/') {
        let cmd = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
        let cmd = cmd.split('@').next().unwrap_or("");
        match cmd {
            "/scan" => {
                app.tg.send(&app.jellyfin_scan(), Some(kb_after()));
            }
            "/links" => {
                app.tg.send(&app.links_text(), Some(kb_links()));
            }
            "/recent" => {
                app.tg.send(&app.recent_text(), Some(kb_recent()));
            }
            _ => {
                app.tg.send(&app.menu_text(), Some(app.kb_menu()));
            }
        }
        return;
    }

    let fid = drive_id(text);
    let pd = pixeldrain_id(text);
    if fid.is_none() && pd.is_none() {
        let card = format!(
            "**⚠️ PX-Media · Unsupported Link** · {}\n\n{}\n\n_{}_",
            hl("Check Format"),
            table_markdown(
                &["Provider", "Supported Pattern"],
                &[
                    vec!["Google Drive".into(), "`https://drive.google.com/file/d/<id>/view`".into()],
                    vec!["Pixeldrain".into(), "`https://pixeldrain.com/u/<id>`".into()],
                ],
            ),
            stamp_wib()
        );
        app.tg.send(&card, Some(app.kb_menu()));
        return;
    }

    let folder = { app.state.lock().unwrap().folder.clone() };
    let Some(folder) = folder else {
        app.tg.send(&app.menu_text(), Some(app.kb_menu()));
        return;
    };

    if let Some((kind, pid)) = pd {
        if kind != "u" {
            app.tg.send(
                &format!(
                    "**ℹ️ PX-Media · Lists Not Supported Yet** · {}\n\nOnly single Pixeldrain files (`/u/<id>`) are supported right now.",
                    hl("Info")
                ),
                Some(app.kb_menu()),
            );
            return;
        }
        let info = app.pixeldrain_info(&pid);
        let preview_name = info
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&pid);
        let preview_size = info
            .get("size")
            .and_then(Value::as_u64)
            .filter(|&s| s > 0)
            .map(human_bytes)
            .unwrap_or_else(|| "?".to_string());

        {
            let mut st = app.state.lock().unwrap();
            st.pending = Some(Pending {
                folder: folder.clone(),
                kind: "pixeldrain".to_string(),
                url: None,
                fid: Some(pid.clone()),
            });
            app.persist_state(&st);
        }
        let card = format!(
            "**⬇️ PX-Media · Confirm Download** · {}\n\n{}\n\nStart downloading into this folder?\n_{}_",
            hl("Pixeldrain"),
            field_grid(&[
                ("Target", hl(target_label(&folder))),
                ("File", format!("`{}`", truncate_str(preview_name, 40))),
                ("Size", hl(&preview_size)),
                ("File ID", format!("`{pid}`")),
            ]),
            stamp_wib()
        );
        app.tg.send(&card, Some(kb_confirm(&folder)));
        return;
    }

    let fid = fid.unwrap();
    {
        let mut st = app.state.lock().unwrap();
        st.pending = Some(Pending {
            folder: folder.clone(),
            kind: "gdrive".to_string(),
            url: Some(text.to_string()),
            fid: None,
        });
        app.persist_state(&st);
    }
    let card = format!(
        "**⬇️ PX-Media · Confirm Download** · {}\n\n{}\n\nStart downloading into this folder?\n_{}_",
        hl("Google Drive"),
        field_grid(&[
            ("Target", hl(target_label(&folder))),
            ("Source", "Google Drive (`gdrive-dl`)".to_string()),
            ("File ID", format!("`{}`", truncate_str(&fid, 16))),
        ]),
        stamp_wib()
    );
    app.tg.send(&card, Some(kb_confirm(&folder)));
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-media {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                println!(
                    "OK: px-media {VERSION} | state={} | gdrive_dl={} | jellyfin={}",
                    cfg.state_file.display(),
                    cfg.gdrive_dl,
                    cfg.jellyfin_url
                );
                return;
            }
            Err(e) => {
                eprintln!("CONFIG ERROR: {e}");
                std::process::exit(1);
            }
        }
    }

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            log_error(&e);
            std::process::exit(1);
        }
    };

    let app = App::new(cfg);
    log_info(&format!("PX-Media v{VERSION} (Rust) started"));
    app.tg.set_my_commands(&[
        ("start", "Open dashboard"),
        ("recent", "Recent files in active folder"),
        ("links", "Media service access links"),
        ("scan", "Trigger Jellyfin library scan"),
        ("help", "Help & dashboard"),
    ]);
    app.tg.send(&app.menu_text(), Some(app.kb_menu()));

    loop {
        let offset = { app.state.lock().unwrap().offset };
        match app.tg.get_updates(offset, 25) {
            Ok(updates) => {
                for u in updates {
                    if let Some(uid) = u.get("update_id").and_then(Value::as_i64) {
                        let mut st = app.state.lock().unwrap();
                        st.offset = uid + 1;
                        app.persist_state(&st);
                    }
                    if let Some(cb) = u.get("callback_query") {
                        handle_callback(&app, cb);
                    } else if let Some(msg) = u.get("message") {
                        handle_message(&app, msg);
                    }
                }
            }
            Err(e) => {
                log_warn(&format!("polling error: {e}"));
                thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drive_and_pixeldrain_parsers() {
        assert_eq!(
            drive_id("https://drive.google.com/file/d/1AbCdEfGhIjKlMnOpQrStUvWx/view"),
            Some("1AbCdEfGhIjKlMnOpQrStUvWx".to_string())
        );
        assert_eq!(
            pixeldrain_id("https://pixeldrain.com/u/AbC123xy"),
            Some(("u".to_string(), "AbC123xy".to_string()))
        );
    }

    #[test]
    fn test_progress_line_eta() {
        let line = format_progress_line(50 * 1024 * 1024, Some(100 * 1024 * 1024), 10);
        assert!(line.contains("▓▓▓▓▓░░░░░"));
        assert!(line.contains("==50%=="));
        assert!(line.contains("ETA 00:10"));
    }

    #[test]
    fn test_looks_complete_guard() {
        let dir = env::temp_dir().join("px_media_test");
        let _ = fs::create_dir_all(&dir);
        let html = dir.join("bad.html");
        fs::write(&html, b"<!DOCTYPE html><html><head></head></html>").unwrap();
        assert!(!looks_complete(&html).0);
        let ok = dir.join("good.mkv");
        fs::write(&ok, b"Rar!\x1a\x07\x01\x00data").unwrap();
        assert!(looks_complete(&ok).0);
        let _ = fs::remove_dir_all(&dir);
    }
}
