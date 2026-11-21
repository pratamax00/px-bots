mod sys;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sys::{
    collect_service_snaps, create_backup, extract_router_list, fmt_mem_opt, format_uptime_compact,
    is_transitional, list_latest_backups, load_agent_sessions, load_backup_targets,
    load_ops_forum, load_services, load_thresholds, now_epoch, read_host_metrics, restore_backup,
    router_get, router_refresh_catalog, run_argv, run_shell, service_control, service_logs,
    set_agent_session_compress, set_agent_session_mode, set_agent_session_model, svec,
    trigger_compress_now, utc_compact_stamp, verify_backup, JobManager, ServiceSnap,
};
use tg_core::bot::TgClient;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, hl, human_bytes, md_text, progress_bar, rows, stamp_wib,
    table_markdown, tail_str, tg_button_row, tg_cb_button_styled, truncate_str,
};
use tg_core::{log_error, log_info, log_warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const CONFIRM_TTL_SECS: u64 = 180;
const SERVICES_PAGE_SIZE: usize = 8;
const SESSIONS_PAGE_SIZE: usize = 10;

// ── configuration (100% env-only) ───────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    config_dir: PathBuf,
    self_unit: String,
    state_file: PathBuf,
    jobs_dir: PathBuf,
    backup_dir: Option<PathBuf>,
    diagnostics_dir: Option<PathBuf>,
    router_base_url: String,
    router_api_key: String,
    router_data_dir: Option<PathBuf>,
    hermes_bin: Option<PathBuf>,
    hermes_profile_dir: Option<PathBuf>,
    hermes_profiles_root: Option<PathBuf>,
    hermes_catalog_profiles: Vec<String>,
}

fn opt_path_env(name: &str) -> Option<PathBuf> {
    env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
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
            .or_else(|_| env::var("OWNER_TELEGRAM_ID"))
            .or_else(|_| env::var("TELEGRAM_OWNER_ID"))
            .map_err(|_| "OWNER_ID environment variable is required".to_string())?;
        let owner_id = owner_raw
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("Invalid OWNER_ID: {e}"))?;

        let config_dir = opt_path_env("OPS_CONFIG_DIR")
            .ok_or_else(|| "OPS_CONFIG_DIR environment variable is required".to_string())?;
        let self_unit = env::var("SELF_UNIT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "px-ops.service".to_string());
        let state_file = opt_path_env("STATE_FILE")
            .unwrap_or_else(|| PathBuf::from("./state/px-ops.json"));
        let jobs_dir = opt_path_env("JOBS_DIR")
            .unwrap_or_else(|| PathBuf::from("./state/px-ops-jobs"));
        let backup_dir = opt_path_env("BACKUP_DIR");
        let diagnostics_dir = opt_path_env("DIAGNOSTICS_DIR");
        let router_base_url = env::var("NINE_ROUTER_BASE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:20128".to_string())
            .trim_end_matches('/')
            .to_string();
        let router_api_key = env::var("NINE_ROUTER_API_KEY").unwrap_or_default();
        let router_data_dir = opt_path_env("NINE_ROUTER_DATA_DIR");
        let hermes_bin = opt_path_env("HERMES_BIN");
        let hermes_profile_dir = opt_path_env("HERMES_PROFILE_DIR");
        let hermes_profiles_root = opt_path_env("HERMES_PROFILES_ROOT");
        let hermes_catalog_profiles = env::var("HERMES_CATALOG_PROFILES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            tg_token,
            owner_id,
            config_dir,
            self_unit,
            state_file,
            jobs_dir,
            backup_dir,
            diagnostics_dir,
            router_base_url,
            router_api_key,
            router_data_dir,
            hermes_bin,
            hermes_profile_dir,
            hermes_profiles_root,
            hermes_catalog_profiles,
        })
    }
}

// ── persistent state ────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    maintenance: bool,
    #[serde(default)]
    manual_off: HashSet<String>,
    #[serde(default)]
    cooldowns: HashMap<String, u64>,
    #[serde(default)]
    exec_cwd: String,
    #[serde(default)]
    session_keys: Vec<String>,
    #[serde(default)]
    model_choices: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct Surface {
    chat_id: i64,
    thread_id: Option<i64>,
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    ServiceOp { id: String, op: String },
    ServerUpdate,
    ServerReboot,
    ServerShutdownStep1,
    ServerShutdownStep2,
    BackupCreate { target_id: String },
    BackupAll,
    BackupRestore { target_id: String, file_name: String },
    SessionsBulkMode { mode: String },
    SessionsBulkModel { model: Option<String> },
    SessionsBulkCompress { pct: Option<u32> },
    SessionsCompressNow,
}

#[derive(Debug, Clone)]
struct PendingConfirm {
    action: ConfirmAction,
    expires_at: u64,
}

// ── app context ─────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    tg: Arc<TgClient>,
    jobs: Arc<JobManager>,
    state: Mutex<BotState>,
    confirms: Mutex<HashMap<String, PendingConfirm>>,
    confirm_seq: AtomicU64,
}

impl App {
    fn new(cfg: Config) -> Arc<Self> {
        let tg = TgClient::new(cfg.tg_token.clone(), cfg.owner_id);
        let jobs = JobManager::new(cfg.jobs_dir.clone());
        let initial: BotState = load_json(&cfg.state_file);
        Arc::new(Self {
            cfg,
            tg,
            jobs,
            state: Mutex::new(initial),
            confirms: Mutex::new(HashMap::new()),
            confirm_seq: AtomicU64::new(1),
        })
    }

    fn persist(&self, st: &BotState) {
        save_json_atomic(&self.cfg.state_file, st);
    }

    fn is_authorized(&self, chat_id: i64, from_id: i64) -> bool {
        if from_id != self.cfg.owner_id {
            return false;
        }
        if chat_id == self.cfg.owner_id {
            return true;
        }
        let forum = load_ops_forum(&self.cfg.config_dir);
        forum.parsed_chat_id() == Some(chat_id)
    }

    fn create_confirm(&self, action: ConfirmAction) -> String {
        let seq = self.confirm_seq.fetch_add(1, Ordering::Relaxed);
        let tok = format!("{:x}{:02x}", now_epoch() & 0xFFFFF, seq & 0xFF);
        let mut map = self.confirms.lock().unwrap();
        let now = now_epoch();
        map.retain(|_, v| v.expires_at > now);
        map.insert(
            tok.clone(),
            PendingConfirm {
                action,
                expires_at: now + CONFIRM_TTL_SECS,
            },
        );
        tok
    }

    fn take_confirm(&self, tok: &str) -> Option<ConfirmAction> {
        let mut map = self.confirms.lock().unwrap();
        let entry = map.remove(tok)?;
        if entry.expires_at >= now_epoch() {
            Some(entry.action)
        } else {
            None
        }
    }

    fn cancel_confirm(&self, tok: &str) {
        self.confirms.lock().unwrap().remove(tok);
    }

    fn deliver(
        &self,
        surf: Surface,
        msg_id: Option<i64>,
        title: &str,
        markdown: &str,
        kb: Value,
    ) -> Option<i64> {
        if markdown.len() <= 3900 {
            return self
                .tg
                .present_in(surf.chat_id, surf.thread_id, msg_id, markdown, Some(kb));
        }
        let fname = format!(
            "px-ops-{}-{}.txt",
            utc_compact_stamp(),
            title
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .take(24)
                .collect::<String>()
        );
        let note = format!(
            "**📄 {}**\n\n{}\n\nFull output attached below.\n_Updated: {}_",
            md_text(title),
            field_grid(&[
                ("File", format!("`{fname}` ({} chars)", markdown.len())),
                ("Status", hl("Attached")),
            ]),
            stamp_wib()
        );
        let sent = self
            .tg
            .present_in(surf.chat_id, surf.thread_id, msg_id, &note, Some(kb));
        if let Err(e) = self
            .tg
            .send_document_to(surf.chat_id, surf.thread_id, &fname, markdown.as_bytes(), Some(title))
        {
            log_warn(&format!("send_document_to failed: {e}"));
        }
        sent
    }

    fn notify_route(&self, kind: &str, markdown: &str, kb: Option<Value>) {
        let forum = load_ops_forum(&self.cfg.config_dir);
        let mode = if kind == "alerts" {
            forum.notify.alerts.to_ascii_lowercase()
        } else {
            forum.notify.jobs.to_ascii_lowercase()
        };
        let topic_key = if kind == "alerts" { "alerts" } else { "ops" };
        let topic_id = forum
            .topic(topic_key)
            .or_else(|| forum.topic("jobs"))
            .or_else(|| forum.topic("alerts"));

        let mut sent_any = false;
        if mode == "ops" || mode == "dual" {
            if let Some(ops_chat) = forum.parsed_chat_id() {
                if self
                    .tg
                    .send_to(ops_chat, topic_id, markdown, kb.clone())
                    .is_some()
                {
                    sent_any = true;
                }
            }
        }
        if mode == "dm" || mode == "dual" || !sent_any {
            let _ = self.tg.send(markdown, kb);
        }
    }

    // ── keyboards ───────────────────────────────────────────────────────────
    fn kb_main() -> Value {
        json!({
            "inline_keyboard": rows(vec![
                button("📊 Dashboard", "menu:dashboard", "primary"),
                button("⚡ Apps", "menu:apps", "success"),
                button("🧩 Services", "menu:services", "primary"),
                button("🖥 Server", "menu:server", "primary"),
                button("🌐 9Router", "menu:router", "primary"),
                button("🧠 Sessions", "menu:sessions", "primary"),
                button("🛠 Tools", "menu:tools", "primary"),
                button("💾 Backup", "menu:backup", "success"),
                button("📋 Jobs", "menu:jobs", "primary"),
                button("⚡ Power", "menu:power", "danger"),
                button("⚙️ Settings", "menu:settings", "primary"),
                button("❓ Help", "menu:help", ""),
            ])
        })
    }

    /// Slim dashboard keyboard containing only sections not already in the card pills.
    fn kb_dashboard() -> Value {
        json!({
            "inline_keyboard": [
                [
                    button("🖥 Server", "menu:server", "primary"),
                    button("🌐 9Router", "menu:router", "primary"),
                    button("🧠 Sessions", "menu:sessions", "primary"),
                ],
                [
                    button("🛠 Tools", "menu:tools", "primary"),
                    button("📋 Jobs", "menu:jobs", "primary"),
                    button("⚙️ Settings", "menu:settings", "primary"),
                ],
                [
                    button("⚡ Power", "menu:power", "danger"),
                    button("❓ Help", "menu:help", ""),
                ],
            ]
        })
    }

    fn kb_back_home(extra: Vec<Value>) -> Value {
        let mut all = extra;
        all.push(button("🏠 Home", "menu:dashboard", "primary"));
        json!({"inline_keyboard": rows(all)})
    }

    fn kb_confirm(tok: &str, label: &str, danger: bool) -> Value {
        json!({
            "inline_keyboard": [
                [
                    button(label, &format!("cfm:yes:{tok}"), if danger { "danger" } else { "success" }),
                    button("❌ Cancel", &format!("cfm:no:{tok}"), "danger"),
                ],
                [button("🏠 Home", "menu:dashboard", "primary")],
            ]
        })
    }

    // ── Dashboard ───────────────────────────────────────────────────────────
    fn render_dashboard(&self, surf: Surface, msg_id: Option<i64>) {
        let metrics = read_host_metrics();
        let services = load_services(&self.cfg.config_dir);
        let snaps = collect_service_snaps(&services);
        let maintenance = self.state.lock().unwrap().maintenance;

        let ram_pct = if metrics.ram_total > 0 {
            (metrics.ram_used as f64 / metrics.ram_total as f64) * 100.0
        } else {
            0.0
        };
        let swap_pct = if metrics.swap_total > 0 {
            (metrics.swap_used as f64 / metrics.swap_total as f64) * 100.0
        } else {
            0.0
        };

        let mut fields = vec![
            (
                "CPU",
                format!(
                    "`{}` {} (load {:.2})",
                    progress_bar(metrics.cpu_pct / 100.0, 8),
                    hl(&format!("{:.1}%", metrics.cpu_pct)),
                    metrics.load1
                ),
            ),
            (
                "RAM",
                format!(
                    "`{}` {} ({}/{})",
                    progress_bar(ram_pct / 100.0, 8),
                    hl(&format!("{ram_pct:.1}%")),
                    human_bytes(metrics.ram_used),
                    human_bytes(metrics.ram_total)
                ),
            ),
            (
                "Swap",
                format!(
                    "`{}` {:.0}% ({}/{})",
                    progress_bar(swap_pct / 100.0, 8),
                    swap_pct,
                    human_bytes(metrics.swap_used),
                    human_bytes(metrics.swap_total)
                ),
            ),
            (
                "Disk /",
                format!(
                    "`{}` {:.0}% ({}/{})",
                    progress_bar(metrics.disk_pct / 100.0, 8),
                    metrics.disk_pct,
                    human_bytes(metrics.disk_used),
                    human_bytes(metrics.disk_total)
                ),
            ),
        ];
        if let Some(t) = metrics.temp_c {
            fields.push(("Temp", hl(&format!("{t:.1}°C"))));
        }

        let core_snaps: Vec<&ServiceSnap> = snaps.iter().filter(|s| !s.def.is_app()).collect();
        let active_core = core_snaps.iter().filter(|s| s.tone() == "ok").count();
        let bad_core = core_snaps.iter().filter(|s| s.tone() == "bad").count();
        let active_apps = snaps
            .iter()
            .filter(|s| s.def.is_app() && s.tone() == "ok")
            .count();
        let total_apps = snaps.iter().filter(|s| s.def.is_app()).count();

        fields.push((
            "Services",
            format!(
                "{active_core}/{} core active{} · {active_apps}/{total_apps} apps on",
                core_snaps.len(),
                if bad_core > 0 {
                    format!(" · ⚠️ {bad_core} failed")
                } else {
                    String::new()
                }
            ),
        ));
        fields.push((
            "Maintenance",
            if maintenance {
                hl("⚠️ Active (alerts paused)")
            } else {
                "Off".to_string()
            },
        ));

        let svc_rows: Vec<Vec<String>> = core_snaps
            .iter()
            .take(14)
            .map(|s| {
                vec![
                    tg_cb_button_styled(&s.def.label, &format!("svc:open:{}", s.def.id), "link"),
                    s.status_badge(),
                    fmt_mem_opt(s.mem_bytes),
                    s.uptime_secs
                        .map(format_uptime_compact)
                        .unwrap_or_else(|| "—".to_string()),
                ]
            })
            .collect();
        let svc_md = if svc_rows.is_empty() {
            "_No services configured._".to_string()
        } else {
            table_markdown(&["Service", "State", "RAM", "Uptime"], &svc_rows)
        };

        let pills = tg_button_row(&[
            tg_cb_button_styled("🔄 Refresh", "menu:dashboard", "primary"),
            tg_cb_button_styled("⚡ Apps", "menu:apps", "success"),
            tg_cb_button_styled("🧩 Services", "menu:services", "primary"),
            tg_cb_button_styled("💾 Backup", "menu:backup", "success"),
        ]);

        let md = format!(
            "**🖥 PX-Ops** · `{}` · up {}\n\n{}\n\n{}\n\n{}\n\n_v{VERSION} · {}_",
            md_text(&metrics.hostname),
            format_uptime_compact(metrics.uptime_secs),
            field_grid(&fields),
            pills,
            details_block(
                &format!("Core Services ({active_core}/{})", core_snaps.len()),
                &svc_md
            ),
            stamp_wib()
        );
        self.deliver(surf, msg_id, "dashboard", &md, Self::kb_dashboard());
    }

    // ── Apps Toggle Panel ───────────────────────────────────────────────────
    fn render_apps(&self, surf: Surface, msg_id: Option<i64>) {
        let services = load_services(&self.cfg.config_dir);
        let snaps = collect_service_snaps(&services);
        let apps: Vec<&ServiceSnap> = snaps.iter().filter(|s| s.def.is_app()).collect();

        let trows: Vec<Vec<String>> = apps
            .iter()
            .map(|s| {
                let on = s.tone() == "ok";
                let act = tg_cb_button_styled(
                    if on { "⏹ Stop" } else { "▶ Start" },
                    &format!("app:toggle:{}", s.def.id),
                    if on { "danger" } else { "success" },
                );
                vec![
                    s.def.label.clone(),
                    s.status_badge(),
                    fmt_mem_opt(s.mem_bytes),
                    act,
                ]
            })
            .collect();
        let tbl = if trows.is_empty() {
            "_No optional apps configured (`group: \"apps\"`)._".to_string()
        } else {
            table_markdown(&["App", "State", "RAM", "Action"], &trows)
        };

        let kb = json!({
            "inline_keyboard": [[
                button("🔄 Refresh", "menu:apps", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });

        let md = format!(
            "**⚡ Optional Heavy Apps (RAM Saver)**\n\n{}\n\nTap Start/Stop in the table to toggle immediately.\n_Updated: {}_",
            tbl,
            stamp_wib()
        );
        self.deliver(surf, msg_id, "apps", &md, kb);
    }

    fn exec_app_toggle(&self, surf: Surface, msg_id: Option<i64>, id: &str) {
        let services = load_services(&self.cfg.config_dir);
        let Some(svc) = services.into_iter().find(|s| s.id == id) else {
            self.render_apps(surf, msg_id);
            return;
        };
        let snap = collect_service_snaps(std::slice::from_ref(&svc)).into_iter().next();
        let is_on = snap.map(|s| s.tone() == "ok").unwrap_or(false);
        let op = if is_on { "stop" } else { "start" };
        let t0 = Instant::now();
        let res = service_control(&svc, op, &self.cfg.self_unit);
        {
            let mut st = self.state.lock().unwrap();
            if op == "stop" {
                st.manual_off.insert(svc.id.clone());
            } else {
                st.manual_off.remove(&svc.id);
            }
            self.persist(&st);
        }
        self.jobs.append_audit(
            &format!("app.{op}"),
            &svc.id,
            res.exit_code,
            t0.elapsed().as_millis() as u64,
            &svc.label,
        );
        self.render_apps(surf, msg_id);
    }

    // ── Services Panel ──────────────────────────────────────────────────────
    fn render_services(&self, surf: Surface, msg_id: Option<i64>, page: usize) {
        let services = load_services(&self.cfg.config_dir);
        let snaps = collect_service_snaps(&services);
        let total_pages = snaps.len().div_ceil(SERVICES_PAGE_SIZE).max(1);
        let page = page.min(total_pages - 1);
        let start = page * SERVICES_PAGE_SIZE;
        let slice = &snaps[start..(start + SERVICES_PAGE_SIZE).min(snaps.len())];

        let active_n = snaps.iter().filter(|s| s.tone() == "ok").count();
        let bad_n = snaps.iter().filter(|s| s.tone() == "bad").count();
        let trows: Vec<Vec<String>> = slice
            .iter()
            .map(|s| {
                vec![
                    tg_cb_button_styled(&s.def.label, &format!("svc:open:{}", s.def.id), "link"),
                    s.status_badge(),
                    fmt_mem_opt(s.mem_bytes),
                    s.uptime_secs
                        .map(format_uptime_compact)
                        .unwrap_or_else(|| "—".to_string()),
                ]
            })
            .collect();

        let mut kb_rows: Vec<Value> = Vec::new();
        let mut pager = Vec::new();
        if page > 0 {
            pager.push(button("◀ Prev", &format!("menu:services:{}", page - 1), "primary"));
        }
        if page + 1 < total_pages {
            pager.push(button("Next ▶", &format!("menu:services:{}", page + 1), "primary"));
        }
        if !pager.is_empty() {
            kb_rows.push(Value::Array(pager));
        }
        kb_rows.push(json!([
            button("🔄 Refresh", &format!("menu:services:{page}"), "primary"),
            button("🏠 Home", "menu:dashboard", "primary"),
        ]));

        let md = format!(
            "**🧩 Services** · Page {}/{}\n\n{}\n\n{}\n\nTap any service to inspect or control.\n_Updated: {}_",
            page + 1,
            total_pages,
            field_grid(&[
                ("Total", snaps.len().to_string()),
                ("Active", hl(&active_n.to_string())),
                ("Failed", if bad_n > 0 { hl(&format!("⚠️ {bad_n}")) } else { "0".into() }),
            ]),
            table_markdown(&["Service", "State", "RAM", "Uptime"], &trows),
            stamp_wib()
        );
        self.deliver(surf, msg_id, "services", &md, json!({"inline_keyboard": kb_rows}));
    }

    fn render_service_detail(&self, surf: Surface, msg_id: Option<i64>, id: &str) {
        let services = load_services(&self.cfg.config_dir);
        let Some(svc) = services.into_iter().find(|s| s.id == id) else {
            self.render_services(surf, msg_id, 0);
            return;
        };
        let snap = collect_service_snaps(std::slice::from_ref(&svc))
            .into_iter()
            .next()
            .unwrap();
        let is_manual_off = self.state.lock().unwrap().manual_off.contains(&svc.id);
        let is_active = snap.tone() == "ok";
        let mut action_pills = Vec::new();
        if is_active {
            action_pills.push(tg_cb_button_styled("🔁 Restart", &format!("svc:restart:{}", svc.id), "primary"));
            action_pills.push(tg_cb_button_styled("⏹ Stop", &format!("svc:stop:{}", svc.id), "danger"));
        } else {
            action_pills.push(tg_cb_button_styled("▶ Start", &format!("svc:start:{}", svc.id), "success"));
        }
        action_pills.push(tg_cb_button_styled("📜 Logs", &format!("svc:logs:{}", svc.id), "link"));

        let md = format!(
            "**🧩 Service · {}**\n\n{}\n\n{}\n\n_Updated: {}_",
            md_text(&svc.label),
            field_grid(&[
                ("ID", format!("`{}`", svc.id)),
                ("Unit", format!("`{}` ({})", svc.unit, svc.manager)),
                ("State", format!("{} ({})", hl(&snap.status_badge()), snap.sub)),
                ("PID / Restarts", format!("{} / {}", snap.pid, snap.restarts)),
                ("Memory", fmt_mem_opt(snap.mem_bytes)),
                (
                    "Uptime",
                    snap.uptime_secs
                        .map(format_uptime_compact)
                        .unwrap_or_else(|| "—".to_string())
                ),
                (
                    "Alerting",
                    if svc.is_app() || svc.is_soft() {
                        "Optional (no alert)".into()
                    } else if is_manual_off {
                        "Muted (manual stop)".into()
                    } else {
                        "Monitored".into()
                    }
                ),
            ]),
            tg_button_row(&action_pills),
            stamp_wib()
        );
        let kb = json!({
            "inline_keyboard": [[
                button("🔄 Refresh", &format!("svc:open:{}", svc.id), "primary"),
                button("↩ Services", "menu:services", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, &format!("svc-{}", svc.id), &md, kb);
    }

    fn exec_service_logs(&self, surf: Surface, msg_id: Option<i64>, id: &str) {
        let services = load_services(&self.cfg.config_dir);
        let Some(svc) = services.into_iter().find(|s| s.id == id) else {
            return;
        };
        let res = service_logs(&svc, 45);
        let tail = tail_str(&res.output, 2800);
        let md = format!(
            "**📜 Logs · {}**\n\n```\n{}\n```\n_Updated: {}_",
            md_text(&svc.label),
            tail.replace("```", "'''"),
            stamp_wib()
        );
        let kb = Self::kb_back_home(vec![
            button("🔄 Refresh Logs", &format!("svc:logs:{}", svc.id), "primary"),
            button("↩ Service", &format!("svc:open:{}", svc.id), "primary"),
        ]);
        self.deliver(surf, msg_id, &format!("logs-{}", svc.id), &md, kb);
    }

    fn exec_service_op(&self, surf: Surface, msg_id: Option<i64>, id: &str, op: &str) {
        let services = load_services(&self.cfg.config_dir);
        let Some(svc) = services.into_iter().find(|s| s.id == id) else {
            return;
        };
        let t0 = Instant::now();
        let res = service_control(&svc, op, &self.cfg.self_unit);
        {
            let mut st = self.state.lock().unwrap();
            if op == "stop" {
                st.manual_off.insert(svc.id.clone());
            } else if op == "start" || op == "restart" {
                st.manual_off.remove(&svc.id);
            }
            self.persist(&st);
        }
        self.jobs.append_audit(
            &format!("svc.{op}"),
            &svc.id,
            res.exit_code,
            t0.elapsed().as_millis() as u64,
            &svc.label,
        );
        if op == "restart" && svc.unit == self.cfg.self_unit {
            let md = format!(
                "**🔁 Self-Restart Scheduled**\n\n{}\n\nWait ~3 seconds, then tap Refresh.",
                field_grid(&[
                    ("Unit", format!("`{}`", svc.unit)),
                    ("Status", hl("⏳ Restarting in 2s")),
                ])
            );
            self.tg
                .present_in(surf.chat_id, surf.thread_id, msg_id, &md, Some(Self::kb_main()));
            return;
        }
        self.render_service_detail(surf, msg_id, id);
    }

    // ── Server Panel ────────────────────────────────────────────────────────
    fn render_server_menu(&self, surf: Surface, msg_id: Option<i64>) {
        let row1 = tg_button_row(&[
            tg_cb_button_styled("ℹ️ Info", "srv:info", "primary"),
            tg_cb_button_styled("💽 Storage", "srv:storage", "primary"),
            tg_cb_button_styled("🌐 Network", "srv:network", "primary"),
            tg_cb_button_styled("📈 Processes", "srv:processes", "primary"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled("📦 Check Updates", "srv:updates", "primary"),
            tg_cb_button_styled("🩺 SMART Disk", "srv:smart", "primary"),
            tg_cb_button_styled("⬆ Apply Updates", "srv:apply_update", "danger"),
        ]);
        let md = format!(
            "**🖥 Host Server Operations**\n\n{}\n\n{}\n\n{}",
            field_grid(&[
                ("Read-Only", "Info · Storage · Network · Processes · Updates · SMART".into()),
                ("Mutation", "Apply Updates (confirmed background job)".into()),
            ]),
            row1,
            row2,
        );
        self.deliver(surf, msg_id, "server", &md, Self::kb_back_home(vec![]));
    }

    fn exec_server_cmd(&self, surf: Surface, msg_id: Option<i64>, sub: &str) {
        let kb = Self::kb_back_home(vec![
            button("🔄 Refresh", &format!("srv:{sub}"), "primary"),
            button("↩ Server", "menu:server", "primary"),
        ]);
        let t0 = Instant::now();
        let md = match sub {
            "info" => {
                let m = read_host_metrics();
                let r = run_shell(
                    "source /etc/os-release 2>/dev/null; printf '%s\\n' \"$PRETTY_NAME\"; uname -rm; timedatectl show -p Timezone --value 2>/dev/null; head -c 12 /proc/sys/kernel/random/boot_id",
                    None,
                    6,
                );
                let l: Vec<&str> = r.output.lines().collect();
                format!(
                    "**ℹ️ Server · System Info**\n\n{}\n\n_Updated: {}_",
                    field_grid(&[
                        ("Hostname", format!("`{}`", md_text(&m.hostname))),
                        ("Uptime", format_uptime_compact(m.uptime_secs)),
                        ("OS", l.first().copied().unwrap_or("—").to_string()),
                        ("Kernel", l.get(1).copied().unwrap_or("—").to_string()),
                        ("Timezone", l.get(2).copied().unwrap_or("—").to_string()),
                        ("CPU Cores", m.cpu_cores.to_string()),
                        ("Boot ID", format!("`{}…`", l.get(3).copied().unwrap_or("—"))),
                    ]),
                    stamp_wib()
                )
            }
            "storage" => {
                let r = run_shell(
                    "df -hT --type ext4 --type btrfs --type xfs --type zfs 2>/dev/null; echo '---INODE---'; df -hi --type ext4 --type btrfs --type xfs 2>/dev/null",
                    None,
                    8,
                );
                let (d_part, i_part) = r.output.split_once("---INODE---").unwrap_or((&r.output, ""));
                let drows: Vec<Vec<String>> = d_part
                    .lines()
                    .skip(1)
                    .filter_map(|l| {
                        let p: Vec<&str> = l.split_whitespace().collect();
                        if p.len() >= 7 {
                            Some(vec![p[6].into(), p[2].into(), p[3].into(), p[5].into()])
                        } else {
                            None
                        }
                    })
                    .collect();
                let irows: Vec<Vec<String>> = i_part
                    .lines()
                    .skip(1)
                    .filter_map(|l| {
                        let p: Vec<&str> = l.split_whitespace().collect();
                        if p.len() >= 6 {
                            Some(vec![p[5].into(), p[4].into(), truncate_str(p[0], 24)])
                        } else {
                            None
                        }
                    })
                    .collect();
                format!(
                    "**💽 Server · Storage**\n\n{}\n\n{}\n\n_Updated: {}_",
                    table_markdown(&["Mount", "Size", "Used", "Use%"], &drows),
                    details_block("Inode Usage", &table_markdown(&["Mount", "Inode%", "Device"], &irows)),
                    stamp_wib()
                )
            }
            "network" => {
                let r = run_shell(
                    "ip -brief address; echo '---ROUTE---'; ip route show default; echo '---PORTS---'; ss -lntupH 2>/dev/null | head -25",
                    None,
                    8,
                );
                let mut if_rows = Vec::new();
                let mut route_lines = Vec::new();
                let mut port_rows = Vec::new();
                let mut sec = 0;
                for line in r.output.lines() {
                    if line.trim() == "---ROUTE---" {
                        sec = 1;
                        continue;
                    }
                    if line.trim() == "---PORTS---" {
                        sec = 2;
                        continue;
                    }
                    if sec == 0 {
                        let p: Vec<&str> = line.split_whitespace().collect();
                        if p.len() >= 2 {
                            let addrs: Vec<&str> = p[2..]
                                .iter()
                                .copied()
                                .filter(|a| !a.starts_with("fe80::"))
                                .take(2)
                                .collect();
                            if p[0] == "lo" || !addrs.is_empty() {
                                if_rows.push(vec![
                                    p[0].into(),
                                    p[1].to_ascii_lowercase(),
                                    if addrs.is_empty() { "—".into() } else { addrs.join(" ") },
                                ]);
                            }
                        }
                    } else if sec == 1 && !line.trim().is_empty() {
                        route_lines.push(line.trim().to_string());
                    } else if sec == 2 {
                        let p: Vec<&str> = line.split_whitespace().collect();
                        if p.len() >= 5 {
                            port_rows.push(vec![
                                p[0].to_ascii_lowercase(),
                                truncate_str(p[4], 26),
                                truncate_str(p.get(6).copied().unwrap_or("—"), 22),
                            ]);
                        }
                    }
                }
                format!(
                    "**🌐 Server · Network**\n\n{}\n\n**Default Routes**\n```\n{}\n```\n\n{}\n\n_Updated: {}_",
                    table_markdown(&["Iface", "State", "Address"], &if_rows),
                    if route_lines.is_empty() { "none".into() } else { route_lines.join("\n") },
                    details_block(
                        &format!("Listening Ports ({})", port_rows.len()),
                        &table_markdown(&["Proto", "Local", "Process"], &port_rows)
                    ),
                    stamp_wib()
                )
            }
            "processes" => {
                let r = run_shell("ps -eo pid,%cpu,%mem,comm --sort=-%cpu | head -16", None, 6);
                let prows: Vec<Vec<String>> = r
                    .output
                    .lines()
                    .skip(1)
                    .filter_map(|l| {
                        let p: Vec<&str> = l.split_whitespace().collect();
                        if p.len() >= 4 {
                            Some(vec![p[0].into(), p[1].into(), p[2].into(), truncate_str(p[3], 20)])
                        } else {
                            None
                        }
                    })
                    .collect();
                format!(
                    "**📈 Server · Top Processes (by CPU)**\n\n{}\n\n_Updated: {}_",
                    table_markdown(&["PID", "%CPU", "%MEM", "Command"], &prows),
                    stamp_wib()
                )
            }
            "updates" => {
                let r = run_shell("apt list --upgradable 2>/dev/null", None, 15);
                let pkgs: Vec<Vec<String>> = r
                    .output
                    .lines()
                    .filter(|l| l.contains('/'))
                    .map(|l| {
                        let (name, rest) = l.split_once('/').unwrap_or((l, ""));
                        let ver = rest.split_whitespace().nth(1).unwrap_or("—");
                        vec![truncate_str(name, 26), truncate_str(ver, 22)]
                    })
                    .collect();
                if pkgs.is_empty() {
                    format!(
                        "**📦 Server · Package Updates**\n\n{}\n\n_Updated: {}_",
                        field_grid(&[("Status", hl("✅ All packages up to date"))]),
                        stamp_wib()
                    )
                } else {
                    let shown = pkgs.iter().take(25).cloned().collect::<Vec<_>>();
                    format!(
                        "**📦 Server · Package Updates**\n\n{}\n\n{}\n\n_Use **Apply Updates** to run `apt-get upgrade -y`._",
                        field_grid(&[("Upgradable", hl(&format!("{} packages", pkgs.len())))]),
                        table_markdown(&["Package", "Candidate"], &shown)
                    )
                }
            }
            "smart" => {
                let r = run_shell(
                    r#"for d in /dev/sd? /dev/nvme?n?; do [ -b "$d" ] || continue; echo "=== $d ==="; sudo -n smartctl -H "$d" 2>/dev/null | grep -i "overall-health"; sudo -n smartctl -A "$d" 2>/dev/null | awk '$2=="Reallocated_Sector_Ct" || $2=="Current_Pending_Sector" || $2=="Offline_Uncorrectable" || $2=="Temperature_Celsius" {print $2": "$10}'; done"#,
                    None,
                    30,
                );
                let body = if r.output.trim().is_empty() {
                    "(no SMART disks detected or smartctl unavailable)".to_string()
                } else {
                    truncate_str(&r.output, 2600)
                };
                format!(
                    "**🩺 Server · SMART Disk Health**\n\n```\n{}\n```\n_Updated: {}_",
                    body.replace("```", "'''"),
                    stamp_wib()
                )
            }
            _ => "**🖥 Server**\n\nUnknown action.".to_string(),
        };
        self.jobs.append_audit(
            &format!("server.{sub}"),
            "",
            0,
            t0.elapsed().as_millis() as u64,
            sub,
        );
        self.deliver(surf, msg_id, &format!("server-{sub}"), &md, kb);
    }

    // ── 9Router Panel ───────────────────────────────────────────────────────
    fn render_router_menu(&self, surf: Surface, msg_id: Option<i64>) {
        let row1 = tg_button_row(&[
            tg_cb_button_styled("💚 Quick Health", "rtr:health", "success"),
            tg_cb_button_styled("🩺 Full Health", "rtr:health_full", "primary"),
            tg_cb_button_styled("🔄 Refresh Catalog", "rtr:refresh", "success"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled("🧩 Providers", "rtr:list:providers", "primary"),
            tg_cb_button_styled("📦 Models", "rtr:list:models", "primary"),
            tg_cb_button_styled("🔌 Nodes", "rtr:list:nodes", "primary"),
            tg_cb_button_styled("🔑 Keys", "rtr:list:keys", "link"),
            tg_cb_button_styled("🔀 Combos", "rtr:list:combos", "link"),
        ]);
        let md = format!(
            "**🌐 9Router Control**\n\n{}\n\n{}\n\n{}",
            field_grid(&[
                ("Endpoint", format!("`{}`", md_text(&self.cfg.router_base_url))),
                (
                    "CLI Auth",
                    if self.cfg.router_data_dir.is_some() {
                        "Configured".into()
                    } else {
                        "Not configured".into()
                    }
                ),
            ]),
            row1,
            row2,
        );
        self.deliver(surf, msg_id, "router", &md, Self::kb_back_home(vec![]));
    }

    fn exec_router(&self, surf: Surface, msg_id: Option<i64>, sub: &str) {
        let t0 = Instant::now();
        let dd = self.cfg.router_data_dir.as_deref();
        let kb = Self::kb_back_home(vec![button("↩ 9Router", "menu:router", "primary")]);

        let md = match sub {
            "health" => match router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/api/health", 5) {
                Ok(v) => {
                    let st = v.get("status").and_then(Value::as_str).unwrap_or("ok");
                    format!(
                        "**💚 9Router · Quick Health**\n\n{}",
                        field_grid(&[
                            ("Status", hl(&format!("✅ {st}"))),
                            ("Latency", format!("{} ms", t0.elapsed().as_millis())),
                        ])
                    )
                }
                Err(e) => format!(
                    "**❌ 9Router · Health Failed**\n\n{}",
                    field_grid(&[("Error", md_text(&e))])
                ),
            },
            "health_full" => {
                let h = router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/api/health", 6);
                let nodes = router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/api/provider-nodes", 10)
                    .map(|v| extract_router_list(&v).len().to_string())
                    .unwrap_or_else(|_| "—".into());
                let conns = router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/api/providers", 10)
                    .map(|v| extract_router_list(&v).len().to_string())
                    .unwrap_or_else(|_| "—".into());
                let models = router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/v1/models", 10)
                    .map(|v| extract_router_list(&v).len().to_string())
                    .unwrap_or_else(|_| "—".into());
                let status = match h {
                    Ok(v) => hl(&format!("✅ {}", v.get("status").and_then(Value::as_str).unwrap_or("ok"))),
                    Err(e) => hl(&format!("❌ {}", truncate_str(&e, 40))),
                };
                format!(
                    "**🩺 9Router · Full Health**\n\n{}",
                    field_grid(&[
                        ("Status", status),
                        ("Nodes", nodes),
                        ("Connections", conns),
                        ("Models", models),
                        ("Latency", format!("{} ms", t0.elapsed().as_millis())),
                    ])
                )
            }
            "refresh" => {
                let Some(ref root) = self.cfg.hermes_profiles_root else {
                    self.deliver(
                        surf,
                        msg_id,
                        "router-refresh",
                        "**⚠️ Catalog Sync Not Configured**\n\nSet `HERMES_PROFILES_ROOT` and `HERMES_CATALOG_PROFILES` in the service vault.",
                        kb,
                    );
                    return;
                };
                match router_refresh_catalog(
                    &self.cfg.router_base_url,
                    &self.cfg.router_api_key,
                    dd,
                    root,
                    &self.cfg.hermes_catalog_profiles,
                ) {
                    Ok((provs, mods, conns, nodes)) => format!(
                        "**🔄 9Router · Catalog Synced**\n\n{}",
                        field_grid(&[
                            ("Status", hl("✅ Catalog written")),
                            ("Providers", provs.to_string()),
                            ("Models", mods.to_string()),
                            ("Connections", conns.to_string()),
                            ("Nodes", nodes.to_string()),
                            ("Profiles", self.cfg.hermes_catalog_profiles.len().to_string()),
                        ])
                    ),
                    Err(e) => format!(
                        "**❌ Catalog Sync Failed**\n\n{}",
                        field_grid(&[("Error", md_text(&e))])
                    ),
                }
            }
            rest if rest.starts_with("list:") => {
                let res_name = &rest[5..];
                let ep = match res_name {
                    "providers" => "/api/providers",
                    "nodes" => "/api/provider-nodes",
                    "models" => "/v1/models",
                    "keys" => "/api/keys",
                    "combos" => "/api/combos",
                    _ => "",
                };
                if ep.is_empty() {
                    "**❌ Unknown resource**".to_string()
                } else {
                    match router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, ep, 12) {
                        Ok(body) => {
                            let items = extract_router_list(&body);
                            let trows: Vec<Vec<String>> = items
                                .iter()
                                .take(20)
                                .map(|it| {
                                    let id = it
                                        .get("id")
                                        .or_else(|| it.get("name"))
                                        .or_else(|| it.get("provider"))
                                        .or_else(|| it.get("model"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("?");
                                    let info = it
                                        .get("provider")
                                        .or_else(|| it.get("owned_by"))
                                        .or_else(|| it.get("type"))
                                        .or_else(|| it.get("status"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("—");
                                    vec![truncate_str(id, 34), truncate_str(info, 20)]
                                })
                                .collect();
                            let more = if items.len() > 20 {
                                format!("\n\n_… +{} more_", items.len() - 20)
                            } else {
                                String::new()
                            };
                            format!(
                                "**🌐 9Router · {} ({})**\n\n{}{more}",
                                res_name,
                                items.len(),
                                table_markdown(&["ID", "Info"], &trows)
                            )
                        }
                        Err(e) => format!(
                            "**❌ 9Router · {res_name}**\n\n{}",
                            field_grid(&[("Error", md_text(&e))])
                        ),
                    }
                }
            }
            _ => "**🌐 9Router**\n\nUnknown action.".to_string(),
        };
        self.jobs
            .append_audit(&format!("router.{sub}"), "", 0, t0.elapsed().as_millis() as u64, sub);
        self.deliver(surf, msg_id, &format!("router-{sub}"), &md, kb);
    }

    // ── Sessions Panel ──────────────────────────────────────────────────────
    fn render_sessions(&self, surf: Surface, msg_id: Option<i64>, page: usize) {
        let Some(ref pdir) = self.cfg.hermes_profile_dir else {
            self.deliver(
                surf,
                msg_id,
                "sessions",
                "**🧠 Sessions Not Configured**\n\nSet `HERMES_PROFILE_DIR` in the service vault.",
                Self::kb_back_home(vec![]),
            );
            return;
        };
        let list = load_agent_sessions(pdir, &self.cfg.config_dir);
        {
            let mut st = self.state.lock().unwrap();
            st.session_keys = list.iter().map(|s| s.key.clone()).collect();
            self.persist(&st);
        }
        let total = list.len();
        let total_pages = total.div_ceil(SESSIONS_PAGE_SIZE).max(1);
        let p = page.min(total_pages - 1);
        let start = p * SESSIONS_PAGE_SIZE;
        let end = (start + SESSIONS_PAGE_SIZE).min(total);

        let fp = list.iter().filter(|s| s.mode == "fullpower").count();
        let he = list.iter().filter(|s| s.mode == "hemat").count();
        let ov_mod = list.iter().filter(|s| s.model != "default").count();
        let ov_comp = list.iter().filter(|s| s.compress_pct.is_some()).count();

        let trows: Vec<Vec<String>> = (start..end)
            .map(|idx| {
                let s = &list[idx];
                let comp = s
                    .compress_pct
                    .map(|v| format!("{}%", (v * 100.0).round() as i32))
                    .unwrap_or_else(|| "auto".to_string());
                vec![
                    tg_cb_button_styled(&truncate_str(&s.label, 24), &format!("ses:open:{idx}"), "link"),
                    s.mode.clone(),
                    truncate_str(&s.model, 18),
                    comp,
                ]
            })
            .collect();

        let bulk_row1 = tg_button_row(&[
            tg_cb_button_styled("⚡ Bulk Mode", "ses:bmode", "primary"),
            tg_cb_button_styled("🤖 Bulk Model", "ses:bmodel", "primary"),
            tg_cb_button_styled("🗜 Bulk Compress", "ses:bcomp", "primary"),
        ]);
        let bulk_row2 = tg_button_row(&[
            tg_cb_button_styled("🔥 Compress Now", "ses:compnow", "danger"),
        ]);

        let mut kb_rows: Vec<Value> = Vec::new();
        let mut pager = Vec::new();
        if p > 0 {
            pager.push(button("◀ Prev", &format!("ses:page:{}", p - 1), "primary"));
        }
        if p + 1 < total_pages {
            pager.push(button("Next ▶", &format!("ses:page:{}", p + 1), "primary"));
        }
        if !pager.is_empty() {
            kb_rows.push(Value::Array(pager));
        }
        kb_rows.push(json!([
            button("🔄 Refresh", &format!("ses:page:{p}"), "primary"),
            button("🏠 Home", "menu:dashboard", "primary"),
        ]));

        let md = format!(
            "**🧠 Agent Sessions** · Page {}/{total_pages}\n\n{}\n\n{}\n\n{}\n\n{}\n\nTap any session in the table for individual Mode / Model / Compress.\n_Updated: {}_",
            p + 1,
            field_grid(&[
                ("Total Sessions", total.to_string()),
                ("Fullpower / Hemat", format!("{fp} / {he}")),
                ("Overrides", format!("{ov_mod} model · {ov_comp} compress")),
            ]),
            if trows.is_empty() {
                "_No Telegram sessions found._".to_string()
            } else {
                table_markdown(&["Session", "Mode", "Model", "Comp"], &trows)
            },
            bulk_row1,
            bulk_row2,
            stamp_wib()
        );
        self.deliver(surf, msg_id, "sessions", &md, json!({"inline_keyboard": kb_rows}));
    }

    fn render_sessions_bulk_mode(&self, surf: Surface, msg_id: Option<i64>) {
        let pills = tg_button_row(&[
            tg_cb_button_styled("😌 All Santai", "ses:bulk:santai", "primary"),
            tg_cb_button_styled("🌿 All Hemat", "ses:bulk:hemat", "success"),
            tg_cb_button_styled("🔥 All Fullpower", "ses:bulk:fullpower", "danger"),
        ]);
        let md = format!(
            "**⚡ Bulk Session Mode**\n\nApply a mode preset to **all** tracked Telegram sessions.\n\n{pills}"
        );
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Sessions", "menu:sessions", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, "sessions-bulk-mode", &md, kb);
    }

    fn render_sessions_bulk_model(&self, surf: Surface, msg_id: Option<i64>) {
        let dd = self.cfg.router_data_dir.as_deref();
        let models: Vec<String> = match router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/v1/models", 10) {
            Ok(v) => extract_router_list(&v)
                .into_iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(ToString::to_string))
                .take(14)
                .collect(),
            Err(_) => Vec::new(),
        };
        {
            let mut st = self.state.lock().unwrap();
            st.model_choices = models.clone();
            self.persist(&st);
        }
        let list_md = if models.is_empty() {
            "_No models returned by 9Router `/v1/models`._".to_string()
        } else {
            let mrows: Vec<Vec<String>> = models
                .iter()
                .enumerate()
                .map(|(mi, m)| {
                    vec![
                        format!("#{}", mi + 1),
                        tg_cb_button_styled(&truncate_str(m, 34), &format!("ses:bmodelset:{mi}"), "link"),
                    ]
                })
                .collect();
            table_markdown(&["#", "Model (Apply to All)"], &mrows)
        };
        let clear_pill = tg_button_row(&[tg_cb_button_styled(
            "🧹 Clear Model Override (All)",
            "ses:bmodelclr",
            "danger",
        )]);
        let md = format!("**🤖 Bulk Model Override**\n\n{list_md}\n\n{clear_pill}");
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Sessions", "menu:sessions", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, "sessions-bulk-model", &md, kb);
    }

    fn render_sessions_bulk_compress(&self, surf: Surface, msg_id: Option<i64>) {
        let pills = tg_button_row(&[
            tg_cb_button_styled("🗜 10%", "ses:bcompset:10", "primary"),
            tg_cb_button_styled("🗜 25%", "ses:bcompset:25", "primary"),
            tg_cb_button_styled("🗜 50%", "ses:bcompset:50", "primary"),
            tg_cb_button_styled("🧹 Reset All", "ses:bcompset:0", "danger"),
        ]);
        let md = format!(
            "**🗜 Bulk Session Compression**\n\nSet context compression threshold for **all** tracked Telegram sessions.\n\n{pills}"
        );
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Sessions", "menu:sessions", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, "sessions-bulk-compress", &md, kb);
    }

    fn render_session_detail(&self, surf: Surface, msg_id: Option<i64>, idx: usize) {
        let Some(ref pdir) = self.cfg.hermes_profile_dir else {
            return;
        };
        let key_opt = self.state.lock().unwrap().session_keys.get(idx).cloned();
        let Some(key) = key_opt else {
            self.render_sessions(surf, msg_id, 0);
            return;
        };
        let list = load_agent_sessions(pdir, &self.cfg.config_dir);
        let Some(item) = list.into_iter().find(|s| s.key == key) else {
            self.render_sessions(surf, msg_id, 0);
            return;
        };
        let comp_str = item
            .compress_pct
            .map(|v| format!("{}% (override)", (v * 100.0).round() as i32))
            .unwrap_or_else(|| "preset".to_string());
        let mode_pills = tg_button_row(&[
            tg_cb_button_styled("😌 Santai", &format!("ses:mode:{idx}:santai"), if item.mode == "santai" { "primary" } else { "" }),
            tg_cb_button_styled("🌿 Hemat", &format!("ses:mode:{idx}:hemat"), if item.mode == "hemat" { "success" } else { "" }),
            tg_cb_button_styled("🔥 Fullpower", &format!("ses:mode:{idx}:fullpower"), if item.mode == "fullpower" { "danger" } else { "" }),
        ]);
        let model_pills = tg_button_row(&[
            tg_cb_button_styled("🤖 Pick Model", &format!("ses:models:{idx}"), "primary"),
            tg_cb_button_styled("🧹 Clear Model", &format!("ses:clearmod:{idx}"), "danger"),
        ]);
        let comp_pills = tg_button_row(&[
            tg_cb_button_styled("🗜 10%", &format!("ses:comp:{idx}:10"), ""),
            tg_cb_button_styled("🗜 25%", &format!("ses:comp:{idx}:25"), ""),
            tg_cb_button_styled("🗜 50%", &format!("ses:comp:{idx}:50"), ""),
            tg_cb_button_styled("🧹 Reset Comp", &format!("ses:comp:{idx}:0"), "danger"),
        ]);
        let md = format!(
            "**🧠 Session · {}**\n\n{}\n\n{}\n\n{}\n\n{}\n\n_Mode & model updates apply on the next turn (no restart required)._",
            md_text(&item.label),
            field_grid(&[
                ("Key", format!("`{}`", md_text(&item.key))),
                ("Mode", hl(&item.mode)),
                ("Model Override", format!("`{}`", md_text(&item.model))),
                ("Compression", comp_str),
            ]),
            mode_pills,
            model_pills,
            comp_pills,
        );
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Sessions", "menu:sessions", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, "session-detail", &md, kb);
    }

    fn render_session_model_picker(&self, surf: Surface, msg_id: Option<i64>, idx: usize) {
        let dd = self.cfg.router_data_dir.as_deref();
        let models: Vec<String> = match router_get(&self.cfg.router_base_url, &self.cfg.router_api_key, dd, "/v1/models", 10) {
            Ok(v) => extract_router_list(&v)
                .into_iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(ToString::to_string))
                .take(12)
                .collect(),
            Err(_) => Vec::new(),
        };
        {
            let mut st = self.state.lock().unwrap();
            st.model_choices = models.clone();
            self.persist(&st);
        }
        let list_md = if models.is_empty() {
            "_No models returned by 9Router `/v1/models`._".to_string()
        } else {
            let mrows: Vec<Vec<String>> = models
                .iter()
                .enumerate()
                .map(|(mi, m)| {
                    vec![
                        format!("#{}", mi + 1),
                        tg_cb_button_styled(&truncate_str(m, 34), &format!("ses:setmod:{idx}:{mi}"), "link"),
                    ]
                })
                .collect();
            table_markdown(&["#", "Model"], &mrows)
        };
        let clear_pill = tg_button_row(&[tg_cb_button_styled(
            "🧹 Clear Model Override",
            &format!("ses:clearmod:{idx}"),
            "danger",
        )]);
        let md = format!(
            "**🤖 Select Model Override**\n\n{list_md}\n\n{clear_pill}"
        );
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Back", &format!("ses:open:{idx}"), "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, "model-picker", &md, kb);
    }

    // ── Tools Panel ─────────────────────────────────────────────────────────
    fn render_tools_menu(&self, surf: Surface, msg_id: Option<i64>) {
        let row1 = tg_button_row(&[
            tg_cb_button_styled("🩺 Self-Check", "tool:selfcheck", "success"),
            tg_cb_button_styled("📜 Recent Audit", "tool:audit", "primary"),
            tg_cb_button_styled("🗂 Diagnostics", "tool:diag", "primary"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled("📦 Hermes Ver", "tool:hver", "primary"),
            tg_cb_button_styled("🧪 Hermes Doctor", "tool:hdoc", "primary"),
            tg_cb_button_styled("👥 Profiles & GW", "tool:hprof", "primary"),
        ]);
        let md = format!(
            "**🛠 Diagnostics & Operator Tools**\n\nInspect PX-Ops self-health, Hermes CLI, host diagnostics, and audit history.\n\n{row1}\n\n{row2}"
        );
        self.deliver(surf, msg_id, "tools", &md, Self::kb_back_home(vec![]));
    }

    fn exec_tool(&self, surf: Surface, msg_id: Option<i64>, sub: &str) {
        let kb = Self::kb_back_home(vec![button("↩ Tools", "menu:tools", "primary")]);
        let t0 = Instant::now();
        let md = match sub {
            "selfcheck" => {
                let sudo = run_argv("sudo", &svec(&["-n", "true"]), None, 4);
                let nnp = fs::read_to_string("/proc/self/status")
                    .ok()
                    .and_then(|s| {
                        s.lines()
                            .find(|l| l.starts_with("NoNewPrivs:"))
                            .map(|l| l.trim().to_string())
                    })
                    .unwrap_or_else(|| "unknown".into());
                let apt = run_shell(
                    "sudo -n bash -lc 't=/var/lib/apt/lists/.pxops-check; touch \"$t\" && rm -f \"$t\"'",
                    None,
                    5,
                );
                let (maint, cd_n, man_n, off) = {
                    let st = self.state.lock().unwrap();
                    (st.maintenance, st.cooldowns.len(), st.manual_off.len(), st.offset)
                };
                let ses_n = self
                    .cfg
                    .hermes_profile_dir
                    .as_deref()
                    .map(|p| load_agent_sessions(p, &self.cfg.config_dir).len())
                    .unwrap_or(0);
                format!(
                    "**🩺 PX-Ops Self-Check**\n\n{}\n\n_Updated: {}_",
                    field_grid(&[
                        ("Version", format!("`v{VERSION}` (Rust static)")),
                        ("Self Unit", format!("`{}`", self.cfg.self_unit)),
                        ("PID", std::process::id().to_string()),
                        ("Sudo -n", if sudo.exit_code == 0 { hl("✅ OK") } else { hl("❌ Failed") }),
                        ("Privilege Flag", format!("`{nnp}`")),
                        ("APT Lists Write", if apt.exit_code == 0 { "✅ OK".into() } else { "❌ Blocked".into() }),
                        ("Update Offset", off.to_string()),
                        ("Sessions Found", ses_n.to_string()),
                        ("Maintenance", if maint { "⚠️ Active".into() } else { "✅ Off".into() }),
                        ("Cooldowns / ManualOff", format!("{cd_n} / {man_n}")),
                    ]),
                    stamp_wib()
                )
            }
            "audit" => {
                let rows_a = self.jobs.recent_audit(15);
                if rows_a.is_empty() {
                    "**📜 Recent Audit**\n\n_No audit entries yet._".to_string()
                } else {
                    let trows: Vec<Vec<String>> = rows_a
                        .iter()
                        .map(|r| {
                            let at = r.get("at").and_then(Value::as_str).unwrap_or("—");
                            let hhmm = at.get(11..19).unwrap_or(at);
                            vec![
                                hhmm.to_string(),
                                truncate_str(r.get("action").and_then(Value::as_str).unwrap_or("?"), 20),
                                r.get("exit_code").and_then(Value::as_i64).unwrap_or(0).to_string(),
                                truncate_str(r.get("summary").and_then(Value::as_str).unwrap_or(""), 18),
                            ]
                        })
                        .collect();
                    format!(
                        "**📜 Recent Audit**\n\n{}\n\n_Updated: {}_",
                        table_markdown(&["UTC", "Action", "Exit", "Note"], &trows),
                        stamp_wib()
                    )
                }
            }
            "hver" | "hdoc" | "hprof" => {
                let Some(ref hbin) = self.cfg.hermes_bin else {
                    self.deliver(surf, msg_id, "hermes", "**⚠️ HERMES_BIN not configured.**", kb);
                    return;
                };
                let (title, cmd_str, timeout) = match sub {
                    "hver" => ("Hermes Version", format!("{} --version", hbin.display()), 10),
                    "hdoc" => ("Hermes Doctor", format!("{} doctor", hbin.display()), 90),
                    _ => (
                        "Profiles & Gateways",
                        format!("{} profile list; echo '---'; {} gateway list", hbin.display(), hbin.display()),
                        20,
                    ),
                };
                let r = run_shell(&cmd_str, None, timeout);
                format!(
                    "**🛠 {title}**\n\n```\n{}\n```\n_Exit {} · {} ms_",
                    truncate_str(&r.output, 3000).replace("```", "'''"),
                    r.exit_code,
                    r.duration_ms
                )
            }
            "diag" => {
                let dir = self
                    .cfg
                    .diagnostics_dir
                    .clone()
                    .unwrap_or_else(|| self.cfg.jobs_dir.join("diagnostics"));
                let _ = fs::create_dir_all(&dir);
                let fname = format!("px-ops-{}.txt", utc_compact_stamp());
                let fpath = dir.join(&fname);
                let r = run_shell(
                    "hostnamectl 2>/dev/null; echo '---FAILED SYS---'; systemctl --failed --no-pager; echo '---FAILED USER---'; systemctl --user --failed --no-pager; echo '---DISK---'; df -h /; echo '---MEM---'; free -h; echo '---NET---'; ip -brief address",
                    None,
                    20,
                );
                let _ = fs::write(&fpath, &r.output);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&fpath, fs::Permissions::from_mode(0o600));
                }
                let _ = self.tg.send_document_to(
                    surf.chat_id,
                    surf.thread_id,
                    &fname,
                    r.output.as_bytes(),
                    Some("PX-Ops Diagnostics Bundle"),
                );
                format!(
                    "**🗂 Diagnostics Bundle Created**\n\n{}",
                    field_grid(&[
                        ("File", format!("`{fname}`")),
                        ("Size", human_bytes(r.output.len() as u64)),
                    ])
                )
            }
            _ => "**🛠 Tools**\n\nUnknown tool.".to_string(),
        };
        self.jobs
            .append_audit(&format!("tool.{sub}"), "", 0, t0.elapsed().as_millis() as u64, sub);
        self.deliver(surf, msg_id, &format!("tool-{sub}"), &md, kb);
    }

    // ── Backup Panel ────────────────────────────────────────────────────────
    fn render_backup_menu(&self, surf: Surface, msg_id: Option<i64>) {
        let Some(ref bdir) = self.cfg.backup_dir else {
            self.deliver(
                surf,
                msg_id,
                "backup",
                "**💾 Backup Not Configured**\n\nSet `BACKUP_DIR` in the service vault.",
                Self::kb_back_home(vec![]),
            );
            return;
        };
        let targets = load_backup_targets(&self.cfg.config_dir);
        let latest = list_latest_backups(bdir, &targets);
        let trows: Vec<Vec<String>> = latest
            .iter()
            .map(|e| {
                vec![
                    tg_cb_button_styled(&e.label, &format!("bak:open:{}", e.target), "link"),
                    if e.file.is_some() { human_bytes(e.bytes) } else { "—".into() },
                    truncate_str(&e.created_at, 16),
                ]
            })
            .collect();

        let pills = tg_button_row(&[tg_cb_button_styled("📦 Backup All", "bak:all", "success")]);
        let kb = json!({
            "inline_keyboard": [[
                button("🔄 Refresh", "menu:backup", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });

        let md = format!(
            "**💾 Backup Manager**\n\n{}\n\n{}\n\nTap a target in the table to create, verify, or restore.\n_Updated: {}_",
            if trows.is_empty() {
                "_No backup targets configured._".to_string()
            } else {
                table_markdown(&["Target", "Size", "Latest"], &trows)
            },
            pills,
            stamp_wib()
        );
        self.deliver(surf, msg_id, "backup", &md, kb);
    }

    fn render_backup_target(&self, surf: Surface, msg_id: Option<i64>, target_id: &str) {
        let Some(ref bdir) = self.cfg.backup_dir else {
            return;
        };
        let targets = load_backup_targets(&self.cfg.config_dir);
        let Some(t) = targets.into_iter().find(|x| x.id == target_id) else {
            self.render_backup_menu(surf, msg_id);
            return;
        };
        let latest = list_latest_backups(bdir, std::slice::from_ref(&t))
            .into_iter()
            .next();
        let file_opt = latest.as_ref().and_then(|e| e.file.clone());
        let mut action_pills = vec![tg_cb_button_styled(
            "➕ Create Backup",
            &format!("bak:create:{}", t.id),
            "success",
        )];
        if file_opt.is_some() {
            action_pills.push(tg_cb_button_styled(
                "✅ Verify SHA-256",
                &format!("bak:verify:{}", t.id),
                "primary",
            ));
            action_pills.push(tg_cb_button_styled(
                "♻️ Restore",
                &format!("bak:restore:{}", t.id),
                "danger",
            ));
        }
        let md = format!(
            "**💾 Backup Target · {}**\n\n{}\n\n{}",
            md_text(&t.label),
            field_grid(&[
                ("ID", format!("`{}`", t.id)),
                ("Retention", format!("{} archives", t.retention)),
                ("Paths", format!("{} configured", t.paths.len())),
                (
                    "Latest File",
                    file_opt
                        .as_deref()
                        .map(|f| format!("`{f}`"))
                        .unwrap_or_else(|| "None yet".into())
                ),
                (
                    "Latest Size",
                    latest
                        .as_ref()
                        .filter(|e| e.file.is_some())
                        .map(|e| human_bytes(e.bytes))
                        .unwrap_or_else(|| "—".into())
                ),
            ]),
            tg_button_row(&action_pills),
        );
        let kb = json!({
            "inline_keyboard": [[
                button("↩ Backup", "menu:backup", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, &format!("bak-{}", t.id), &md, kb);
    }

    // ── Jobs, Power, Settings, Help ─────────────────────────────────────────
    fn render_jobs(&self, surf: Surface, msg_id: Option<i64>) {
        let list = self.jobs.list();
        let trows: Vec<Vec<String>> = list
            .iter()
            .take(10)
            .map(|j| {
                vec![
                    tg_cb_button_styled(
                        &format!("{} {}", job_icon(&j.status), j.id),
                        &format!("job:open:{}", j.id),
                        "link",
                    ),
                    j.status.clone(),
                    truncate_str(&j.label, 28),
                ]
            })
            .collect();
        let kb_rows = vec![json!([
            button("🔄 Refresh", "menu:jobs", "primary"),
            button("🏠 Home", "menu:dashboard", "primary"),
        ])];
        let md = format!(
            "**📋 Background Jobs**\n\n{}\n\n{}\n\nUse `/job <id>` or `/stop <id>`.\n_Updated: {}_",
            field_grid(&[
                ("Tracked", list.len().to_string()),
                ("Running", hl(&self.jobs.running_count().to_string())),
            ]),
            if trows.is_empty() {
                "_No background jobs recorded yet._".to_string()
            } else {
                table_markdown(&["Job", "Status", "Label"], &trows)
            },
            stamp_wib()
        );
        self.deliver(surf, msg_id, "jobs", &md, json!({"inline_keyboard": kb_rows}));
    }

    fn render_job_detail(&self, surf: Surface, msg_id: Option<i64>, jid: &str) {
        let Some(job) = self.jobs.get(jid) else {
            self.deliver(
                surf,
                msg_id,
                "job",
                &format!("**📋 Job `{}` not found.**", md_text(jid)),
                Self::kb_back_home(vec![button("📋 Jobs", "menu:jobs", "primary")]),
            );
            return;
        };
        let log = self.jobs.read_log(&job);
        let running = job.status == "running";
        let stop_pill = if running {
            format!(
                "\n\n{}",
                tg_button_row(&[tg_cb_button_styled("🛑 Stop Job", &format!("job:stop:{}", job.id), "danger")])
            )
        } else {
            String::new()
        };
        let md = format!(
            "**📄 Job `{}`**\n\n{}{}\n\n{}\n\n_Updated: {}_",
            job.id,
            field_grid(&[
                ("Status", hl(&format!("{} {}", job_icon(&job.status), job.status))),
                ("Label", md_text(&job.label)),
                ("Exit", job.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "—".into())),
            ]),
            stop_pill,
            details_block(
                "Log Tail",
                &format!("```\n{}\n```", tail_str(&log, 2200).replace("```", "'''"))
            ),
            stamp_wib()
        );
        let kb = json!({
            "inline_keyboard": [[
                button("🔄 Refresh", &format!("job:open:{}", job.id), "primary"),
                button("📋 Jobs", "menu:jobs", "primary"),
                button("🏠 Home", "menu:dashboard", "primary"),
            ]]
        });
        self.deliver(surf, msg_id, &format!("job-{}", job.id), &md, kb);
    }

    fn render_power(&self, surf: Surface, msg_id: Option<i64>) {
        let pills = tg_button_row(&[
            tg_cb_button_styled("🔄 Reboot Host", "pwr:reboot", "danger"),
            tg_cb_button_styled("⏹ Shutdown Host", "pwr:shutdown", "danger"),
        ]);
        let md = format!(
            "**⚡ Host Power Control**\n\n{}\n\n{}\n\n⚠️ Reboot requires confirmation; Shutdown requires **2-stage confirmation**.",
            field_grid(&[
                ("Reboot", "`sudo -n systemctl reboot` · back in ~60s".into()),
                ("Shutdown", "`sudo -n systemctl poweroff` · requires physical power-on".into()),
            ]),
            pills,
        );
        self.deliver(surf, msg_id, "power", &md, Self::kb_back_home(vec![]));
    }

    fn render_settings(&self, surf: Surface, msg_id: Option<i64>) {
        let th = load_thresholds(&self.cfg.config_dir);
        let forum = load_ops_forum(&self.cfg.config_dir);
        let (maint, cd_n, man_n) = {
            let st = self.state.lock().unwrap();
            (st.maintenance, st.cooldowns.len(), st.manual_off.len())
        };
        let pills = tg_button_row(&[
            tg_cb_button_styled(
                if maint { "✅ Disable Maintenance" } else { "⚠️ Enable Maintenance" },
                "set:maint",
                if maint { "success" } else { "danger" },
            ),
            tg_cb_button_styled("🧹 Clear Cooldowns", "set:clear_cd", "primary"),
        ]);
        let md = format!(
            "**⚙️ PX-Ops Settings**\n\n{}\n\n{}\n\n_Thresholds and routing are loaded live from `OPS_CONFIG_DIR`._",
            field_grid(&[
                ("Maintenance", if maint { hl("⚠️ Active (alerts paused)") } else { "✅ Off".into() }),
                ("Alert Interval", format!("{}s · cooldown {}s", th.interval_seconds, th.cooldown_seconds)),
                ("Thresholds", format!("RAM >{:.0}% · Disk >{:.0}% · Temp >{:.0}°C", th.ram_percent, th.disk_percent, th.temperature_c)),
                ("OPS Forum", if forum.enabled { format!("Enabled (alerts: {}, jobs: {})", forum.notify.alerts, forum.notify.jobs) } else { "Disabled (DM only)".into() }),
                ("Active Cooldowns", cd_n.to_string()),
                ("Manual-Off Units", man_n.to_string()),
            ]),
            pills,
        );
        self.deliver(surf, msg_id, "settings", &md, Self::kb_back_home(vec![]));
    }

    fn render_help(&self, surf: Surface, msg_id: Option<i64>) {
        let md = format!(
            "**❓ PX-Ops Command Guide**\n\n{}\n\n_Native Rust host operator · Telegram-only · v{VERSION}_",
            field_grid(&[
                ("Panels", "`/start` · `/apps` · `/services` · `/server` · `/router` · `/sessions`".into()),
                ("Tools", "`/tools` · `/backup` · `/jobs` · `/job <id>` · `/stop <id>`".into()),
                ("System", "`/power` · `/settings` · `/exec <cmd>` · `/cwd [dir]` · `/envkeys`".into()),
                ("Safety", "Confirmations required for stops, updates, restores, reboot, and 2-stage shutdown".into()),
            ])
        );
        self.deliver(surf, msg_id, "help", &md, Self::kb_back_home(vec![]));
    }

    // ── Confirmation execution ──────────────────────────────────────────────
    fn ask_confirm(
        &self,
        surf: Surface,
        msg_id: Option<i64>,
        title: &str,
        fields: &[(&str, String)],
        note: &str,
        btn_label: &str,
        action: ConfirmAction,
        danger: bool,
    ) {
        let tok = self.create_confirm(action);
        let md = format!(
            "**⚠️ Confirm · {}**\n\n{}\n\n{}\n_Expires in 3 minutes._",
            md_text(title),
            field_grid(fields),
            note
        );
        self.tg.present_in(
            surf.chat_id,
            surf.thread_id,
            msg_id,
            &md,
            Some(Self::kb_confirm(&tok, btn_label, danger)),
        );
    }

    fn execute_confirmed(self: &Arc<Self>, surf: Surface, msg_id: Option<i64>, action: ConfirmAction) {
        let app = Arc::clone(self);
        match action {
            ConfirmAction::ServiceOp { id, op } => {
                thread::spawn(move || app.exec_service_op(surf, msg_id, &id, &op));
            }
            ConfirmAction::ServerUpdate => {
                thread::spawn(move || {
                    let pre = run_shell(
                        "sudo -n true && sudo -n bash -lc 't=/var/lib/apt/lists/.pxops-pre; touch \"$t\" && rm -f \"$t\"'",
                        None,
                        6,
                    );
                    if pre.exit_code != 0 {
                        app.deliver(
                            surf,
                            msg_id,
                            "update-fail",
                            "**❌ Apply Updates Aborted**\n\nSudo / APT preflight failed. Run **Tools → Self-Check**.",
                            Self::kb_back_home(vec![button("🩺 Self-Check", "tool:selfcheck", "primary")]),
                        );
                        return;
                    }
                    let app_done = Arc::clone(&app);
                    match app.jobs.start_shell(
                        "sudo -n apt-get update && sudo -n apt-get upgrade -y".to_string(),
                        "apt update && upgrade".to_string(),
                        None,
                        move |job, tail| {
                            let badge = if job.status == "success" { "✅ Completed" } else { "❌ Failed" };
                            let md = format!(
                                "**📦 Server Update · {badge}**\n\n{}\n\n{}",
                                field_grid(&[
                                    ("Job", format!("`{}`", job.id)),
                                    ("Exit", job.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())),
                                ]),
                                details_block("Log Tail", &format!("```\n{}\n```", tail.replace("```", "'''")))
                            );
                            app_done.notify_route("jobs", &md, Some(Self::kb_back_home(vec![button("📋 Jobs", "menu:jobs", "primary")])));
                        },
                    ) {
                        Ok(job) => {
                            let md = format!(
                                "**⏳ Server Update Started**\n\n{}",
                                field_grid(&[
                                    ("Job ID", format!("`{}`", job.id)),
                                    ("Command", "`apt-get update && upgrade -y`".into()),
                                ])
                            );
                            app.deliver(
                                surf,
                                msg_id,
                                "update-start",
                                &md,
                                Self::kb_back_home(vec![button("📄 View Job", &format!("job:open:{}", job.id), "primary")]),
                            );
                        }
                        Err(e) => {
                            app.deliver(surf, msg_id, "update-err", &format!("**❌ Launch failed:** {}", md_text(&e)), Self::kb_back_home(vec![]));
                        }
                    }
                });
            }
            ConfirmAction::ServerReboot => {
                let md = "**🔄 Host Rebooting Now**\n\nRemote session will disconnect briefly (~60s). Send `/start` once back online.";
                self.tg.present_in(surf.chat_id, surf.thread_id, msg_id, md, None);
                self.jobs.append_audit("server.reboot", "", 0, 0, "reboot scheduled");
                let _ = run_argv("sudo", &svec(&["-n", "systemctl", "reboot"]), None, 5);
            }
            ConfirmAction::ServerShutdownStep1 => {
                self.ask_confirm(
                    surf,
                    msg_id,
                    "FINAL Shutdown Confirmation (2/2)",
                    &[
                        ("Action", hl("POWER OFF HOST")),
                        ("Warning", "Requires physical button or IPMI to power back on".into()),
                    ],
                    "⚠️ **Last check:** Are you completely sure you want to power off the server?",
                    "⏹ YES — POWER OFF",
                    ConfirmAction::ServerShutdownStep2,
                    true,
                );
            }
            ConfirmAction::ServerShutdownStep2 => {
                let md = "**⏹ Host Powering Off Now**\n\nServer is shutting down. Physical access is required to turn it back on.";
                self.tg.present_in(surf.chat_id, surf.thread_id, msg_id, md, None);
                self.jobs.append_audit("server.shutdown", "", 0, 0, "poweroff scheduled");
                let _ = run_argv("sudo", &svec(&["-n", "systemctl", "poweroff"]), None, 5);
            }
            ConfirmAction::BackupCreate { target_id } => {
                thread::spawn(move || {
                    let Some(ref bdir) = app.cfg.backup_dir else {
                        return;
                    };
                    let targets = load_backup_targets(&app.cfg.config_dir);
                    let Some(t) = targets.into_iter().find(|x| x.id == target_id) else {
                        return;
                    };
                    let t0 = Instant::now();
                    let kb = Self::kb_back_home(vec![button("↩ Backup", "menu:backup", "primary")]);
                    match create_backup(bdir, &t, "backup") {
                        Ok(man) => {
                            let md = format!(
                                "**💾 Backup Created · {}**\n\n{}",
                                md_text(&t.label),
                                field_grid(&[
                                    ("File", format!("`{}`", man.id)),
                                    ("Size", hl(&human_bytes(man.bytes))),
                                    ("SHA-256", format!("`{}…`", &man.sha256[..12])),
                                    ("Duration", format!("{} ms", t0.elapsed().as_millis())),
                                ])
                            );
                            app.jobs.append_audit("backup.create", &t.id, 0, t0.elapsed().as_millis() as u64, &man.id);
                            app.deliver(surf, msg_id, "backup-ok", &md, kb);
                        }
                        Err(e) => {
                            app.jobs.append_audit("backup.create", &t.id, 1, t0.elapsed().as_millis() as u64, &e);
                            app.deliver(surf, msg_id, "backup-err", &format!("**❌ Backup Failed**\n\n{}", md_text(&e)), kb);
                        }
                    }
                });
            }
            ConfirmAction::BackupAll => {
                thread::spawn(move || {
                    let Some(ref bdir) = app.cfg.backup_dir else {
                        return;
                    };
                    let targets = load_backup_targets(&app.cfg.config_dir);
                    let t0 = Instant::now();
                    let mut ok_rows = Vec::new();
                    let mut fails = Vec::new();
                    for t in &targets {
                        match create_backup(bdir, t, "backup-all") {
                            Ok(m) => ok_rows.push(vec![t.label.clone(), human_bytes(m.bytes)]),
                            Err(e) => fails.push(format!("{}: {}", t.label, truncate_str(&e, 60))),
                        }
                    }
                    let fail_md = if fails.is_empty() {
                        String::new()
                    } else {
                        format!("\n\n**Failed:**\n- {}", fails.join("\n- "))
                    };
                    let md = format!(
                        "**💾 Backup All · {}/{} OK**\n\n{}{fail_md}\n\n_Completed in {} ms · {}_",
                        ok_rows.len(),
                        targets.len(),
                        table_markdown(&["Target", "Size"], &ok_rows),
                        t0.elapsed().as_millis(),
                        stamp_wib()
                    );
                    app.jobs.append_audit(
                        "backup.all",
                        "",
                        if fails.is_empty() { 0 } else { 1 },
                        t0.elapsed().as_millis() as u64,
                        &format!("{}/{}", ok_rows.len(), targets.len()),
                    );
                    app.deliver(surf, msg_id, "backup-all", &md, Self::kb_back_home(vec![button("↩ Backup", "menu:backup", "primary")]));
                });
            }
            ConfirmAction::BackupRestore { target_id, file_name } => {
                thread::spawn(move || {
                    let Some(ref bdir) = app.cfg.backup_dir else {
                        return;
                    };
                    let targets = load_backup_targets(&app.cfg.config_dir);
                    let Some(t) = targets.into_iter().find(|x| x.id == target_id) else {
                        return;
                    };
                    let t0 = Instant::now();
                    let kb = Self::kb_back_home(vec![button("↩ Backup", "menu:backup", "primary")]);
                    match restore_backup(bdir, &t, &file_name) {
                        Ok((ver, chk)) => {
                            let md = format!(
                                "**♻️ Restore Complete · {}**\n\n{}",
                                md_text(&t.label),
                                field_grid(&[
                                    ("Restored", format!("`{}`", ver.id)),
                                    ("Size", human_bytes(ver.bytes)),
                                    ("Pre-Restore Checkpoint", format!("`{}`", chk.id)),
                                    ("SHA-256", hl("✅ Verified")),
                                ])
                            );
                            app.jobs.append_audit("backup.restore", &t.id, 0, t0.elapsed().as_millis() as u64, &ver.id);
                            app.deliver(surf, msg_id, "restore-ok", &md, kb);
                        }
                        Err(e) => {
                            app.jobs.append_audit("backup.restore", &t.id, 1, t0.elapsed().as_millis() as u64, &e);
                            app.deliver(surf, msg_id, "restore-err", &format!("**❌ Restore Failed**\n\n{}", md_text(&e)), kb);
                        }
                    }
                });
            }
            ConfirmAction::SessionsBulkMode { mode } => {
                if let Some(ref pdir) = self.cfg.hermes_profile_dir {
                    let list = load_agent_sessions(pdir, &self.cfg.config_dir);
                    for s in &list {
                        let _ = set_agent_session_mode(pdir, &s.key, &mode);
                    }
                    self.jobs.append_audit("sessions.bulk.mode", &mode, 0, 0, &format!("count={}", list.len()));
                }
                self.render_sessions(surf, msg_id, 0);
            }
            ConfirmAction::SessionsBulkModel { model } => {
                if let Some(ref pdir) = self.cfg.hermes_profile_dir {
                    let list = load_agent_sessions(pdir, &self.cfg.config_dir);
                    for s in &list {
                        set_agent_session_model(pdir, &s.key, model.as_deref(), &self.cfg.router_base_url);
                    }
                    self.jobs.append_audit(
                        "sessions.bulk.model",
                        model.as_deref().unwrap_or("clear"),
                        0,
                        0,
                        &format!("count={}", list.len()),
                    );
                }
                self.render_sessions(surf, msg_id, 0);
            }
            ConfirmAction::SessionsBulkCompress { pct } => {
                if let Some(ref pdir) = self.cfg.hermes_profile_dir {
                    let list = load_agent_sessions(pdir, &self.cfg.config_dir);
                    let val = pct.map(|p| p as f64 / 100.0);
                    for s in &list {
                        set_agent_session_compress(pdir, &s.key, val);
                    }
                    self.jobs.append_audit(
                        "sessions.bulk.compress",
                        &pct.map(|p| format!("{p}%")).unwrap_or_else(|| "reset".into()),
                        0,
                        0,
                        &format!("count={}", list.len()),
                    );
                }
                self.render_sessions(surf, msg_id, 0);
            }
            ConfirmAction::SessionsCompressNow => {
                if let Some(ref pdir) = self.cfg.hermes_profile_dir {
                    let list = load_agent_sessions(pdir, &self.cfg.config_dir);
                    let keys: Vec<String> = list.iter().map(|s| s.key.clone()).collect();
                    trigger_compress_now(pdir, &keys);
                    self.jobs.append_audit("sessions.compress_now", "all", 0, 0, &format!("count={}", keys.len()));
                }
                self.render_sessions(surf, msg_id, 0);
            }
        }
    }

    // ── Background alert monitor thread ─────────────────────────────────────
    fn spawn_alert_loop(self: &Arc<Self>) {
        let app = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(15));
            loop {
                let th = load_thresholds(&app.cfg.config_dir);
                let interval = th.interval_seconds.max(30);
                let maint = app.state.lock().unwrap().maintenance;
                if !maint {
                    app.run_alert_cycle(&th);
                }
                thread::sleep(Duration::from_secs(interval));
            }
        });
    }

    fn run_alert_cycle(&self, th: &sys::ThresholdsConfig) {
        let mut problems: Vec<(String, String)> = Vec::new();
        let m = read_host_metrics();
        if m.load1 > (m.cpu_cores as f64) * th.load_per_cpu {
            problems.push(("CPU Load".into(), format!("{:.2} ({} cores)", m.load1, m.cpu_cores)));
        }
        if m.ram_total > 0 {
            let ram_pct = (m.ram_used as f64 / m.ram_total as f64) * 100.0;
            if ram_pct > th.ram_percent {
                problems.push(("RAM".into(), format!("{ram_pct:.1}% (>{:.0}%)", th.ram_percent)));
            }
        }
        if m.disk_pct > th.disk_percent {
            problems.push(("Disk /".into(), format!("{:.0}% (>{:.0}%)", m.disk_pct, th.disk_percent)));
        }
        if let Some(tc) = m.temp_c {
            if tc > th.temperature_c {
                problems.push(("Temperature".into(), format!("{tc:.1}°C (>{:.0}°C)", th.temperature_c)));
            }
        }

        let services = load_services(&self.cfg.config_dir);
        let mut snaps = collect_service_snaps(&services);
        // Retry once after 1.5s if any monitored service returned "unknown"
        let has_unknown = snaps.iter().any(|s| {
            !s.def.is_app()
                && !s.def.is_soft()
                && s.def.unit != self.cfg.self_unit
                && s.state == "unknown"
        });
        if has_unknown {
            thread::sleep(Duration::from_millis(1500));
            snaps = collect_service_snaps(&services);
        }

        let manual_off = self.state.lock().unwrap().manual_off.clone();
        let mut stale_marks = Vec::new();
        for s in &snaps {
            if s.def.is_app() || s.def.is_soft() || s.def.unit == self.cfg.self_unit {
                continue;
            }
            if is_transitional(&s.state) || is_transitional(&s.sub) {
                continue;
            }
            if manual_off.contains(&s.def.id) {
                if s.tone() == "ok" {
                    stale_marks.push(s.def.id.clone());
                }
                continue;
            }
            if s.tone() != "ok" {
                problems.push((
                    s.def.label.clone(),
                    if s.sub.is_empty() {
                        s.status_badge()
                    } else {
                        format!("{} / {}", s.status_badge(), s.sub)
                    },
                ));
            }
        }
        if !stale_marks.is_empty() {
            let mut st = self.state.lock().unwrap();
            for id in stale_marks {
                st.manual_off.remove(&id);
            }
            self.persist(&st);
        }

        if problems.is_empty() {
            return;
        }
        let key = problems
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join("|");
        let now = now_epoch();
        {
            let mut st = self.state.lock().unwrap();
            if let Some(&last) = st.cooldowns.get(&key) {
                if now.saturating_sub(last) < th.cooldown_seconds {
                    return;
                }
            }
            // Record cooldown BEFORE sending so restarts never spam duplicates
            st.cooldowns.insert(key, now);
            st.cooldowns
                .retain(|_, &mut ts| now.saturating_sub(ts) < th.cooldown_seconds * 4);
            self.persist(&st);
        }

        let trows: Vec<Vec<String>> = problems
            .iter()
            .map(|(k, v)| vec![k.clone(), v.clone()])
            .collect();
        let body = format!(
            "**⚠️ Host Alert** · {} issue(s)\n\n{}\n\n_Optional parked apps ignored · {}_",
            problems.len(),
            table_markdown(&["Component", "Condition"], &trows),
            stamp_wib()
        );
        self.notify_route("alerts", &body, Some(Self::kb_back_home(vec![button("🧩 Services", "menu:services", "primary")])));
    }
}

// ── callback & message routers ──────────────────────────────────────────────
fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let from_id = cb.pointer("/from/id").and_then(Value::as_i64).unwrap_or(0);
    let chat_id = cb
        .pointer("/message/chat/id")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if !app.is_authorized(chat_id, from_id) {
        return;
    }
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);
    let thread_id = cb
        .pointer("/message/message_thread_id")
        .and_then(Value::as_i64);
    let surf = Surface { chat_id, thread_id };
    let data = cb.get("data").and_then(Value::as_str).unwrap_or("");

    // Confirmation gate
    if let Some(tok) = data.strip_prefix("cfm:yes:") {
        app.tg.ack(cb_id, "Confirmed…");
        match app.take_confirm(tok) {
            Some(act) => app.execute_confirmed(surf, msg_id, act),
            None => {
                app.tg.present_in(
                    surf.chat_id,
                    surf.thread_id,
                    msg_id,
                    "**⏱ Confirmation Expired**\n\nPlease trigger the action again.",
                    Some(App::kb_main()),
                );
            }
        }
        return;
    }
    if let Some(tok) = data.strip_prefix("cfm:no:") {
        app.tg.ack(cb_id, "Cancelled");
        app.cancel_confirm(tok);
        let a = Arc::clone(app);
        thread::spawn(move || a.render_dashboard(surf, msg_id));
        return;
    }

    // Menu navigation
    if let Some(target) = data.strip_prefix("menu:") {
        app.tg.ack(cb_id, "");
        let a = Arc::clone(app);
        if target == "dashboard" {
            thread::spawn(move || a.render_dashboard(surf, msg_id));
        } else if target == "apps" {
            thread::spawn(move || a.render_apps(surf, msg_id));
        } else if target == "services" {
            thread::spawn(move || a.render_services(surf, msg_id, 0));
        } else if let Some(pg) = target.strip_prefix("services:") {
            let p = pg.parse::<usize>().unwrap_or(0);
            thread::spawn(move || a.render_services(surf, msg_id, p));
        } else if target == "server" {
            app.render_server_menu(surf, msg_id);
        } else if target == "router" {
            app.render_router_menu(surf, msg_id);
        } else if target == "sessions" {
            app.render_sessions(surf, msg_id, 0);
        } else if target == "tools" {
            app.render_tools_menu(surf, msg_id);
        } else if target == "backup" {
            thread::spawn(move || a.render_backup_menu(surf, msg_id));
        } else if target == "jobs" {
            app.render_jobs(surf, msg_id);
        } else if target == "power" {
            app.render_power(surf, msg_id);
        } else if target == "settings" {
            app.render_settings(surf, msg_id);
        } else if target == "help" {
            app.render_help(surf, msg_id);
        }
        return;
    }

    // Apps 1-tap toggle
    if let Some(id) = data.strip_prefix("app:toggle:") {
        app.tg.ack(cb_id, "Toggling app…");
        let a = Arc::clone(app);
        let sid = id.to_string();
        thread::spawn(move || a.exec_app_toggle(surf, msg_id, &sid));
        return;
    }

    // Services
    if let Some(rest) = data.strip_prefix("svc:") {
        if let Some((op, id)) = rest.split_once(':') {
            app.tg.ack(cb_id, "");
            let a = Arc::clone(app);
            let sid = id.to_string();
            match op {
                "open" => {
                    thread::spawn(move || a.render_service_detail(surf, msg_id, &sid));
                }
                "logs" => {
                    thread::spawn(move || a.exec_service_logs(surf, msg_id, &sid));
                }
                "start" => {
                    thread::spawn(move || a.exec_service_op(surf, msg_id, &sid, "start"));
                }
                "stop" | "restart" => {
                    let is_stop = op == "stop";
                    app.ask_confirm(
                        surf,
                        msg_id,
                        &format!("{} Service `{sid}`", if is_stop { "Stop" } else { "Restart" }),
                        &[
                            ("Service", format!("`{sid}`")),
                            ("Operation", hl(op)),
                        ],
                        if is_stop {
                            "Stopping a core service also marks it manual-off to prevent alert spam."
                        } else {
                            "Restarting this service now?"
                        },
                        if is_stop { "⏹ Confirm Stop" } else { "🔁 Confirm Restart" },
                        ConfirmAction::ServiceOp {
                            id: sid,
                            op: op.to_string(),
                        },
                        is_stop,
                    );
                }
                _ => {}
            }
            return;
        }
    }

    // Server
    if let Some(sub) = data.strip_prefix("srv:") {
        app.tg.ack(cb_id, "");
        if sub == "apply_update" {
            app.ask_confirm(
                surf,
                msg_id,
                "Apply APT System Updates",
                &[
                    ("Command", "`sudo apt-get update && apt-get upgrade -y`".into()),
                    ("Execution", "Background job (preflight verified first)".into()),
                ],
                "Install all upgradable Debian/Ubuntu packages?",
                "⬆ Apply Updates",
                ConfirmAction::ServerUpdate,
                true,
            );
            return;
        }
        let a = Arc::clone(app);
        let s = sub.to_string();
        thread::spawn(move || a.exec_server_cmd(surf, msg_id, &s));
        return;
    }

    // 9Router
    if let Some(sub) = data.strip_prefix("rtr:") {
        app.tg.ack(cb_id, "");
        let a = Arc::clone(app);
        let s = sub.to_string();
        thread::spawn(move || a.exec_router(surf, msg_id, &s));
        return;
    }

    // Sessions
    if let Some(rest) = data.strip_prefix("ses:") {
        app.tg.ack(cb_id, "");
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.as_slice() {
            ["open", idx_s] => {
                if let Ok(idx) = idx_s.parse::<usize>() {
                    app.render_session_detail(surf, msg_id, idx);
                }
            }
            ["models", idx_s] => {
                if let Ok(idx) = idx_s.parse::<usize>() {
                    let a = Arc::clone(app);
                    thread::spawn(move || a.render_session_model_picker(surf, msg_id, idx));
                }
            }
            ["mode", idx_s, mode] => {
                if let (Ok(idx), Some(ref pdir)) = (idx_s.parse::<usize>(), &app.cfg.hermes_profile_dir) {
                    let key_opt = app.state.lock().unwrap().session_keys.get(idx).cloned();
                    if let Some(k) = key_opt {
                        let _ = set_agent_session_mode(pdir, &k, mode);
                    }
                    app.render_session_detail(surf, msg_id, idx);
                }
            }
            ["comp", idx_s, pct_s] => {
                if let (Ok(idx), Ok(p), Some(ref pdir)) = (idx_s.parse::<usize>(), pct_s.parse::<u32>(), &app.cfg.hermes_profile_dir) {
                    let key_opt = app.state.lock().unwrap().session_keys.get(idx).cloned();
                    if let Some(k) = key_opt {
                        if p == 0 {
                            set_agent_session_compress(pdir, &k, None);
                        } else {
                            set_agent_session_compress(pdir, &k, Some(p as f64 / 100.0));
                        }
                    }
                    app.render_session_detail(surf, msg_id, idx);
                }
            }
            ["clearmod", idx_s] => {
                if let (Ok(idx), Some(ref pdir)) = (idx_s.parse::<usize>(), &app.cfg.hermes_profile_dir) {
                    let key_opt = app.state.lock().unwrap().session_keys.get(idx).cloned();
                    if let Some(k) = key_opt {
                        set_agent_session_model(pdir, &k, None, &app.cfg.router_base_url);
                    }
                    app.render_session_detail(surf, msg_id, idx);
                }
            }
            ["setmod", idx_s, mi_s] => {
                if let (Ok(idx), Ok(mi), Some(ref pdir)) = (idx_s.parse::<usize>(), mi_s.parse::<usize>(), &app.cfg.hermes_profile_dir) {
                    let (key_opt, mod_opt) = {
                        let st = app.state.lock().unwrap();
                        (st.session_keys.get(idx).cloned(), st.model_choices.get(mi).cloned())
                    };
                    if let (Some(k), Some(m)) = (key_opt, mod_opt) {
                        set_agent_session_model(pdir, &k, Some(&m), &app.cfg.router_base_url);
                    }
                    app.render_session_detail(surf, msg_id, idx);
                }
            }
            ["page", p_s] => {
                let p = p_s.parse::<usize>().unwrap_or(0);
                app.render_sessions(surf, msg_id, p);
            }
            ["bmode"] => app.render_sessions_bulk_mode(surf, msg_id),
            ["bmodel"] => {
                let a = Arc::clone(app);
                thread::spawn(move || a.render_sessions_bulk_model(surf, msg_id));
            }
            ["bmodelset", mi_s] => {
                if let Ok(mi) = mi_s.parse::<usize>() {
                    let mod_opt = app.state.lock().unwrap().model_choices.get(mi).cloned();
                    if let Some(m) = mod_opt {
                        app.ask_confirm(
                            surf,
                            msg_id,
                            "Bulk Session Model Override",
                            &[("Model", format!("`{}`", md_text(&m))), ("Scope", "All Telegram sessions".into())],
                            "Apply this model override to every tracked session?",
                            "✅ Apply Bulk Model",
                            ConfirmAction::SessionsBulkModel { model: Some(m) },
                            false,
                        );
                    }
                }
            }
            ["bmodelclr"] => {
                app.ask_confirm(
                    surf,
                    msg_id,
                    "Clear All Model Overrides",
                    &[("Action", hl("Reset to default")), ("Scope", "All Telegram sessions".into())],
                    "Remove custom model override from every tracked session?",
                    "🧹 Clear All Models",
                    ConfirmAction::SessionsBulkModel { model: None },
                    true,
                );
            }
            ["bcomp"] => app.render_sessions_bulk_compress(surf, msg_id),
            ["bcompset", pct_s] => {
                if let Ok(p) = pct_s.parse::<u32>() {
                    let opt = if p == 0 { None } else { Some(p) };
                    let lbl = opt.map(|v| format!("{v}%")).unwrap_or_else(|| "Reset to preset".into());
                    app.ask_confirm(
                        surf,
                        msg_id,
                        "Bulk Session Compression",
                        &[("Threshold", hl(&lbl)), ("Scope", "All Telegram sessions".into())],
                        "Apply this compression threshold to every tracked session?",
                        "✅ Apply Bulk Compress",
                        ConfirmAction::SessionsBulkCompress { pct: opt },
                        opt.is_none(),
                    );
                }
            }
            ["compnow"] => {
                app.ask_confirm(
                    surf,
                    msg_id,
                    "Trigger Compress Now",
                    &[("Action", hl("Write compress-now.json")), ("Scope", "All active Telegram sessions".into())],
                    "Request immediate context compression for all active sessions?",
                    "🔥 Compress Now",
                    ConfirmAction::SessionsCompressNow,
                    true,
                );
            }
            ["bulk", mode] => {
                app.ask_confirm(
                    surf,
                    msg_id,
                    &format!("Bulk Session Mode → {mode}"),
                    &[("Mode", hl(mode)), ("Scope", "All Telegram sessions".into())],
                    "Apply this mode to every tracked session?",
                    "✅ Apply Bulk Mode",
                    ConfirmAction::SessionsBulkMode { mode: (*mode).to_string() },
                    false,
                );
            }
            _ => app.render_sessions(surf, msg_id, 0),
        }
        return;
    }

    // Tools
    if let Some(sub) = data.strip_prefix("tool:") {
        app.tg.ack(cb_id, "");
        let a = Arc::clone(app);
        let s = sub.to_string();
        thread::spawn(move || a.exec_tool(surf, msg_id, &s));
        return;
    }

    // Backup
    if let Some(rest) = data.strip_prefix("bak:") {
        app.tg.ack(cb_id, "");
        if rest == "all" {
            app.ask_confirm(
                surf,
                msg_id,
                "Backup All Targets",
                &[("Scope", "All configured targets sequentially".into())],
                "Create fresh compressed tar.gz + SHA-256 backups for all targets?",
                "📦 Start Backup All",
                ConfirmAction::BackupAll,
                false,
            );
            return;
        }
        if let Some((op, tid)) = rest.split_once(':') {
            let tid_s = tid.to_string();
            match op {
                "open" => app.render_backup_target(surf, msg_id, &tid_s),
                "create" => {
                    app.ask_confirm(
                        surf,
                        msg_id,
                        &format!("Create Backup · {tid_s}"),
                        &[("Target", format!("`{tid_s}`"))],
                        "Create a new verified `.tar.gz` snapshot?",
                        "💾 Create Backup",
                        ConfirmAction::BackupCreate { target_id: tid_s },
                        false,
                    );
                }
                "verify" => {
                    let a = Arc::clone(app);
                    thread::spawn(move || {
                        let Some(ref bdir) = a.cfg.backup_dir else {
                            return;
                        };
                        let targets = load_backup_targets(&a.cfg.config_dir);
                        let Some(t) = targets.into_iter().find(|x| x.id == tid_s) else {
                            return;
                        };
                        let latest = list_latest_backups(bdir, std::slice::from_ref(&t))
                            .into_iter()
                            .next()
                            .and_then(|e| e.file);
                        let kb = App::kb_back_home(vec![button("↩ Target", &format!("bak:open:{tid_s}"), "primary")]);
                        match latest {
                            Some(fname) => match verify_backup(bdir, &tid_s, &fname) {
                                Ok(m) => {
                                    let md = format!(
                                        "**✅ Backup Verified · {}**\n\n{}",
                                        md_text(&t.label),
                                        field_grid(&[
                                            ("File", format!("`{}`", m.id)),
                                            ("Size", human_bytes(m.bytes)),
                                            ("SHA-256", hl("✅ Match + tar OK")),
                                        ])
                                    );
                                    a.deliver(surf, msg_id, "verify-ok", &md, kb);
                                }
                                Err(e) => {
                                    a.deliver(surf, msg_id, "verify-err", &format!("**❌ Verification Failed**\n\n{}", md_text(&e)), kb);
                                }
                            },
                            None => {
                                a.deliver(surf, msg_id, "verify-none", "**ℹ️ No archive to verify.**", kb);
                            }
                        }
                    });
                }
                "restore" => {
                    if let Some(ref bdir) = app.cfg.backup_dir {
                        let targets = load_backup_targets(&app.cfg.config_dir);
                        if let Some(t) = targets.into_iter().find(|x| x.id == tid_s) {
                            if let Some(fname) = list_latest_backups(bdir, std::slice::from_ref(&t))
                                .into_iter()
                                .next()
                                .and_then(|e| e.file)
                            {
                                app.ask_confirm(
                                    surf,
                                    msg_id,
                                    &format!("Restore Backup · {}", t.label),
                                    &[
                                        ("Target", format!("`{}`", t.id)),
                                        ("Archive", format!("`{fname}`")),
                                        ("Safety", "Pre-restore checkpoint created automatically".into()),
                                    ],
                                    "⚠️ Live files will be overwritten from this archive.",
                                    "♻️ Confirm Restore",
                                    ConfirmAction::BackupRestore {
                                        target_id: t.id,
                                        file_name: fname,
                                    },
                                    true,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return;
    }

    // Jobs
    if let Some(rest) = data.strip_prefix("job:") {
        if let Some((op, jid)) = rest.split_once(':') {
            if op == "stop" {
                let ok = app.jobs.stop(jid);
                app.tg.ack(cb_id, if ok { "SIGTERM sent" } else { "Not running" });
                app.render_job_detail(surf, msg_id, jid);
            } else {
                app.tg.ack(cb_id, "");
                app.render_job_detail(surf, msg_id, jid);
            }
            return;
        }
    }

    // Power
    if let Some(sub) = data.strip_prefix("pwr:") {
        app.tg.ack(cb_id, "");
        if sub == "reboot" {
            app.ask_confirm(
                surf,
                msg_id,
                "Reboot Host Server",
                &[
                    ("Action", hl("systemctl reboot")),
                    ("Downtime", "~60 seconds".into()),
                ],
                "Restart the host immediately?",
                "🔄 Confirm Reboot",
                ConfirmAction::ServerReboot,
                true,
            );
        } else if sub == "shutdown" {
            app.ask_confirm(
                surf,
                msg_id,
                "Shutdown Host Server (Step 1/2)",
                &[
                    ("Action", hl("systemctl poweroff")),
                    ("Warning", "Host will NOT come back until manually powered on".into()),
                ],
                "⚠️ Proceed to final shutdown confirmation?",
                "⚠️ Continue (1/2)",
                ConfirmAction::ServerShutdownStep1,
                true,
            );
        }
        return;
    }

    // Settings
    if let Some(sub) = data.strip_prefix("set:") {
        app.tg.ack(cb_id, "Updated");
        {
            let mut st = app.state.lock().unwrap();
            if sub == "maint" {
                st.maintenance = !st.maintenance;
            } else if sub == "clear_cd" {
                st.cooldowns.clear();
            }
            app.persist(&st);
        }
        app.render_settings(surf, msg_id);
        return;
    }

    app.tg.ack(cb_id, "");
    let a = Arc::clone(app);
    thread::spawn(move || a.render_dashboard(surf, msg_id));
}

fn handle_message(app: &Arc<App>, msg: &Value) {
    let from_id = msg.pointer("/from/id").and_then(Value::as_i64).unwrap_or(0);
    let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64).unwrap_or(0);
    if !app.is_authorized(chat_id, from_id) {
        return;
    }
    let thread_id = msg.get("message_thread_id").and_then(Value::as_i64);
    let surf = Surface { chat_id, thread_id };
    let text = msg.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() || !text.starts_with('/') {
        return;
    }

    let (head_raw, rest_str) = match text.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (text, ""),
    };
    let cmd = head_raw.split('@').next().unwrap_or("").to_ascii_lowercase();
    let a = Arc::clone(app);

    match cmd.as_str() {
        "/start" | "/menu" => {
            thread::spawn(move || a.render_dashboard(surf, None));
        }
        "/apps" => {
            thread::spawn(move || a.render_apps(surf, None));
        }
        "/services" => {
            thread::spawn(move || a.render_services(surf, None, 0));
        }
        "/server" => app.render_server_menu(surf, None),
        "/router" => app.render_router_menu(surf, None),
        "/sessions" => app.render_sessions(surf, None, 0),
        "/tools" => app.render_tools_menu(surf, None),
        "/backup" => {
            thread::spawn(move || a.render_backup_menu(surf, None));
        }
        "/jobs" => app.render_jobs(surf, None),
        "/job" => {
            if rest_str.is_empty() {
                app.render_jobs(surf, None);
            } else {
                app.render_job_detail(surf, None, rest_str);
            }
        }
        "/stop" => {
            if rest_str.is_empty() {
                app.deliver(surf, None, "stop", "Usage: `/stop <job_id>`", App::kb_back_home(vec![]));
            } else {
                let ok = app.jobs.stop(rest_str);
                let md = format!(
                    "**🛑 Stop Job**\n\n{}",
                    field_grid(&[
                        ("Job ID", format!("`{}`", md_text(rest_str))),
                        ("Result", if ok { hl("SIGTERM sent") } else { "Not running".into() }),
                    ])
                );
                app.deliver(surf, None, "stop", &md, App::kb_back_home(vec![button("📋 Jobs", "menu:jobs", "primary")]));
            }
        }
        "/power" => app.render_power(surf, None),
        "/settings" => app.render_settings(surf, None),
        "/help" => app.render_help(surf, None),
        "/cwd" => {
            if rest_str.is_empty() {
                let cur = {
                    let s = app.state.lock().unwrap().exec_cwd.clone();
                    if s.is_empty() {
                        env::current_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "/".into())
                    } else {
                        s
                    }
                };
                app.deliver(
                    surf,
                    None,
                    "cwd",
                    &format!("**📁 Working Directory**\n\n`{}`", md_text(&cur)),
                    App::kb_back_home(vec![]),
                );
            } else {
                let p = Path::new(rest_str);
                if p.is_dir() {
                    let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                    {
                        let mut st = app.state.lock().unwrap();
                        st.exec_cwd = canon.display().to_string();
                        app.persist(&st);
                    }
                    app.deliver(
                        surf,
                        None,
                        "cwd",
                        &format!("**📁 Working Directory Updated**\n\n`{}`", md_text(&canon.display().to_string())),
                        App::kb_back_home(vec![]),
                    );
                } else {
                    app.deliver(
                        surf,
                        None,
                        "cwd",
                        &format!("**❌ Directory Not Found**\n\n`{}`", md_text(rest_str)),
                        App::kb_back_home(vec![]),
                    );
                }
            }
        }
        "/envkeys" => {
            let mut keys: Vec<String> = env::vars().map(|(k, _)| k).collect();
            keys.sort();
            let md = format!(
                "**🔑 Configured Environment Keys ({})**\n\n```\n{}\n```",
                keys.len(),
                keys.join("\n")
            );
            app.deliver(surf, None, "envkeys", &md, App::kb_back_home(vec![]));
        }
        "/exec" => {
            if rest_str.is_empty() {
                app.deliver(surf, None, "exec", "Usage: `/exec <shell command>`", App::kb_back_home(vec![]));
                return;
            }
            let command = rest_str.to_string();
            let cwd_opt = {
                let s = app.state.lock().unwrap().exec_cwd.clone();
                (!s.is_empty()).then(|| PathBuf::from(s))
            };
            thread::spawn(move || {
                let r = run_shell(&command, cwd_opt.as_deref(), 30);
                a.jobs.append_audit(
                    "exec",
                    &truncate_str(&command, 60),
                    r.exit_code,
                    r.duration_ms,
                    &format!("exit {}", r.exit_code),
                );
                let out_body = if r.output.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    r.output.clone()
                };
                let md = format!(
                    "**💻 Shell Exec**\n\n{}\n\n```\n{}\n```",
                    field_grid(&[
                        ("Command", format!("`{}`", md_text(&truncate_str(&command, 80)))),
                        ("Exit / Time", format!("{} · {} ms", r.exit_code, r.duration_ms)),
                    ]),
                    out_body.replace("```", "'''")
                );
                a.deliver(surf, None, "exec", &md, App::kb_back_home(vec![]));
            });
        }
        _ => app.render_help(surf, None),
    }
}

fn job_icon(status: &str) -> &'static str {
    match status {
        "success" => "✅",
        "failed" => "❌",
        "killed" => "🛑",
        "running" => "⏳",
        _ => "•",
    }
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-ops {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                let svcs = load_services(&cfg.config_dir);
                let baks = load_backup_targets(&cfg.config_dir);
                let forum = load_ops_forum(&cfg.config_dir);
                let cfg_ok = cfg.config_dir.is_dir();
                println!(
                    "OK: px-ops {VERSION} | config_dir={} (exists={cfg_ok}) | services={} | backup_targets={} | ops_forum={}",
                    cfg.config_dir.display(),
                    svcs.len(),
                    baks.len(),
                    forum.enabled
                );
                if !cfg_ok {
                    std::process::exit(1);
                }
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
    if let Some(p) = cfg.state_file.parent() {
        let _ = fs::create_dir_all(p);
    }

    let app = App::new(cfg);
    let svc_count = load_services(&app.cfg.config_dir).len();
    log_info(&format!(
        "PX-Ops v{VERSION} (Rust) started | config_dir={} | services={svc_count}",
        app.cfg.config_dir.display()
    ));

    app.tg.set_my_commands(&[
        ("start", "Open PX-Ops dashboard"),
        ("apps", "Toggle heavy RAM-saver apps"),
        ("services", "Service status & control"),
        ("server", "Host info, storage, network, updates"),
        ("router", "9Router health & catalog sync"),
        ("sessions", "Agent session modes & models"),
        ("tools", "Self-check, doctor, diagnostics, audit"),
        ("backup", "Create, verify & restore backups"),
        ("jobs", "List background jobs"),
        ("job", "View job log by ID"),
        ("stop", "Stop a running job"),
        ("power", "Confirmed reboot / shutdown"),
        ("settings", "Maintenance mode & alert settings"),
        ("exec", "Run host shell command"),
        ("cwd", "Show or change /exec working dir"),
        ("envkeys", "List configured env key names"),
        ("help", "Command reference"),
    ]);

    app.spawn_alert_loop();

    let owner_surf = Surface {
        chat_id: app.cfg.owner_id,
        thread_id: None,
    };
    app.render_dashboard(owner_surf, None);

    loop {
        let offset = { app.state.lock().unwrap().offset };
        match app.tg.get_updates(offset, 25) {
            Ok(updates) => {
                for u in updates {
                    if let Some(uid) = u.get("update_id").and_then(Value::as_i64) {
                        let mut st = app.state.lock().unwrap();
                        st.offset = uid + 1;
                        app.persist(&st);
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
