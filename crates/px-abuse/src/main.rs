mod af;

use af::{
    apply_reprio_router, effective_workers, extract_interesting_log_tail, extract_pool_items,
    format_farm_summary_line, get_provider, is_safe_batch_id, list_pending_cpa, load_batch_ids,
    mark_batch_deleted, normalize_import_result, parse_job_farm_summary, plan_reprio_from_items,
    run_af, smoke_args, summarize_pool_items, utc_compact_stamp, utc_iso_now, JobManager,
    JobRecord, PendingCpaItem, PROVIDERS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tg_core::bot::TgClient;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, format_duration, hl, md_text, progress_bar, rows,
    stamp_wib, table_markdown, tail_str, tg_button_row, tg_cb_button_styled, truncate_str,
};
use tg_core::{log_error, log_info, log_warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const CONFIRM_TTL_SECS: u64 = 180;
const BATCH_IMPORT_MAX: usize = 5;

// ── configuration (100% env-only) ───────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    af_bin: PathBuf,
    af_root: PathBuf,
    pending_cpa_root: Option<PathBuf>,
    router_env_file: Option<PathBuf>,
    router_backup_dir: Option<PathBuf>,
    gen_domains: Vec<String>,
    state_file: PathBuf,
    jobs_dir: PathBuf,
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

        let af_bin = PathBuf::from(
            env::var("AF_BIN")
                .map_err(|_| "AF_BIN environment variable is required".to_string())?,
        );
        let af_root = PathBuf::from(
            env::var("AF_ROOT")
                .map_err(|_| "AF_ROOT environment variable is required".to_string())?,
        );
        let pending_cpa_root = env::var("PENDING_CPA_ROOT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let router_env_file = env::var("ROUTER_ENV_FILE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let router_backup_dir = env::var("ROUTER_BACKUP_DIR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let gen_domains = env::var("GEN_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| "./state/px-abuse.json".to_string()),
        );
        let jobs_dir = PathBuf::from(
            env::var("JOBS_DIR").unwrap_or_else(|_| "./state/px-abuse-jobs".to_string()),
        );

        Ok(Self {
            tg_token,
            owner_id,
            af_bin,
            af_root,
            pending_cpa_root,
            router_env_file,
            router_backup_dir,
            gen_domains,
            state_file,
            jobs_dir,
        })
    }
}

// ── persistent state ────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    #[serde(default = "default_notify")]
    notify_mode: String,
    #[serde(default = "default_provider")]
    default_provider: String,
    #[serde(default = "default_workers")]
    default_workers: u32,
    #[serde(default = "default_smoke")]
    default_target_smoke: u32,
    #[serde(default = "default_batch")]
    default_target_batch: u32,
    #[serde(default = "default_rounds")]
    default_rounds: u32,
    #[serde(default = "default_proxy")]
    default_proxy_policy: String,
    #[serde(default)]
    auto_import_default: bool,
}

fn default_notify() -> String {
    "full".to_string()
}
fn default_provider() -> String {
    "grok".to_string()
}
fn default_workers() -> u32 {
    6
}
fn default_smoke() -> u32 {
    6
}
fn default_batch() -> u32 {
    12
}
fn default_rounds() -> u32 {
    1
}
fn default_proxy() -> String {
    "winners".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify_mode: default_notify(),
            default_provider: default_provider(),
            default_workers: default_workers(),
            default_target_smoke: default_smoke(),
            default_target_batch: default_batch(),
            default_rounds: default_rounds(),
            default_proxy_policy: default_proxy(),
            auto_import_default: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GenParams {
    #[serde(default)]
    workers: u32,
    #[serde(default)]
    target: u32,
    #[serde(default)]
    rounds: u32,
    #[serde(default)]
    proxy_policy: String,
    #[serde(default)]
    auto_import: bool,
    #[serde(default)]
    domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LastImport {
    email: String,
    connection_id: Option<String>,
    provider: String,
    at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SessionState {
    #[serde(default = "default_screen")]
    screen: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    params: GenParams,
    #[serde(default)]
    awaiting: Option<String>,
    #[serde(default)]
    pending_items: Vec<PendingCpaItem>,
    #[serde(default)]
    selected_emails: HashSet<String>,
    #[serde(default)]
    last_import: Option<LastImport>,
}

fn default_screen() -> String {
    "home".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    session: SessionState,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum ConfirmAction {
    GenerateRun {
        provider: String,
        mode: String,
        target: u32,
        workers: u32,
        max_rounds: u32,
        proxy_policy: String,
        auto_import: bool,
        domain: Option<String>,
    },
    SmokeRun {
        provider: String,
        target: u32,
        workers: u32,
        max_rounds: u32,
    },
    ArchiveQuickApply {
        provider: String,
    },
    ReprioApply {
        provider: String,
    },
    DeleteBatch {
        batch_id: String,
        ids: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct PendingConfirm {
    action: ConfirmAction,
    expires_at: u64,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    fn current_provider(&self) -> String {
        let st = self.state.lock().unwrap();
        st.session
            .provider
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| st.settings.default_provider.clone())
    }

    fn create_confirm(&self, action: ConfirmAction) -> String {
        let seq = self.confirm_seq.fetch_add(1, Ordering::Relaxed);
        let token = format!("{:x}{:02x}", now_epoch() & 0xFFFFF, seq & 0xFF);
        let mut map = self.confirms.lock().unwrap();
        let now = now_epoch();
        map.retain(|_, v| v.expires_at > now);
        map.insert(
            token.clone(),
            PendingConfirm {
                action,
                expires_at: now + CONFIRM_TTL_SECS,
            },
        );
        token
    }

    fn take_confirm(&self, token: &str) -> Option<ConfirmAction> {
        let mut map = self.confirms.lock().unwrap();
        let entry = map.remove(token)?;
        if entry.expires_at >= now_epoch() {
            Some(entry.action)
        } else {
            None
        }
    }

    fn cancel_confirm(&self, token: &str) {
        self.confirms.lock().unwrap().remove(token);
    }

    fn deliver(&self, msg_id: Option<i64>, title: &str, markdown: &str, kb: Value) -> Option<i64> {
        if markdown.len() <= 3900 {
            return self.tg.present(msg_id, markdown, Some(kb));
        }
        let safe_name = format!(
            "px-abuse-{}-{}.txt",
            utc_compact_stamp(),
            title
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .take(28)
                .collect::<String>()
        );
        let note = format!(
            "**📄 {}**\n\n{}\n\nFull output attached as a text file.\n\n_Updated: {}_",
            md_text(title),
            field_grid(&[
                ("Output", format!("`{}` ({} chars)", safe_name, markdown.len())),
                ("Status", hl("Attached below")),
            ]),
            stamp_wib()
        );
        let sent = self.tg.present(msg_id, &note, Some(kb));
        if let Err(e) = self.tg.send_document(&safe_name, markdown.as_bytes(), Some(title)) {
            log_warn(&format!("send_document failed: {e}"));
        }
        sent
    }

    // ── keyboards ───────────────────────────────────────────────────────────
    fn kb_home() -> Value {
        json!({
            "inline_keyboard": [
                [
                    button("📚 Playbooks", "nav:playbooks", "primary"),
                    button("📊 Pool", "nav:pool", "primary"),
                    button("📥 Import", "nav:import", "primary"),
                ],
                [
                    button("🌐 Proxy", "nav:proxy", "primary"),
                    button("📋 Jobs", "nav:jobs", "primary"),
                    button("⚙️ Settings", "nav:settings", "primary"),
                ],
                [button("❓ Help", "cmd:help", "")],
            ]
        })
    }

    fn kb_nav_footer(extra: Vec<Value>) -> Vec<Value> {
        let mut all = extra;
        all.push(button("🏠 Home", "nav:home", "primary"));
        all.push(button("↩ Back", "nav:back", "primary"));
        rows(all).as_array().cloned().unwrap_or_default()
    }

    fn kb_confirm(token: &str, danger: bool) -> Value {
        json!({
            "inline_keyboard": [
                [
                    button("✅ Confirm", &format!("cfm:yes:{token}"), if danger { "danger" } else { "success" }),
                    button("❌ Cancel", &format!("cfm:no:{token}"), "danger"),
                ],
                [button("🏠 Home", "nav:home", "primary")],
            ]
        })
    }

    fn kb_after_generate(job_id: &str, outcome: &str) -> Value {
        let mut r = Vec::new();
        if outcome == "zero_pass" {
            r.push(json!([
                button("📄 View Log", &format!("jobopen:{job_id}"), "primary"),
                button("🌐 Proxy", "nav:proxy", "primary"),
            ]));
            r.push(json!([
                button("↻ Retry Generate", "nav:generate", "success"),
                button("➡ Post-Generate", "pb:post_generate", "primary"),
            ]));
        } else {
            r.push(json!([
                button("📋 Jobs", "nav:jobs", "primary"),
                if !job_id.is_empty() {
                    button("📄 View Log", &format!("jobopen:{job_id}"), "primary")
                } else {
                    button("📊 Status", "cmd:status", "primary")
                },
            ]));
            r.push(json!([
                button("📥 Import / CPA", "nav:import", "primary"),
                button("➡ Post-Generate", "pb:post_generate", "primary"),
            ]));
        }
        r.push(json!([
            button("🏠 Home", "nav:home", "primary"),
            button("⚡ Generate", "nav:generate", "success"),
        ]));
        json!({"inline_keyboard": r})
    }

    fn kb_job(job_id: &str, running: bool) -> Value {
        let mut r = Vec::new();
        if running {
            r.push(json!([
                button("🔄 Refresh Log", &format!("jobopen:{job_id}"), "primary"),
                button("🛑 Stop Job", &format!("jobstop:{job_id}"), "danger"),
            ]));
        } else {
            r.push(json!([
                button("🔄 Refresh Log", &format!("jobopen:{job_id}"), "primary"),
                button("➡ Post-Generate", "pb:post_generate", "success"),
            ]));
        }
        r.push(json!([
            button("📋 Jobs", "nav:jobs", "primary"),
            button("🏠 Home", "nav:home", "primary"),
        ]));
        json!({"inline_keyboard": r})
    }

    fn kb_result() -> Value {
        json!({
            "inline_keyboard": [
                [
                    button("🏠 Home", "nav:home", "primary"),
                    button("📋 Jobs", "nav:jobs", "primary"),
                ],
                [
                    button("📊 Pool", "nav:pool", "primary"),
                    button("📚 Playbooks", "nav:playbooks", "primary"),
                ],
            ]
        })
    }

    fn kb_after_import() -> Value {
        json!({
            "inline_keyboard": [
                [
                    button("🔎 Chat-Probe", "probe:last", "primary"),
                    button("✅ Usable Scan", "cmd:usable", "success"),
                ],
                [
                    button("➡ Post-Generate", "pb:post_generate", "primary"),
                    button("📊 Status", "cmd:status", "primary"),
                ],
                [
                    button("📊 Pool", "nav:pool", "primary"),
                    button("🏠 Home", "nav:home", "primary"),
                ],
            ]
        })
    }

    fn kb_pending_import(items: &[PendingCpaItem], selected: &HashSet<String>) -> Value {
        let mut r: Vec<Value> = Vec::new();
        for (idx, it) in items.iter().take(6).enumerate() {
            let short = if it.email.chars().count() > 22 {
                let c: Vec<char> = it.email.chars().collect();
                let left: String = c[..9].iter().collect();
                let right: String = c[c.len() - 9..].iter().collect();
                format!("{left}…{right}")
            } else {
                it.email.clone()
            };
            let on = selected.contains(&it.email.to_ascii_lowercase());
            let prefix = if on { "✓ " } else { "○ " };
            r.push(json!([
                button(&format!("{prefix}{short}"), &format!("impsel:{idx}"), ""),
                button("▶ 1-Tap", &format!("imp:{idx}"), "success"),
            ]));
        }
        let sel_n = selected.len();
        let batch_lbl = if sel_n > 0 {
            format!("📥 Import Selected ({sel_n})")
        } else {
            "📥 Import Selected".to_string()
        };
        r.push(json!([
            button(&batch_lbl, "impbatch:run", "success"),
            button("☑ Select First 3", "impbatch:top3", "primary"),
        ]));
        r.push(json!([
            button("🧹 Clear Selection", "impbatch:clear", "danger"),
            button("🔄 Refresh Pending", "pb:post_generate", "primary"),
        ]));
        r.push(json!([
            button("❓ Import Help", "nav:import_help", ""),
            button("📊 Status", "cmd:status", "primary"),
        ]));
        r.push(json!([button("🏠 Home", "nav:home", "primary")]));
        json!({"inline_keyboard": r})
    }

    // ── screens ─────────────────────────────────────────────────────────────
    fn render_home(&self, msg_id: Option<i64>) {
        let (settings, running) = {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "home".to_string();
            st.session.awaiting = None;
            self.persist(&st);
            (st.settings.clone(), self.jobs.running_count())
        };
        let pills = tg_button_row(&[
            tg_cb_button_styled("⚡ Generate", "nav:generate", "success"),
            tg_cb_button_styled("📊 Status", "cmd:status", "primary"),
            tg_cb_button_styled("🔄 Refresh", "nav:home", "primary"),
        ]);
        let run_badge = if running > 0 {
            hl(&format!("⏳ {running} active"))
        } else {
            "0 idle".to_string()
        };
        let md = format!(
            "**🏭 PX-Abuse** · _v{VERSION}_\n\n{}\n\n{}\n\n_Account-Factory & Pool Console · {}_",
            field_grid(&[
                ("Provider", hl(&settings.default_provider)),
                ("Running Jobs", run_badge),
                (
                    "Defaults",
                    format!(
                        "W{} · Smoke {} · Batch {}",
                        settings.default_workers,
                        settings.default_target_smoke,
                        settings.default_target_batch
                    )
                ),
                ("Notify Mode", settings.notify_mode.clone()),
                (
                    "Auto-Import",
                    if settings.auto_import_default {
                        "ON (UI only)".to_string()
                    } else {
                        "OFF (UI only)".to_string()
                    }
                ),
            ]),
            pills,
            stamp_wib()
        );
        self.tg.present(msg_id, &md, Some(Self::kb_home()));
    }

    fn render_help(&self, msg_id: Option<i64>) {
        let md = format!(
            "**❓ PX-Abuse Command Guide**\n\n{}\n\n_Native Rust operator console · Telegram-only · {}_",
            field_grid(&[
                ("Navigation", "`/start` · `/menu` · `/playbooks` · `/settings`".into()),
                ("Inspect", "`/status` · `/usable [full]` · `/pool` · `/proxy [sync]`".into()),
                ("Import", "`/import <email>` · `/probe [id|email]`".into()),
                ("Cleanup", "`/archive [apply|quick [apply]]` · `/delete batch <id>`".into()),
                ("Ops", "`/ops daily | cleanup | postsmoke | reprio [apply]`".into()),
                ("Jobs", "`/smoke [dry|run]` · `/jobs` · `/job <id>` · `/stop <id>`".into()),
                ("Safety", "All mutating & live farm runs require explicit confirmation".into()),
            ]),
            stamp_wib()
        );
        self.tg.present(msg_id, &md, Some(Self::kb_result()));
    }

    fn render_provider_picker(&self, msg_id: Option<i64>) {
        {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "generate".to_string();
            st.session.action = None;
            st.session.awaiting = None;
            self.persist(&st);
        }
        let ready_n = PROVIDERS.iter().filter(|p| p.ready).count();
        let pills: Vec<String> = PROVIDERS
            .iter()
            .map(|p| {
                if p.ready {
                    tg_cb_button_styled(&format!("⚡ {}", p.label), &format!("prov:generate:{}", p.id), "success")
                } else {
                    tg_cb_button_styled(&format!("{} · Soon", p.label), &format!("provsoon:{}", p.id), "link")
                }
            })
            .collect();
        let kb_rows = Self::kb_nav_footer(vec![]);
        let md = format!(
            "**⚡ Generate · Select Provider**\n\n{}\n\n{}\n\nSelect a provider above.",
            field_grid(&[
                ("Flow", "Provider → Mode → Params → Preview".into()),
                ("Ready Providers", ready_n.to_string()),
            ]),
            tg_button_row(&pills),
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_gen_actions(&self, msg_id: Option<i64>) {
        let prov_id = {
            let mut st = self.state.lock().unwrap();
            let pid = st
                .session
                .provider
                .clone()
                .unwrap_or_else(|| st.settings.default_provider.clone());
            st.session.provider = Some(pid.clone());
            st.session.screen = "genactions".to_string();
            st.session.awaiting = None;
            self.persist(&st);
            pid
        };
        let label = get_provider(&prov_id).map(|p| p.label).unwrap_or(&prov_id);
        let mut kb_rows = rows(vec![
            button("🧪 Smoke", &format!("gen:{prov_id}:smoke"), "primary"),
            button("📦 Batch", &format!("gen:{prov_id}:batch"), "success"),
            button("🏭 Mass Farm", &format!("gen:{prov_id}:farm"), "danger"),
            button("📝 Dry-Run", &format!("gen:{prov_id}:dry"), ""),
        ])
        .as_array()
        .cloned()
        .unwrap_or_default();
        kb_rows.extend(Self::kb_nav_footer(vec![button(
            "🔄 Switch Provider",
            "nav:generate",
            "primary",
        )]));
        let md = format!(
            "**⚡ Generate · {}**\n\n{}\n\nSelect a generation mode.",
            md_text(label),
            field_grid(&[
                ("Provider", hl(label)),
                ("Smoke", "Small verification run".into()),
                ("Batch", "Standard stock refill".into()),
                ("Mass Farm", "High-volume run · strict confirmation".into()),
                ("Dry-Run", "Validate parameters without launching workers".into()),
            ])
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn ensure_gen_params(st: &mut BotState) -> (String, String, GenParams) {
        let prov = st
            .session
            .provider
            .clone()
            .unwrap_or_else(|| st.settings.default_provider.clone());
        let mode = st
            .session
            .action
            .clone()
            .unwrap_or_else(|| "batch".to_string());
        if st.session.params.workers == 0 {
            st.session.params.workers = st.settings.default_workers;
        }
        if st.session.params.target == 0 {
            st.session.params.target = match mode.as_str() {
                "smoke" | "dry" => st.settings.default_target_smoke,
                "farm" => st.settings.default_target_batch.max(24),
                _ => st.settings.default_target_batch,
            };
        }
        if st.session.params.rounds == 0 {
            st.session.params.rounds = st.settings.default_rounds;
        }
        if st.session.params.proxy_policy.is_empty() {
            st.session.params.proxy_policy = st.settings.default_proxy_policy.clone();
        }
        (prov, mode, st.session.params.clone())
    }

    fn render_gen_params(&self, msg_id: Option<i64>) {
        let (prov_id, mode, params) = {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "genparams".to_string();
            st.session.awaiting = None;
            let res = Self::ensure_gen_params(&mut st);
            self.persist(&st);
            res
        };
        let prov = get_provider(&prov_id);
        let label = prov.map(|p| p.label).unwrap_or(&prov_id);
        let domain_aware = prov.map(|p| p.domain_aware).unwrap_or(false);
        let (req_w, eff_w, w_note) = effective_workers(prov, params.workers);

        let mut fields = vec![
            ("Provider", hl(label)),
            ("Mode", capitalize(&mode)),
            ("Workers", format!("{req_w} → {eff_w} effective")),
            ("Target", params.target.to_string()),
            ("Rounds", params.rounds.to_string()),
        ];
        if mode == "farm" {
            fields.push((
                "Est. Slots",
                format!("~{} (target × rounds)", params.target * params.rounds),
            ));
        }
        fields.push(("Proxy", format!("{} · UI only", params.proxy_policy)));
        fields.push((
            "Auto-Import",
            format!("{} · UI only", if params.auto_import { "ON" } else { "OFF" }),
        ));
        fields.push((
            "Domain",
            if domain_aware {
                params.domain.clone().unwrap_or_else(|| "any".to_string())
            } else {
                format!("n/a · ignored by {label}")
            },
        ));

        let mut kb_rows = vec![
            json!([
                button(&format!("Workers: {}", params.workers), "gset:workers", ""),
                button(&format!("Target: {}", params.target), "gset:target", ""),
            ]),
            json!([
                button(&format!("Rounds: {}", params.rounds), "gset:rounds", ""),
                button(&format!("Proxy: {} · UI", params.proxy_policy), "gset:proxy", ""),
            ]),
            json!([
                button(
                    &format!("Auto-Import: {} · UI", if params.auto_import { "ON" } else { "OFF" }),
                    "gset:autoimport",
                    ""
                ),
                button("👁 Preview", "gset:preview", "success"),
            ]),
        ];
        if domain_aware && !self.cfg.gen_domains.is_empty() {
            let d = params.domain.as_deref().unwrap_or("any");
            kb_rows.push(json!([button(&format!("Domain: {d}"), "gset:domain", "")]));
        }
        kb_rows.extend(Self::kb_nav_footer(vec![button(
            "↩ Modes",
            "nav:genactions",
            "primary",
        )]));

        let md = format!(
            "**⚡ Generate · {} · {} · Parameters**\n\n{}\n\n_{}_\nTap a chip to adjust, then open Preview.",
            md_text(label),
            capitalize(&mode),
            field_grid(&fields),
            md_text(&w_note),
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_gen_preview(&self, msg_id: Option<i64>) {
        let (prov_id, mode, params, notify_mode) = {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "genpreview".to_string();
            st.session.awaiting = None;
            let (pr, mo, pa) = Self::ensure_gen_params(&mut st);
            let nm = st.settings.notify_mode.clone();
            self.persist(&st);
            (pr, mo, pa, nm)
        };
        let prov = get_provider(&prov_id);
        let label = prov.map(|p| p.label).unwrap_or(&prov_id);
        let domain_aware = prov.map(|p| p.domain_aware).unwrap_or(false);
        let (req_w, eff_w, _) = effective_workers(prov, params.workers);
        let soft_cap = (mode == "farm").then_some(params.target * params.rounds);

        let mut warns = Vec::new();
        if mode == "batch" && params.target > 24 {
            warns.push("Batch target > 24".to_string());
        }
        if let Some(sc) = soft_cap {
            if params.target > 48 || params.rounds > 3 || sc > 72 {
                warns.push(format!("Large farm (~{sc} est. slots) — verify proxy stock"));
            }
        }
        if req_w != eff_w {
            warns.push(format!("Effective workers fixed at {eff_w}"));
        }
        if params.auto_import {
            warns.push("Auto-import is UI state only".to_string());
        }
        warns.push("Proxy policy is UI state only".to_string());

        let mut fields = vec![
            ("Provider", hl(label)),
            ("Mode", capitalize(&mode)),
            ("Workers", format!("{req_w} → {eff_w}")),
            ("Target", params.target.to_string()),
            ("Rounds", params.rounds.to_string()),
        ];
        if let Some(sc) = soft_cap {
            fields.push(("Est. Slots", format!("~{sc}")));
        }
        fields.push(("Proxy", format!("{} · state only", params.proxy_policy)));
        fields.push((
            "Auto-Import",
            format!("{} · state only", if params.auto_import { "ON" } else { "OFF" }),
        ));
        fields.push((
            "Domain",
            if domain_aware {
                params.domain.clone().unwrap_or_else(|| "any".to_string())
            } else {
                format!("n/a · ignored by {prov_id}")
            },
        ));
        fields.push(("Notify", notify_mode));

        let run_label = match mode.as_str() {
            "dry" => "📝 Run Dry-Run",
            "farm" => "⚠️ Confirm Mass Farm",
            _ => "▶ Launch Job",
        };
        let run_style = if mode == "farm" { "danger" } else { "success" };
        let mut kb_rows = vec![json!([
            button(run_label, "genrun:go", run_style),
            button("✏️ Edit Params", "gset:backparams", "primary"),
        ])];
        kb_rows.extend(Self::kb_nav_footer(vec![]));

        let note = if mode == "dry" {
            "Dry-run validates configuration without launching browser workers."
        } else if mode == "farm" {
            "Mass farm requires an extra confirmation gate before starting."
        } else {
            "Launching creates a monitored background job with live progress updates."
        };
        let md = format!(
            "**👁 Generate Preview · {} · {}**\n\n{}\n\n⚠️ _{}_\n{}",
            md_text(label),
            capitalize(&mode),
            field_grid(&fields),
            md_text(&warns.join(" · ")),
            note
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_playbooks(&self, msg_id: Option<i64>) {
        let settings = {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "playbooks".to_string();
            st.session.awaiting = None;
            self.persist(&st);
            st.settings.clone()
        };
        let row1 = tg_button_row(&[
            tg_cb_button_styled("🧪 Quick Smoke", "pb:quick_smoke", "primary"),
            tg_cb_button_styled("📦 Fill Stock", "pb:fill_stock", "success"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled("➡ Post-Generate", "pb:post_generate", "primary"),
            tg_cb_button_styled("🧹 Cleanup Stock", "pb:cleanup", "danger"),
        ]);
        let md = format!(
            "**📚 Operator Playbooks**\n\n{}\n\n{}\n\n{}\n\n- **Quick Smoke** — Fast 1-round verification\n- **Fill Stock** — Standard batch run (auto-import OFF)\n- **Post-Generate** — Pending CPA + usable audit + import\n- **Cleanup Stock** — Archive quota/denied + reprio",
            field_grid(&[
                ("Default Provider", hl(&settings.default_provider)),
                ("Smoke Target", settings.default_target_smoke.to_string()),
                ("Batch Target", settings.default_target_batch.to_string()),
                ("Default Workers", settings.default_workers.to_string()),
            ]),
            row1,
            row2,
        );
        let kb = json!({
            "inline_keyboard": [[
                button("🏠 Home", "nav:home", "primary"),
                button("⚡ Generate", "nav:generate", "success"),
            ]]
        });
        self.tg.present(msg_id, &md, Some(kb));
    }

    fn render_pool_menu(&self, msg_id: Option<i64>) {
        {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "pool".to_string();
            st.session.awaiting = None;
            self.persist(&st);
        }
        let row1 = tg_button_row(&[
            tg_cb_button_styled("📊 Status", "cmd:status", "primary"),
            tg_cb_button_styled("📋 Pool List", "cmd:pool", "primary"),
            tg_cb_button_styled("✅ Usable Scan", "cmd:usable", "success"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled("↕ Reprio Plan", "cmd:ops_reprio", "primary"),
            tg_cb_button_styled("🗄 Archive Quick", "cmd:archive_quick", "danger"),
            tg_cb_button_styled("🧹 Cleanup", "pb:cleanup", "danger"),
        ]);
        let kb_rows = Self::kb_nav_footer(vec![]);
        let md = format!(
            "**📊 Pool & Usable Health**\n\n{}\n\n{}\n\n{}",
            field_grid(&[
                ("Status", "Usable pool classification + proxy summary".into()),
                ("Pool List", "Inspect active connections & priority order".into()),
                ("Usable Scan", "Run fresh `af usable scan` & report".into()),
                ("Reprio", "Rank fresh/transient first, quota/denied last".into()),
                ("Archive Quick", "Preview or archive quota + denied accounts".into()),
            ]),
            row1,
            row2,
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_import_menu(&self, msg_id: Option<i64>, help: bool) {
        {
            let mut st = self.state.lock().unwrap();
            st.session.screen = if help { "import_help" } else { "import" }.to_string();
            st.session.awaiting = None;
            self.persist(&st);
        }
        let md = if help {
            format!(
                "**❓ Import / CPA Workflow Guide**\n\n{}\n\n_Remember: `test_valid` checks auth only — always verify usable class._",
                field_grid(&[
                    ("Step 1", "Run Smoke / Batch to generate pending CPA files".into()),
                    ("Step 2", "Open Playbooks → Post-Generate to list pending accounts".into()),
                    ("Step 3", "Tap `○` to multi-select or `▶ 1-Tap` to import one".into()),
                    ("Step 4", "Batch import supports up to 5 accounts with live progress".into()),
                    ("Step 5", "Tap Chat-Probe or `/probe` to inspect pool auth + class".into()),
                    ("Slash", "`/import <email>` · `/probe [id|email]`".into()),
                ])
            )
        } else {
            format!(
                "**📥 Import / CPA**\n\n{}\n\n_`test_valid` ≠ usable · auto-import stays OFF by default._",
                field_grid(&[
                    ("Pending Scan", "Inspect generated `pending_cpa/xai-*.json` files".into()),
                    ("1-Tap Import", "Runs `af pool import-one --via-pilot`".into()),
                    ("Batch Import", format!("Up to {BATCH_IMPORT_MAX} selected accounts with live progress")),
                    ("Slash", "`/import <email>` · `/probe [id|email]`".into()),
                ])
            )
        };
        let pills = tg_button_row(&[
            tg_cb_button_styled("📥 Pending CPA", "cmd:ops_postsmoke", "primary"),
            tg_cb_button_styled("➡ Post-Generate", "pb:post_generate", "success"),
            tg_cb_button_styled("🔎 Probe Last", "probe:last", "primary"),
            tg_cb_button_styled("❓ Guide", "nav:import_help", "link"),
        ]);
        let md = format!("{md}\n\n{pills}");
        let kb_rows = Self::kb_nav_footer(vec![]);
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_proxy_menu(&self, msg_id: Option<i64>) {
        {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "proxy".to_string();
            st.session.awaiting = None;
            self.persist(&st);
        }
        let pills = tg_button_row(&[
            tg_cb_button_styled("📊 Proxy Status", "cmd:proxy", "primary"),
            tg_cb_button_styled("🔄 Sync Pool", "cmd:proxy_sync", "success"),
        ]);
        let kb_rows = Self::kb_nav_footer(vec![]);
        let md = format!(
            "**🌐 Proxy Pool**\n\n{}\n\n{}",
            field_grid(&[
                ("Status", "Ready, cooldown, quarantine, leased & top winners".into()),
                ("Sync", "Refresh proxy pool state via `af proxy sync`".into()),
            ]),
            pills,
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_jobs(&self, msg_id: Option<i64>) {
        {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "jobs".to_string();
            st.session.awaiting = None;
            self.persist(&st);
        }
        let jobs = self.jobs.list();
        let running_n = self.jobs.running_count();
        let table_md = if jobs.is_empty() {
            "_No background jobs recorded yet._".to_string()
        } else {
            let trows: Vec<Vec<String>> = jobs
                .iter()
                .take(10)
                .map(|j| {
                    vec![
                        tg_cb_button_styled(
                            &format!("{} {}", job_icon(&j.status), j.id),
                            &format!("jobopen:{}", j.id),
                            "link",
                        ),
                        j.status.clone(),
                        truncate_str(&j.label, 28),
                    ]
                })
                .collect();
            table_markdown(&["Job", "Status", "Label"], &trows)
        };
        let kb_rows = vec![json!([
            button("🔄 Refresh", "nav:jobs", "primary"),
            button("🏠 Home", "nav:home", "primary"),
        ])];

        let md = format!(
            "**📋 Background Jobs**\n\n{}\n\n{}\n\nTap a job below to view its log, or use `/job <id>` · `/stop <id>`.\n_Updated: {}_",
            field_grid(&[
                ("Total Tracked", jobs.len().to_string()),
                ("Running", hl(&running_n.to_string())),
            ]),
            table_md,
            stamp_wib()
        );
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn render_job_detail(&self, msg_id: Option<i64>, job_id: &str) {
        let Some(job) = self.jobs.get(job_id) else {
            let md = format!(
                "**📋 Job Not Found**\n\n{}",
                field_grid(&[("Job ID", format!("`{}`", md_text(job_id))), ("Status", "Not found".into())])
            );
            self.tg.present(msg_id, &md, Some(Self::kb_result()));
            return;
        };
        let log_text = self.jobs.read_log(&job);
        let farm = parse_job_farm_summary(&log_text);
        let farm_line = format_farm_summary_line(&farm);
        let running = job.status == "running";

        let mut fields = vec![
            ("Job ID", format!("`{}`", job.id)),
            ("Status", hl(&format!("{} {}", job_icon(&job.status), job.status))),
            ("Label", md_text(&job.label)),
            (
                "Exit Code",
                job.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".to_string()),
            ),
        ];
        if let Some(fl) = farm_line {
            fields.push(("Farm", fl));
        }
        if farm.outcome == "zero_pass" && !running {
            fields.push(("Warning", hl("Job exit 0 ≠ farm pass (0 created)")));
        }
        let log_tail = tail_str(&log_text, 2200);
        let md = format!(
            "**📄 Job `{}`**\n\n{}\n\n{}\n\n_Updated: {}_",
            job.id,
            field_grid(&fields),
            details_block("Log Tail", &format!("```\n{}\n```", log_tail.replace("```", "'''"))),
            stamp_wib()
        );
        let kb = if running {
            Self::kb_job(&job.id, true)
        } else if job.status == "success" {
            Self::kb_after_generate(&job.id, farm.outcome)
        } else {
            Self::kb_job(&job.id, false)
        };
        self.deliver(msg_id, &format!("job-{}", job.id), &md, kb);
    }

    fn render_settings(&self, msg_id: Option<i64>) {
        let s = {
            let mut st = self.state.lock().unwrap();
            st.session.screen = "settings".to_string();
            st.session.awaiting = None;
            self.persist(&st);
            st.settings.clone()
        };
        let row1 = tg_button_row(&[
            tg_cb_button_styled(&format!("Notify: {}", s.notify_mode), "set:notify", "primary"),
            tg_cb_button_styled(&format!("Provider: {}", s.default_provider), "set:provider", "primary"),
            tg_cb_button_styled(&format!("Workers: {}", s.default_workers), "set:workers", "primary"),
        ]);
        let row2 = tg_button_row(&[
            tg_cb_button_styled(&format!("Batch: {}", s.default_target_batch), "set:batch", "primary"),
            tg_cb_button_styled(&format!("Smoke: {}", s.default_target_smoke), "set:smoke", "primary"),
            tg_cb_button_styled(&format!("Proxy: {}", s.default_proxy_policy), "set:proxy", ""),
            tg_cb_button_styled(
                &format!("Auto-Import: {}", if s.auto_import_default { "ON" } else { "OFF" }),
                "set:autoimport",
                if s.auto_import_default { "success" } else { "" },
            ),
        ]);
        let md = format!(
            "**⚙️ PX-Abuse Settings**\n\n{}\n\n{}\n\n{}\n\n_Slash `/smoke run` uses the defaults above._",
            field_grid(&[
                ("Notify Mode", format!("{} (simple / full)", hl(&s.notify_mode))),
                ("Default Provider", hl(&s.default_provider)),
                ("Default Workers", s.default_workers.to_string()),
                ("Smoke Target", s.default_target_smoke.to_string()),
                ("Batch Target", s.default_target_batch.to_string()),
                ("Default Rounds", s.default_rounds.to_string()),
                ("Proxy Policy", format!("{} · UI only", s.default_proxy_policy)),
                (
                    "Auto-Import",
                    format!("{} · UI only", if s.auto_import_default { "ON" } else { "OFF" })
                ),
            ]),
            row1,
            row2,
        );
        let kb_rows = Self::kb_nav_footer(vec![]);
        self.tg
            .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
    }

    // ── af operations (run in background worker thread to keep loop crisp) ──
    fn show_working(&self, msg_id: Option<i64>, title: &str, detail: &str) -> Option<i64> {
        let md = format!(
            "**⏳ {}**\n\n{}\n\n_{}_",
            md_text(title),
            field_grid(&[("Status", hl("Running…")), ("Action", md_text(detail))]),
            stamp_wib()
        );
        self.tg.present(msg_id, &md, None)
    }

    fn exec_status(&self, msg_id: Option<i64>) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Status Overview", &format!("Querying {prov} pool & proxy"));
        let t0 = Instant::now();

        let bin = self.cfg.af_bin.clone();
        let root = self.cfg.af_root.clone();
        let p_clone = prov.clone();
        let h_usable = thread::spawn({
            let (b, r) = (bin.clone(), root.clone());
            move || run_af(&b, &r, &svec(&["usable", "report", "--json"]), 30)
        });
        let h_proxy = thread::spawn({
            let (b, r) = (bin.clone(), root.clone());
            move || run_af(&b, &r, &svec(&["proxy", "status", "--json"]), 30)
        });
        let h_provs = thread::spawn({
            let (b, r) = (bin.clone(), root.clone());
            move || run_af(&b, &r, &svec(&["providers", "list", "--json"]), 20)
        });
        let h_pool = thread::spawn(move || {
            run_af(&bin, &root, &svec(&["--provider", &p_clone, "pool", "list", "--json"]), 60)
        });

        let r_usable = h_usable.join().unwrap();
        let r_proxy = h_proxy.join().unwrap();
        let r_provs = h_provs.join().unwrap();
        let r_pool = h_pool.join().unwrap();

        let pool_items = extract_pool_items(r_pool.json.as_ref());
        let summary = summarize_pool_items(&pool_items);
        let ratio = if summary.total > 0 {
            summary.fresh as f64 / summary.total as f64
        } else {
            0.0
        };
        let bar = format!("`{}` ({}/{})", progress_bar(ratio, 10), summary.fresh, summary.total);

        let pj = r_proxy.json.as_ref();
        let proxy_line = format!(
            "total {} · ready {} · cooldown {} · quarantine {} · leased {}",
            json_num_str(pj, "total"),
            json_num_str(pj, "ready"),
            json_num_str(pj, "cooldown"),
            json_num_str(pj, "quarantine"),
            json_num_str(pj, "leased"),
        );

        let mut sorted = summary.items.clone();
        sorted.sort_by_key(|x| x.priority);
        let top_rows: Vec<Vec<String>> = sorted
            .iter()
            .take(6)
            .map(|it| {
                vec![
                    format!("p{}", it.priority),
                    format!("{}/{}", it.usable_class, it.usable_code),
                    truncate_str(&it.email, 28),
                ]
            })
            .collect();
        let top_md = if top_rows.is_empty() {
            "_Pool empty._".to_string()
        } else {
            table_markdown(&["Prio", "Class", "Account"], &top_rows)
        };

        let prov_lines = r_provs
            .json
            .as_ref()
            .and_then(|v| v.get("providers").or_else(|| v.get("items")).and_then(Value::as_array))
            .map(|arr| {
                arr.iter()
                    .map(|x| {
                        let name = x
                            .get("name")
                            .or_else(|| x.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let st = x
                            .get("status")
                            .or_else(|| x.get("state"))
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        format!("- **{name}**: `{st}`")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| "_Unavailable_".to_string());

        let usable_brief = format_usable_report_brief(r_usable.json.as_ref());
        let hints_md = summary
            .hints
            .iter()
            .map(|h| format!("- {h}"))
            .collect::<Vec<_>>()
            .join("\n");

        let md = format!(
            "**📊 PX-Abuse · Status Overview**\n\n{}\n\n**Top Priority Pool**\n{}\n\n**Next Actions**\n{}\n\n{}\n\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Health", format!("{} {}", health_icon(summary.health), hl(summary.health))),
                ("Fresh Stock", bar),
                ("Breakdown", summary.counts_line.clone()),
                ("Proxy Pool", proxy_line),
            ]),
            top_md,
            hints_md,
            details_block(
                "Usable Report & Providers",
                &format!("**Usable Report**\n{usable_brief}\n\n**Providers**\n{prov_lines}")
            ),
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        let code = r_usable.exit_code.max(r_proxy.exit_code).max(r_pool.exit_code);
        self.jobs
            .append_audit("status", &prov, code, t0.elapsed().as_millis() as u64, summary.health);
        self.deliver(work_mid, "status", &md, Self::kb_result());
    }

    fn exec_usable(&self, msg_id: Option<i64>, full: bool) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Usable Scan", &format!("Scanning {prov} pool"));
        let t0 = Instant::now();
        let scan = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["usable", "scan", "--from-pool", "--provider", &prov, "--json"]),
            120,
        );
        let report = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["usable", "report", "--json"]),
            25,
        );
        let brief = format_usable_report_brief(report.json.as_ref());
        let mut extra = String::new();
        if full {
            if let Some(items) = report
                .json
                .as_ref()
                .and_then(|v| v.get("items").and_then(Value::as_array))
            {
                let trows: Vec<Vec<String>> = items
                    .iter()
                    .take(30)
                    .map(|it| {
                        vec![
                            it.get("class").and_then(Value::as_str).unwrap_or("?").to_string(),
                            it.get("error_code")
                                .and_then(|v| v.as_str().map(ToString::to_string).or_else(|| v.as_i64().map(|n| n.to_string())))
                                .unwrap_or_else(|| "-".to_string()),
                            truncate_str(
                                it.get("identity").and_then(Value::as_str).unwrap_or(""),
                                32,
                            ),
                        ]
                    })
                    .collect();
                if !trows.is_empty() {
                    extra = format!("\n\n**Items**\n{}", table_markdown(&["Class", "Code", "Identity"], &trows));
                }
            }
        }
        let code = if scan.exit_code != 0 { scan.exit_code } else { report.exit_code };
        let md = format!(
            "**✅ PX-Abuse · Usable Scan**\n\n{}\n\n{}{}\n\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Mode", if full { "full".into() } else { "summary".into() }),
                ("Exit Code", code.to_string()),
            ]),
            brief,
            extra,
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs
            .append_audit("usable", &prov, code, t0.elapsed().as_millis() as u64, "usable");
        self.deliver(work_mid, "usable", &md, Self::kb_result());
    }

    fn exec_pool(&self, msg_id: Option<i64>) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Pool List", &format!("Listing {prov} pool"));
        let t0 = Instant::now();
        let res = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", &prov, "pool", "list", "--json"]),
            60,
        );
        let items = extract_pool_items(res.json.as_ref());
        let summary = summarize_pool_items(&items);
        let mut by_test: HashMap<String, usize> = HashMap::new();
        for it in &summary.items {
            *by_test.entry(it.test_status.clone()).or_insert(0) += 1;
        }
        let mut test_parts: Vec<String> = by_test
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        test_parts.sort();
        let test_line = if test_parts.is_empty() {
            "empty".to_string()
        } else {
            test_parts.join(" · ")
        };

        let mut sorted = summary.items.clone();
        sorted.sort_by_key(|x| x.priority);
        let trows: Vec<Vec<String>> = sorted
            .iter()
            .take(12)
            .map(|it| {
                vec![
                    format!("p{}", it.priority),
                    format!("{}/{}", it.usable_class, it.usable_code),
                    it.test_status.clone(),
                    truncate_str(&it.email, 26),
                ]
            })
            .collect();
        let top_md = if trows.is_empty() {
            "_No connections in pool._".to_string()
        } else {
            table_markdown(&["Prio", "Class", "Test", "Account"], &trows)
        };
        let hints_md = summary
            .hints
            .iter()
            .map(|h| format!("- {h}"))
            .collect::<Vec<_>>()
            .join("\n");

        let md = format!(
            "**📋 PX-Abuse · Pool List**\n\n{}\n\n**Top Priority Connections**\n{}\n\n**Next**\n{}\n\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Total", summary.total.to_string()),
                ("Health", format!("{} {}", health_icon(summary.health), hl(summary.health))),
                ("Usable", summary.counts_line.clone()),
                ("testStatus", test_line),
            ]),
            top_md,
            hints_md,
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs
            .append_audit("pool", &prov, res.exit_code, t0.elapsed().as_millis() as u64, "pool");
        self.deliver(work_mid, "pool", &md, Self::kb_result());
    }

    fn exec_proxy(&self, msg_id: Option<i64>, sync: bool) {
        let work_mid = self.show_working(
            msg_id,
            if sync { "Proxy Sync" } else { "Proxy Status" },
            if sync { "Syncing winners pool" } else { "Reading proxy pool status" },
        );
        let t0 = Instant::now();
        let (code, body) = if sync {
            let s = run_af(&self.cfg.af_bin, &self.cfg.af_root, &svec(&["proxy", "sync", "--json"]), 60);
            let st = run_af(&self.cfg.af_bin, &self.cfg.af_root, &svec(&["proxy", "status", "--json"]), 25);
            let c = if s.exit_code != 0 { s.exit_code } else { st.exit_code };
            let txt = format!(
                "{}\n\n{}",
                details_block("Sync Output", &format!("```json\n{}\n```", compact_json(s.json.as_ref(), &s.output, 1100))),
                details_block("Current Status", &format!("```json\n{}\n```", compact_json(st.json.as_ref(), &st.output, 800))),
            );
            (c, txt)
        } else {
            let st = run_af(&self.cfg.af_bin, &self.cfg.af_root, &svec(&["proxy", "status", "--json"]), 25);
            let w = run_af(&self.cfg.af_bin, &self.cfg.af_root, &svec(&["proxy", "winners", "--json"]), 25);
            let c = if st.exit_code != 0 { st.exit_code } else { w.exit_code };
            let pj = st.json.as_ref();
            let wcount = json_num_str(w.json.as_ref(), "count");
            let sample = w
                .json
                .as_ref()
                .and_then(|v| v.get("winners").and_then(Value::as_array))
                .map(|arr| {
                    let s: Vec<Value> = arr.iter().take(5).cloned().collect();
                    serde_json::to_string_pretty(&s).unwrap_or_default()
                })
                .unwrap_or_else(|| "(none)".to_string());
            let txt = format!(
                "{}\n\n{}",
                field_grid(&[
                    ("Total", json_num_str(pj, "total")),
                    ("Ready", hl(&json_num_str(pj, "ready"))),
                    ("Cooldown", json_num_str(pj, "cooldown")),
                    ("Quarantine", json_num_str(pj, "quarantine")),
                    ("Leased", json_num_str(pj, "leased")),
                    ("Winners", wcount),
                ]),
                details_block("Sample Winners (Top 5)", &format!("```json\n{}\n```", truncate_str(&sample, 900)))
            );
            (c, txt)
        };

        let title = if sync { "Proxy Sync" } else { "Proxy Status" };
        let md = format!(
            "**🌐 PX-Abuse · {title}**\n\n{body}\n\n_Completed in {} ms · {}_",
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs.append_audit(
            if sync { "proxy.sync" } else { "proxy" },
            "",
            code,
            t0.elapsed().as_millis() as u64,
            title,
        );
        self.deliver(work_mid, "proxy", &md, Self::kb_result());
    }

    fn exec_archive_quick(&self, msg_id: Option<i64>, apply: bool, provider_override: Option<&str>) {
        let prov = provider_override
            .map(ToString::to_string)
            .unwrap_or_else(|| self.current_provider());
        let work_mid = self.show_working(
            msg_id,
            if apply { "Archive Quick · Apply" } else { "Archive Quick · Preview" },
            &format!("Checking {prov} quota/denied accounts"),
        );
        let t0 = Instant::now();
        if !apply {
            let pool = run_af(
                &self.cfg.af_bin,
                &self.cfg.af_root,
                &svec(&["--provider", &prov, "pool", "list", "--json"]),
                60,
            );
            let items = extract_pool_items(pool.json.as_ref());
            let summary = summarize_pool_items(&items);
            let targets: Vec<_> = summary
                .items
                .iter()
                .filter(|it| matches!(it.usable_class, "quota" | "denied"))
                .collect();
            let list_md = if targets.is_empty() {
                "_No quota or denied accounts found._".to_string()
            } else {
                let trows: Vec<Vec<String>> = targets
                    .iter()
                    .take(20)
                    .map(|it| {
                        vec![
                            format!("p{}", it.priority),
                            format!("{}/{}", it.usable_class, it.usable_code),
                            truncate_str(&it.email, 28),
                        ]
                    })
                    .collect();
                table_markdown(&["Prio", "Class", "Account"], &trows)
            };
            let md = format!(
                "**🗄 Archive Quick (Preview)**\n\n{}\n\n{}\n\n_Archiving backs up accounts locally without deleting from 9Router._",
                field_grid(&[
                    ("Provider", hl(&prov)),
                    ("Targets", hl(&targets.len().to_string())),
                    ("Classes", "quota + denied".into()),
                ]),
                list_md
            );
            let kb = json!({
                "inline_keyboard": [
                    [
                        button("🗄 Apply Archive", "cmd:archive_quick_apply", "danger"),
                        button("↕ Reprio Plan", "cmd:ops_reprio", "primary"),
                    ],
                    [
                        button("📊 Pool", "nav:pool", "primary"),
                        button("🏠 Home", "nav:home", "primary"),
                    ],
                ]
            });
            self.jobs.append_audit(
                "archive.quick.plan",
                &prov,
                pool.exit_code,
                t0.elapsed().as_millis() as u64,
                &format!("targets={}", targets.len()),
            );
            self.deliver(work_mid, "archive-preview", &md, kb);
            return;
        }

        let label = format!("pxabuse_quick_{}", utc_compact_stamp());
        let res = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&[
                "archive",
                "inactive",
                "--from-pool",
                "--provider",
                &prov,
                "--classes",
                "quota,denied,auth_dead",
                "--label",
                &label,
                "--json",
            ]),
            120,
        );
        let batch_id = res
            .json
            .as_ref()
            .and_then(|v| {
                v.get("batch")
                    .or_else(|| v.get("label"))
                    .or_else(|| v.get("id"))
                    .and_then(Value::as_str)
            })
            .unwrap_or(&label)
            .to_string();
        let md = format!(
            "**🗄 Archive Quick · Applied**\n\n{}\n\n{}\n\nOptional 9Router delete: `/delete batch {}`\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Batch ID", format!("`{}`", md_text(&batch_id))),
                ("Exit Code", res.exit_code.to_string()),
            ]),
            details_block(
                "Archive JSON",
                &format!("```json\n{}\n```", compact_json(res.json.as_ref(), &res.output, 1400))
            ),
            md_text(&batch_id),
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs.append_audit(
            "archive.quick.apply",
            &prov,
            res.exit_code,
            t0.elapsed().as_millis() as u64,
            &batch_id,
        );
        self.deliver(work_mid, "archive-apply", &md, Self::kb_result());
    }

    fn exec_archive_quota(&self, msg_id: Option<i64>, apply: bool) {
        let prov = self.current_provider();
        let work_mid = self.show_working(
            msg_id,
            if apply { "Archive Apply" } else { "Archive Dry-Run" },
            &format!("Running archive on {prov}"),
        );
        let t0 = Instant::now();
        let mut args = svec(&[
            "archive",
            "inactive",
            "--from-pool",
            "--provider",
            &prov,
            "--classes",
            "quota,denied,auth_dead",
            "--json",
        ]);
        if !apply {
            args.push("--dry-run".to_string());
        }
        let res = run_af(&self.cfg.af_bin, &self.cfg.af_root, &args, 120);
        let hint = if apply {
            "Router deletion remains separate: `/delete batch <id>`"
        } else {
            "Use `/archive apply` to archive without deleting."
        };
        let md = format!(
            "**🗄 Archive · {}**\n\n{}\n\n{}\n\n{}",
            if apply { "Applied" } else { "Dry-Run" },
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Mode", if apply { "apply".into() } else { "dry-run".into() }),
                ("Exit Code", res.exit_code.to_string()),
            ]),
            details_block(
                "Result",
                &format!("```json\n{}\n```", compact_json(res.json.as_ref(), &res.output, 1400))
            ),
            hint
        );
        self.jobs.append_audit(
            if apply { "archive.apply" } else { "archive.dry" },
            &prov,
            res.exit_code,
            t0.elapsed().as_millis() as u64,
            "archive",
        );
        self.deliver(work_mid, "archive", &md, Self::kb_result());
    }

    fn exec_reprio_plan(&self, msg_id: Option<i64>) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Reprio Plan", &format!("Analyzing {prov} priorities"));
        let t0 = Instant::now();
        let res = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", &prov, "pool", "list", "--json"]),
            60,
        );
        let items = extract_pool_items(res.json.as_ref());
        let summary = summarize_pool_items(&items);
        let plan = plan_reprio_from_items(&summary.items);
        let changed_n = plan.iter().filter(|r| r.changed).count();

        let trows: Vec<Vec<String>> = plan
            .iter()
            .take(16)
            .map(|r| {
                vec![
                    format!("{}p{}→p{}", if r.changed { "*" } else { "" }, r.old_priority, r.new_priority),
                    format!("{}/{}", r.usable_class, r.usable_code),
                    truncate_str(&r.email, 26),
                ]
            })
            .collect();
        let table_md = if trows.is_empty() {
            "_Pool empty._".to_string()
        } else {
            table_markdown(&["Move", "Class", "Account"], &trows)
        };
        let md = format!(
            "**↕ Priority Reorder Plan**\n\n{}\n\n{}\n\n_Order: fresh → transient → other → quota → denied._",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Health", format!("{} {}", health_icon(summary.health), summary.health)),
                ("Changes", hl(&format!("{changed_n}/{}", plan.len()))),
                ("Breakdown", summary.counts_line.clone()),
            ]),
            table_md
        );
        let kb = json!({
            "inline_keyboard": [
                [
                    button("✅ Apply Reprio", "cmd:ops_reprio_apply", "danger"),
                    button("🗄 Archive Quick", "cmd:archive_quick", "primary"),
                ],
                [
                    button("📊 Pool", "nav:pool", "primary"),
                    button("🏠 Home", "nav:home", "primary"),
                ],
            ]
        });
        self.jobs.append_audit(
            "ops.reprio.plan",
            &prov,
            res.exit_code,
            t0.elapsed().as_millis() as u64,
            &format!("changes={changed_n}"),
        );
        self.deliver(work_mid, "reprio-plan", &md, kb);
    }

    fn exec_reprio_apply(&self, msg_id: Option<i64>, prov: &str) {
        let work_mid = self.show_working(msg_id, "Applying Priority Reorder", "Updating router priorities");
        let t0 = Instant::now();
        let Some(ref env_file) = self.cfg.router_env_file else {
            let md = "**❌ Router Not Configured**\n\nSet `ROUTER_ENV_FILE` in the service vault to enable priority updates.";
            self.tg.present(work_mid, md, Some(Self::kb_result()));
            return;
        };
        let res = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", prov, "pool", "list", "--json"]),
            60,
        );
        let items = extract_pool_items(res.json.as_ref());
        let summary = summarize_pool_items(&items);
        let plan = plan_reprio_from_items(&summary.items);
        let changes: Vec<_> = plan.into_iter().filter(|r| r.changed).collect();
        if changes.is_empty() {
            let md = format!(
                "**↕ Priority Reorder**\n\n{}\n\nNo priority changes needed.",
                field_grid(&[("Provider", hl(prov)), ("Changes", "0".into())])
            );
            self.tg.present(work_mid, &md, Some(Self::kb_result()));
            return;
        }
        match apply_reprio_router(env_file, self.cfg.router_backup_dir.as_deref(), &changes) {
            Ok(applied) => {
                let ok = applied.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let md = format!(
                    "**↕ Priority Reorder · Applied**\n\n{}\n\n{}\n\n_Completed in {} ms · {}_",
                    field_grid(&[
                        ("Provider", hl(prov)),
                        ("Status", if ok { hl("✅ OK") } else { hl("⚠️ Partial") }),
                        ("Updated", json_num_str(Some(&applied), "updated_ok")),
                        ("Failed", json_num_str(Some(&applied), "failed")),
                    ]),
                    details_block(
                        "Apply Summary",
                        &format!("```json\n{}\n```", compact_json(Some(&applied), "", 1200))
                    ),
                    t0.elapsed().as_millis(),
                    stamp_wib()
                );
                self.jobs.append_audit(
                    "ops.reprio.apply",
                    prov,
                    if ok { 0 } else { 1 },
                    t0.elapsed().as_millis() as u64,
                    "reprio.apply",
                );
                self.deliver(work_mid, "reprio-apply", &md, Self::kb_result());
            }
            Err(e) => {
                let md = format!(
                    "**❌ Priority Reorder Failed**\n\n{}",
                    field_grid(&[("Provider", prov.to_string()), ("Error", md_text(&e))])
                );
                self.jobs.append_audit(
                    "ops.reprio.apply",
                    prov,
                    1,
                    t0.elapsed().as_millis() as u64,
                    &e,
                );
                self.tg.present(work_mid, &md, Some(Self::kb_result()));
            }
        }
    }

    fn exec_post_generate(&self, msg_id: Option<i64>) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Post-Generate Playbook", "Scanning pending CPA + usable pool");
        let t0 = Instant::now();
        let (total_pending, items) = list_pending_cpa(self.cfg.pending_cpa_root.as_deref(), 12);
        let pool_res = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", &prov, "pool", "list", "--json"]),
            60,
        );
        let summary = summarize_pool_items(&extract_pool_items(pool_res.json.as_ref()));

        let selected = {
            let mut st = self.state.lock().unwrap();
            let valid: HashSet<String> = items
                .iter()
                .map(|it| it.email.to_ascii_lowercase())
                .collect();
            st.session.selected_emails.retain(|e| valid.contains(e));
            st.session.pending_items = items.clone();
            st.session.screen = "playbook_post".to_string();
            self.persist(&st);
            st.session.selected_emails.clone()
        };

        let list_md = if items.is_empty() {
            "_No pending CPA files found._".to_string()
        } else {
            let trows: Vec<Vec<String>> = items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    vec![
                        format!("#{}", i + 1),
                        truncate_str(&it.email, 30),
                        truncate_str(&it.farm, 16),
                    ]
                })
                .collect();
            table_markdown(&["#", "Email", "Farm"], &trows)
        };
        let hint = if items.is_empty() {
            "Run Smoke or Batch generation first to populate pending CPA files."
        } else {
            "`○` toggle multi-select · `▶ 1-Tap` import one · `Import Selected` batch (max 5)."
        };
        let md = format!(
            "**➡ Playbook · Post-Generate**\n\n{}\n\n{}\n\n{}\n\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Pending CPA", hl(&total_pending.to_string())),
                ("Selected", selected.len().to_string()),
                ("Pool Health", format!("{} {} ({})", health_icon(summary.health), summary.health, summary.counts_line)),
            ]),
            list_md,
            hint,
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        let kb = if items.is_empty() {
            json!({
                "inline_keyboard": [
                    [
                        button("⚡ Generate", "nav:generate", "success"),
                        button("📊 Status", "cmd:status", "primary"),
                    ],
                    [
                        button("📊 Pool", "nav:pool", "primary"),
                        button("🏠 Home", "nav:home", "primary"),
                    ],
                ]
            })
        } else {
            Self::kb_pending_import(&items, &selected)
        };
        self.jobs.append_audit(
            "playbook.post_generate",
            &prov,
            pool_res.exit_code,
            t0.elapsed().as_millis() as u64,
            &format!("pending={total_pending}"),
        );
        self.deliver(work_mid, "post-generate", &md, kb);
    }

    fn exec_cleanup_playbook(&self, msg_id: Option<i64>) {
        let prov = self.current_provider();
        let work_mid = self.show_working(msg_id, "Cleanup Stock Playbook", "Checking archive targets, reprio & proxy");
        let t0 = Instant::now();
        let pool = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", &prov, "pool", "list", "--json"]),
            60,
        );
        let proxy = run_af(&self.cfg.af_bin, &self.cfg.af_root, &svec(&["proxy", "status", "--json"]), 25);
        let summary = summarize_pool_items(&extract_pool_items(pool.json.as_ref()));
        let archive_targets: Vec<_> = summary
            .items
            .iter()
            .filter(|it| matches!(it.usable_class, "quota" | "denied"))
            .collect();
        let plan = plan_reprio_from_items(&summary.items);
        let reprio_changes = plan.iter().filter(|r| r.changed).count();
        let pj = proxy.json.as_ref();

        let target_rows: Vec<Vec<String>> = archive_targets
            .iter()
            .take(12)
            .map(|it| {
                vec![
                    format!("p{}", it.priority),
                    format!("{}/{}", it.usable_class, it.usable_code),
                    truncate_str(&it.email, 28),
                ]
            })
            .collect();
        let targets_md = if target_rows.is_empty() {
            "_No quota/denied accounts to archive._".to_string()
        } else {
            table_markdown(&["Prio", "Class", "Account"], &target_rows)
        };

        let md = format!(
            "**🧹 Playbook · Cleanup Stock (Preview)**\n\n{}\n\n**Archive Candidates**\n{}\n\nUse the buttons below to apply archive or reprio (confirmation required).\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Pool Health", format!("{} {} ({})", health_icon(summary.health), summary.health, summary.counts_line)),
                ("Archive Targets", hl(&archive_targets.len().to_string())),
                ("Reprio Changes", hl(&format!("{reprio_changes}/{}", plan.len()))),
                ("Proxy Ready", json_num_str(pj, "ready")),
            ]),
            targets_md,
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        let kb = json!({
            "inline_keyboard": [
                [
                    button("🗄 Archive Apply", "cmd:archive_quick_apply", "danger"),
                    button("↕ Reprio Apply", "cmd:ops_reprio_apply", "danger"),
                ],
                [
                    button("📊 Pool", "nav:pool", "primary"),
                    button("📊 Status", "cmd:status", "primary"),
                ],
                [button("🏠 Home", "nav:home", "primary")],
            ]
        });
        self.jobs.append_audit(
            "playbook.cleanup",
            &prov,
            pool.exit_code,
            t0.elapsed().as_millis() as u64,
            &format!("archive={} reprio={}", archive_targets.len(), reprio_changes),
        );
        self.deliver(work_mid, "cleanup", &md, kb);
    }

    fn exec_import_one(&self, msg_id: Option<i64>, email: &str, cpa_file: Option<&str>) {
        let prov = self.current_provider();
        if email.trim().is_empty() && cpa_file.map(|s| s.trim().is_empty()).unwrap_or(true) {
            self.tg.present(
                msg_id,
                "**📥 Import Usage**\n\nUse `/import <email>` or tap **1-Tap** in **Post-Generate**.",
                Some(Self::kb_result()),
            );
            return;
        }
        let work_mid = self.show_working(msg_id, "Importing CPA", &format!("Importing {email}"));
        let t0 = Instant::now();
        let mut args = svec(&["--provider", &prov, "pool", "import-one", "--via-pilot"]);
        if let Some(cpa) = cpa_file.filter(|s| !s.trim().is_empty()) {
            args.push("--cpa".to_string());
            args.push(cpa.to_string());
        } else {
            args.push("--identity".to_string());
            args.push(email.to_string());
        }
        args.push("--yes".to_string());
        args.push("--json".to_string());

        let res = run_af(&self.cfg.af_bin, &self.cfg.af_root, &args, 180);
        let outcome = normalize_import_result(&res, email);
        {
            let mut st = self.state.lock().unwrap();
            st.session.last_import = Some(LastImport {
                email: outcome.email.clone(),
                connection_id: outcome.connection_id.clone(),
                provider: prov.clone(),
                at: utc_iso_now(),
            });
            self.persist(&st);
        }
        let tv_str = match outcome.test_valid {
            Some(true) => "true (auth only ≠ usable)",
            Some(false) => "false (auth failed)",
            None => "unknown",
        };
        let mut fields = vec![
            ("Provider", hl(&prov)),
            ("Email", format!("`{}`", md_text(&outcome.email))),
            ("Status", if outcome.ok { hl("✅ Imported") } else { hl("❌ Failed") }),
            ("test_valid", tv_str.to_string()),
        ];
        if let Some(ref cid) = outcome.connection_id {
            fields.push(("Connection ID", format!("`{}`", md_text(cid))));
        }
        if let Some(ref err) = outcome.error {
            if !outcome.ok {
                fields.push(("Error", md_text(&truncate_str(err, 160))));
            }
        }
        let md = format!(
            "**📥 PX-Abuse · CPA Import**\n\n{}\n\n{}\n\n_Completed in {} ms · {}_",
            field_grid(&fields),
            details_block(
                "Raw Response",
                &format!("```json\n{}\n```", compact_json(res.json.as_ref(), &res.output, 1100))
            ),
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs.append_audit(
            "import",
            &outcome.email,
            if outcome.ok { 0 } else { 1 },
            t0.elapsed().as_millis() as u64,
            if outcome.ok { "ok" } else { "fail" },
        );
        self.deliver(work_mid, "import", &md, Self::kb_after_import());
    }

    fn exec_import_batch(&self, msg_id: Option<i64>) {
        let (prov, targets) = {
            let st = self.state.lock().unwrap();
            let prov = st
                .session
                .provider
                .clone()
                .unwrap_or_else(|| st.settings.default_provider.clone());
            let picked: Vec<PendingCpaItem> = st
                .session
                .pending_items
                .iter()
                .filter(|it| st.session.selected_emails.contains(&it.email.to_ascii_lowercase()))
                .take(BATCH_IMPORT_MAX)
                .cloned()
                .collect();
            (prov, picked)
        };
        if targets.is_empty() {
            self.tg.present(
                msg_id,
                "**📥 Multi-Import**\n\nNo accounts selected. Tap `○` next to pending accounts first.",
                Some(Self::kb_result()),
            );
            return;
        }
        let total = targets.len();
        let t0 = Instant::now();
        let mut mid = msg_id;
        let mut ok_n = 0usize;
        let mut fail_n = 0usize;
        let mut lines: Vec<String> = Vec::new();

        for (idx, it) in targets.iter().enumerate() {
            let prog_md = format!(
                "**📥 Multi-Import · In Progress ({}/{})**\n\n{}\n\n{}",
                idx + 1,
                total,
                field_grid(&[
                    ("Provider", hl(&prov)),
                    ("Progress", format!("`{}` ({}/{})", progress_bar(idx as f64 / total as f64, 10), idx, total)),
                    ("Current", format!("`{}`", md_text(&it.email))),
                    ("OK / Fail", format!("{ok_n} / {fail_n}")),
                ]),
                if lines.is_empty() {
                    "_Starting first import…_".to_string()
                } else {
                    lines.join("\n")
                }
            );
            mid = self.tg.present(mid, &prog_md, None);

            let args = svec(&[
                "--provider",
                &prov,
                "pool",
                "import-one",
                "--via-pilot",
                "--cpa",
                &it.file,
                "--yes",
                "--json",
            ]);
            let res = run_af(&self.cfg.af_bin, &self.cfg.af_root, &args, 180);
            let out = normalize_import_result(&res, &it.email);
            if out.ok {
                ok_n += 1;
            } else {
                fail_n += 1;
            }
            let tv = match out.test_valid {
                Some(true) => "test✓",
                Some(false) => "test✗",
                None => "test?",
            };
            let id_s = out
                .connection_id
                .as_deref()
                .map(|s| format!(" `{}`", truncate_str(s, 12)))
                .unwrap_or_default();
            lines.push(format!(
                "{}. {} {} `{}`{}",
                idx + 1,
                if out.ok { "✅" } else { "❌" },
                tv,
                md_text(&out.email),
                id_s
            ));
            if out.ok {
                let mut st = self.state.lock().unwrap();
                st.session.selected_emails.remove(&it.email.to_ascii_lowercase());
                st.session.last_import = Some(LastImport {
                    email: out.email,
                    connection_id: out.connection_id,
                    provider: prov.clone(),
                    at: utc_iso_now(),
                });
                self.persist(&st);
            }
        }

        let final_md = format!(
            "**📥 Multi-Import · Complete**\n\n{}\n\n{}\n\n_Note: `test_valid` ≠ usable · {} ms · {}_",
            field_grid(&[
                ("Provider", hl(&prov)),
                ("Processed", total.to_string()),
                ("Imported OK", hl(&ok_n.to_string())),
                ("Failed", fail_n.to_string()),
            ]),
            lines.join("\n"),
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs.append_audit(
            "import.many",
            &prov,
            if fail_n == 0 { 0 } else { 1 },
            t0.elapsed().as_millis() as u64,
            &format!("ok={ok_n} fail={fail_n}"),
        );
        self.deliver(mid, "multi-import", &final_md, Self::kb_after_import());
    }

    fn exec_chat_probe(&self, msg_id: Option<i64>, arg: Option<&str>) {
        let prov = self.current_provider();
        let (mut conn_id, email) = match arg.map(str::trim).filter(|s| !s.is_empty()) {
            Some(a) if a.contains('@') => (String::new(), a.to_ascii_lowercase()),
            Some(a) => (a.to_string(), String::new()),
            None => {
                let last = self.state.lock().unwrap().session.last_import.clone();
                match last {
                    Some(l) => (l.connection_id.unwrap_or_default(), l.email.to_ascii_lowercase()),
                    None => (String::new(), String::new()),
                }
            }
        };
        let work_mid = self.show_working(msg_id, "Chat-Probe", "Running pool auth test & class check");
        let t0 = Instant::now();
        let pool = run_af(
            &self.cfg.af_bin,
            &self.cfg.af_root,
            &svec(&["--provider", &prov, "pool", "list", "--json"]),
            60,
        );
        let summary = summarize_pool_items(&extract_pool_items(pool.json.as_ref()));
        let matched = summary.items.iter().find(|it| {
            (!conn_id.is_empty() && (it.id == conn_id || it.id.starts_with(&conn_id)))
                || (!email.is_empty() && it.email.eq_ignore_ascii_case(&email))
        });
        if conn_id.is_empty() {
            if let Some(m) = matched {
                conn_id = m.id.clone();
            }
        }

        let mut test_valid: Option<bool> = None;
        let mut test_note = "skipped (no matching connection ID)".to_string();
        let mut test_raw = String::new();
        if !conn_id.is_empty() {
            let tres = run_af(
                &self.cfg.af_bin,
                &self.cfg.af_root,
                &svec(&["--provider", &prov, "pool", "test", "--id", &conn_id, "--json"]),
                90,
            );
            test_valid = tres.json.as_ref().and_then(|v| {
                v.get("valid")
                    .or_else(|| v.get("ok"))
                    .or_else(|| v.get("test_valid"))
                    .or_else(|| v.pointer("/result/valid"))
                    .and_then(Value::as_bool)
            });
            test_note = format!(
                "{} · auth only · exit {}",
                match test_valid {
                    Some(true) => "valid",
                    Some(false) => "invalid",
                    None => "unknown",
                },
                tres.exit_code
            );
            test_raw = compact_json(tres.json.as_ref(), &tres.output, 800);
        }

        let honest_ok = matched.map(|m| m.usable_class == "fresh").unwrap_or(false)
            && test_valid == Some(true);
        let mut fields = vec![
            ("Provider", hl(&prov)),
            ("Connection", if conn_id.is_empty() { "—".into() } else { format!("`{}`", md_text(&conn_id)) }),
            ("Pool Test", test_note),
            (
                "Pool Class",
                matched
                    .map(|m| format!("{} · {} · p{}", m.usable_class, m.test_status, m.priority))
                    .unwrap_or_else(|| "not found in pool".to_string()),
            ),
            ("Honest Verdict", if honest_ok { hl("✅ Fresh + Auth OK") } else { hl("⚠️ Not Verified Fresh") }),
        ];
        if !email.is_empty() {
            fields.insert(2, ("Email", format!("`{}`", md_text(&email))));
        }
        let extra = if test_raw.is_empty() {
            String::new()
        } else {
            format!("\n\n{}", details_block("Test Response", &format!("```json\n{test_raw}\n```")))
        };
        let md = format!(
            "**🔎 PX-Abuse · Chat-Probe**\n\n{}{}\n\n_Note: auth `valid` ≠ usable. Usable requires class `fresh` with no quota/denied/locks._",
            field_grid(&fields),
            extra
        );
        self.jobs.append_audit(
            "chat.probe",
            if !email.is_empty() { &email } else { &conn_id },
            if honest_ok { 0 } else { 1 },
            t0.elapsed().as_millis() as u64,
            if honest_ok { "honest_ok" } else { "check" },
        );
        self.deliver(work_mid, "chat-probe", &md, Self::kb_after_import());
    }

    fn exec_delete_batch(&self, msg_id: Option<i64>, batch_id: &str, ids: &[String]) {
        let work_mid = self.show_working(
            msg_id,
            "Deleting Archived Batch",
            &format!("Removing {} connections from router", ids.len()),
        );
        let t0 = Instant::now();
        let mut ok_n = 0usize;
        let mut fail_n = 0usize;
        for id in ids {
            let res = run_af(
                &self.cfg.af_bin,
                &self.cfg.af_root,
                &svec(&["pool", "delete", "--id", id, "--yes", "--json"]),
                60,
            );
            let good = res
                .json
                .as_ref()
                .and_then(|v| v.get("ok").and_then(Value::as_bool))
                .unwrap_or(res.exit_code == 0);
            if good {
                ok_n += 1;
            } else {
                fail_n += 1;
            }
        }
        let marked = if fail_n == 0 {
            mark_batch_deleted(&self.cfg.af_root, batch_id, ok_n).is_ok()
        } else {
            false
        };
        let md = format!(
            "**🗑 Delete Batch · Complete**\n\n{}\n\n_Completed in {} ms · {}_",
            field_grid(&[
                ("Batch ID", format!("`{}`", md_text(batch_id))),
                ("Deleted OK", hl(&ok_n.to_string())),
                ("Failed", fail_n.to_string()),
                ("Manifest Marked", if marked { "yes".into() } else { "no".into() }),
            ]),
            t0.elapsed().as_millis(),
            stamp_wib()
        );
        self.jobs.append_audit(
            "delete.batch",
            batch_id,
            if fail_n == 0 { 0 } else { 1 },
            t0.elapsed().as_millis() as u64,
            &format!("ok={ok_n} fail={fail_n}"),
        );
        self.deliver(work_mid, "delete-batch", &md, Self::kb_result());
    }

    fn launch_bg_job(
        self: &Arc<Self>,
        msg_id: Option<i64>,
        provider: &str,
        mode: &str,
        target: u32,
        workers: u32,
        max_rounds: u32,
        domain: Option<&str>,
        dry: bool,
    ) {
        let args = smoke_args(provider, target, workers, max_rounds, domain, dry);
        let label = format!("generate {mode} {provider} t{target} w{workers}");
        let app_prog = Arc::clone(self);
        let app_done = Arc::clone(self);

        let started = self.jobs.start(
            self.cfg.af_bin.clone(),
            self.cfg.af_root.clone(),
            args,
            label.clone(),
            move |job, log_text| {
                app_prog.push_job_progress(&job, &log_text);
            },
            move |job, log_text| {
                app_done.push_job_done(&job, &log_text);
            },
        );
        match started {
            Ok(job) => {
                let mut fields = vec![
                    ("Provider", hl(provider)),
                    ("Mode", capitalize(mode)),
                    ("Target / Workers", format!("{target} / {workers} (rounds {max_rounds})")),
                    ("Job ID", format!("`{}`", job.id)),
                    ("Progress", hl("⏳ Live updates active")),
                ];
                if let Some(d) = domain.filter(|s| !s.is_empty()) {
                    fields.push(("Domain", d.to_string()));
                }
                let md = format!(
                    "**🚀 PX-Abuse · Job Launched**\n\n{}\n\nThis message updates automatically as output streams.\n_Started: {}_",
                    field_grid(&fields),
                    stamp_wib()
                );
                let sent_mid = self
                    .tg
                    .present(msg_id, &md, Some(Self::kb_job(&job.id, true)));
                if let Some(mid) = sent_mid {
                    self.jobs.set_progress_message(&job.id, mid);
                }
                self.jobs
                    .append_audit("job.start", provider, 0, 0, &format!("{} ({})", job.id, label));
            }
            Err(e) => {
                let md = format!(
                    "**❌ Job Launch Failed**\n\n{}",
                    field_grid(&[("Error", md_text(&e))])
                );
                self.tg.present(msg_id, &md, Some(Self::kb_result()));
            }
        }
    }

    fn push_job_progress(&self, job: &JobRecord, log_text: &str) {
        let Some(mid) = job.progress_message_id else {
            return;
        };
        let farm = parse_job_farm_summary(log_text);
        let farm_line = format_farm_summary_line(&farm).unwrap_or_else(|| "…".to_string());
        let elapsed = now_epoch().saturating_sub(job.started_epoch);
        let tail = extract_interesting_log_tail(log_text, 1400);
        let mut fields = vec![
            ("Job ID", format!("`{}`", job.id)),
            ("Status", hl("⏳ running")),
            ("Label", md_text(&job.label)),
            ("Elapsed", format_duration(elapsed)),
            ("Farm", farm_line),
        ];
        if farm.outcome == "zero_pass" {
            fields.push(("Note", "0 pass so far".into()));
        }
        let md = format!(
            "**⏳ Job Progress · `{}`**\n\n{}\n\n```\n{}\n```\n_Auto-refresh · {}_",
            job.id,
            field_grid(&fields),
            tail.replace("```", "'''"),
            stamp_wib()
        );
        self.tg.edit(mid, &md, Some(Self::kb_job(&job.id, true)));
    }

    fn push_job_done(&self, job: &JobRecord, log_text: &str) {
        let farm = parse_job_farm_summary(log_text);
        let farm_line = format_farm_summary_line(&farm);
        let notify_simple = self.state.lock().unwrap().settings.notify_mode == "simple";

        let badge = if job.status == "success" && farm.outcome == "zero_pass" {
            "⚠️ Completed · 0 Pass"
        } else if job.status == "success" {
            "✅ Completed"
        } else if job.status == "killed" {
            "🛑 Stopped"
        } else {
            "❌ Failed"
        };
        let mut fields = vec![
            ("Job ID", format!("`{}`", job.id)),
            ("Status", hl(badge)),
            ("Label", md_text(&job.label)),
            (
                "Exit Code",
                job.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".to_string()),
            ),
        ];
        if let Some(fl) = farm_line {
            fields.push(("Farm", fl));
        }
        if farm.outcome == "zero_pass" {
            fields.push(("Warning", hl("Exit 0 ≠ farm pass — check proxy/log")));
        }

        let body = if notify_simple {
            format!("**{} · Job `{}`**\n\n{}", badge, job.id, field_grid(&fields))
        } else {
            let tail = extract_interesting_log_tail(log_text, 1200);
            format!(
                "**{} · Job `{}`**\n\n{}\n\n{}\n\n_Finished: {}_",
                badge,
                job.id,
                field_grid(&fields),
                details_block("Output Tail", &format!("```\n{}\n```", tail.replace("```", "'''"))),
                stamp_wib()
            )
        };
        let kb = Self::kb_after_generate(&job.id, farm.outcome);
        self.tg.present(job.progress_message_id, &body, Some(kb));
        self.jobs.append_audit(
            "job.done",
            &job.id,
            job.exit_code.unwrap_or(1),
            now_epoch().saturating_sub(job.started_epoch) * 1000,
            badge,
        );
    }

    // ── confirmation requests ───────────────────────────────────────────────
    fn request_confirm(
        &self,
        msg_id: Option<i64>,
        title: &str,
        fields: &[(&str, String)],
        note: &str,
        action: ConfirmAction,
        danger: bool,
    ) {
        let token = self.create_confirm(action);
        let md = format!(
            "**⚠️ Confirm · {}**\n\n{}\n\n{}\n_Expires in 3 minutes._",
            md_text(title),
            field_grid(fields),
            note
        );
        self.tg
            .present(msg_id, &md, Some(Self::kb_confirm(&token, danger)));
    }

    fn execute_confirmed(self: &Arc<Self>, msg_id: Option<i64>, action: ConfirmAction) {
        match action {
            ConfirmAction::GenerateRun {
                provider,
                mode,
                target,
                workers,
                max_rounds,
                domain,
                ..
            } => {
                self.launch_bg_job(
                    msg_id,
                    &provider,
                    &mode,
                    target,
                    workers,
                    max_rounds,
                    domain.as_deref(),
                    false,
                );
            }
            ConfirmAction::SmokeRun {
                provider,
                target,
                workers,
                max_rounds,
            } => {
                self.launch_bg_job(msg_id, &provider, "smoke", target, workers, max_rounds, None, false);
            }
            ConfirmAction::ArchiveQuickApply { provider } => {
                let app = Arc::clone(self);
                thread::spawn(move || app.exec_archive_quick(msg_id, true, Some(&provider)));
            }
            ConfirmAction::ReprioApply { provider } => {
                let app = Arc::clone(self);
                thread::spawn(move || app.exec_reprio_apply(msg_id, &provider));
            }
            ConfirmAction::DeleteBatch { batch_id, ids } => {
                let app = Arc::clone(self);
                thread::spawn(move || app.exec_delete_batch(msg_id, &batch_id, &ids));
            }
        }
    }

    // ── playbook quick-generate prep ────────────────────────────────────────
    fn prepare_generate_playbook(&self, msg_id: Option<i64>, playbook_id: &str) {
        let (provider, mode, params) = {
            let mut st = self.state.lock().unwrap();
            let prov = st.settings.default_provider.clone();
            let mode = if playbook_id == "quick_smoke" {
                "smoke".to_string()
            } else {
                "batch".to_string()
            };
            let params = GenParams {
                workers: st.settings.default_workers,
                target: if playbook_id == "quick_smoke" {
                    st.settings.default_target_smoke
                } else {
                    st.settings.default_target_batch
                },
                rounds: if playbook_id == "quick_smoke" {
                    1
                } else {
                    st.settings.default_rounds
                },
                proxy_policy: st.settings.default_proxy_policy.clone(),
                auto_import: false,
                domain: None,
            };
            st.session.screen = "genpreview".to_string();
            st.session.provider = Some(prov.clone());
            st.session.action = Some(mode.clone());
            st.session.params = params.clone();
            st.session.awaiting = None;
            self.persist(&st);
            (prov, mode, params)
        };
        let prov_spec = get_provider(&provider);
        let label = prov_spec.map(|p| p.label).unwrap_or(&provider);
        let (req_w, eff_w, _) = effective_workers(prov_spec, params.workers);
        let title = if playbook_id == "quick_smoke" {
            "Quick Smoke"
        } else {
            "Fill Stock"
        };
        let md = format!(
            "**📚 Playbook · {title}**\n\n{}\n\nReview parameters, then launch or edit.",
            field_grid(&[
                ("Provider", hl(label)),
                ("Mode", capitalize(&mode)),
                ("Workers", format!("{req_w} → {eff_w}")),
                ("Target", params.target.to_string()),
                ("Rounds", params.rounds.to_string()),
                ("Proxy", format!("{} · UI only", params.proxy_policy)),
                ("Auto-Import", "OFF · UI only".into()),
            ])
        );
        let kb = json!({
            "inline_keyboard": [
                [
                    button(
                        if mode == "smoke" { "▶ Run Smoke" } else { "▶ Run Batch" },
                        "genrun:go",
                        "success"
                    ),
                    button("✏️ Edit Params", "gset:backparams", "primary"),
                ],
                [
                    button("📚 Playbooks", "nav:playbooks", "primary"),
                    button("🏠 Home", "nav:home", "primary"),
                ],
            ]
        });
        self.tg.present(msg_id, &md, Some(kb));
    }
}

// ── callback & message routers ──────────────────────────────────────────────
fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let chat_id = cb
        .pointer("/message/chat/id")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if !app.tg.is_owner(chat_id) {
        return;
    }
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);
    let data = cb.get("data").and_then(Value::as_str).unwrap_or("");

    // Confirmation gate
    if let Some(tok) = data.strip_prefix("cfm:yes:") {
        app.tg.ack(cb_id, "Confirmed…");
        match app.take_confirm(tok) {
            Some(act) => app.execute_confirmed(msg_id, act),
            None => {
                app.tg.present(
                    msg_id,
                    "**⏱ Confirmation Expired**\n\nPlease trigger the action again.",
                    Some(App::kb_home()),
                );
            }
        }
        return;
    }
    if let Some(tok) = data.strip_prefix("cfm:no:") {
        app.tg.ack(cb_id, "Cancelled");
        app.cancel_confirm(tok);
        app.render_home(msg_id);
        return;
    }

    // Navigation
    if let Some(screen) = data.strip_prefix("nav:") {
        app.tg.ack(cb_id, "");
        match screen {
            "home" => app.render_home(msg_id),
            "generate" => app.render_provider_picker(msg_id),
            "genactions" => app.render_gen_actions(msg_id),
            "playbooks" => app.render_playbooks(msg_id),
            "pool" => app.render_pool_menu(msg_id),
            "import" => app.render_import_menu(msg_id, false),
            "import_help" => app.render_import_menu(msg_id, true),
            "proxy" => app.render_proxy_menu(msg_id),
            "jobs" => app.render_jobs(msg_id),
            "settings" => app.render_settings(msg_id),
            "back" => {
                let cur = app.state.lock().unwrap().session.screen.clone();
                match cur.as_str() {
                    "genpreview" | "genparams" | "chip" | "await_custom" => app.render_gen_params(msg_id),
                    "genactions" => app.render_provider_picker(msg_id),
                    "settings_chip" => app.render_settings(msg_id),
                    _ => app.render_home(msg_id),
                }
            }
            _ => app.render_home(msg_id),
        }
        return;
    }

    // Playbooks
    if let Some(pb) = data.strip_prefix("pb:") {
        app.tg.ack(cb_id, "");
        match pb {
            "quick_smoke" | "fill_stock" => app.prepare_generate_playbook(msg_id, pb),
            "post_generate" => {
                let a = Arc::clone(app);
                thread::spawn(move || a.exec_post_generate(msg_id));
            }
            "cleanup" => {
                let a = Arc::clone(app);
                thread::spawn(move || a.exec_cleanup_playbook(msg_id));
            }
            _ => app.render_playbooks(msg_id),
        }
        return;
    }

    // Provider picker & Generate actions
    if let Some(rest) = data.strip_prefix("prov:generate:") {
        app.tg.ack(cb_id, "");
        if get_provider(rest).map(|p| p.ready).unwrap_or(false) {
            {
                let mut st = app.state.lock().unwrap();
                st.session.provider = Some(rest.to_string());
                app.persist(&st);
            }
            app.render_gen_actions(msg_id);
        } else {
            app.render_provider_picker(msg_id);
        }
        return;
    }
    if let Some(pid) = data.strip_prefix("provsoon:") {
        app.tg.ack(cb_id, &format!("{pid}: adapter coming soon"));
        return;
    }
    if let Some(rest) = data.strip_prefix("gen:") {
        app.tg.ack(cb_id, "");
        if let Some((pid, mode)) = rest.split_once(':') {
            {
                let mut st = app.state.lock().unwrap();
                let target = match mode {
                    "smoke" | "dry" => st.settings.default_target_smoke,
                    "farm" => st.settings.default_target_batch.max(24),
                    _ => st.settings.default_target_batch,
                };
                st.session.provider = Some(pid.to_string());
                st.session.action = Some(mode.to_string());
                st.session.params = GenParams {
                    workers: st.settings.default_workers,
                    target,
                    rounds: st.settings.default_rounds,
                    proxy_policy: st.settings.default_proxy_policy.clone(),
                    auto_import: st.settings.auto_import_default,
                    domain: None,
                };
                st.session.awaiting = None;
                app.persist(&st);
            }
            app.render_gen_params(msg_id);
        }
        return;
    }

    // Generate parameter chips
    if let Some(key) = data.strip_prefix("gset:") {
        app.tg.ack(cb_id, "");
        match key {
            "preview" => app.render_gen_preview(msg_id),
            "backparams" => app.render_gen_params(msg_id),
            "autoimport" => {
                {
                    let mut st = app.state.lock().unwrap();
                    st.session.params.auto_import = !st.session.params.auto_import;
                    app.persist(&st);
                }
                app.render_gen_params(msg_id);
            }
            "workers" => show_chip_picker(
                app,
                msg_id,
                "Workers",
                "Select requested worker count (Grok pilot bridge uses fixed 6).",
                "gw",
                &[("1", "1"), ("2", "2"), ("3", "3"), ("6", "6"), ("12", "12"), ("Custom", "custom")],
                "gset:backparams",
            ),
            "target" => {
                let mode = app.state.lock().unwrap().session.action.clone().unwrap_or_default();
                if mode == "smoke" {
                    show_chip_picker(
                        app,
                        msg_id,
                        "Smoke Target",
                        "Select target account count.",
                        "gt",
                        &[("1", "1"), ("3", "3"), ("6", "6"), ("Custom", "custom")],
                        "gset:backparams",
                    );
                } else {
                    show_chip_picker(
                        app,
                        msg_id,
                        "Batch / Farm Target",
                        "Select target account count.",
                        "gt",
                        &[("6", "6"), ("12", "12"), ("24", "24"), ("48", "48"), ("Custom", "custom")],
                        "gset:backparams",
                    );
                }
            }
            "rounds" => show_chip_picker(
                app,
                msg_id,
                "Max Rounds",
                "Select maximum registration rounds.",
                "gr",
                &[("1", "1"), ("2", "2"), ("3", "3"), ("Custom", "custom")],
                "gset:backparams",
            ),
            "proxy" => show_chip_picker(
                app,
                msg_id,
                "Proxy Policy (UI Only)",
                "Stored in session/settings state.",
                "gp",
                &[("winners · UI", "winners"), ("ready · UI", "ready"), ("sticky · UI", "sticky")],
                "gset:backparams",
            ),
            "domain" => {
                let mut opts: Vec<(String, String)> = vec![("any".to_string(), "any".to_string())];
                for (idx, d) in app.cfg.gen_domains.iter().enumerate() {
                    opts.push((d.clone(), idx.to_string()));
                }
                let refs: Vec<(&str, &str)> = opts.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
                show_chip_picker(
                    app,
                    msg_id,
                    "Target Domain",
                    "Passed to `--domain` for domain-aware providers.",
                    "gd",
                    &refs,
                    "gset:backparams",
                );
            }
            _ => app.render_gen_params(msg_id),
        }
        return;
    }

    if let Some((kind, val)) = data.split_once(':') {
        if matches!(kind, "gw" | "gt" | "gr" | "gp" | "gd") {
            app.tg.ack(cb_id, "");
            if val == "custom" {
                let field = match kind {
                    "gw" => "custom_workers",
                    "gt" => "custom_target",
                    _ => "custom_rounds",
                };
                {
                    let mut st = app.state.lock().unwrap();
                    st.session.awaiting = Some(field.to_string());
                    st.session.screen = "await_custom".to_string();
                    app.persist(&st);
                }
                let md = format!(
                    "**✏️ Custom Input**\n\n{}\n\nSend a positive integer in chat, or cancel.",
                    field_grid(&[("Field", field.trim_start_matches("custom_").to_string())])
                );
                app.tg.present(
                    msg_id,
                    &md,
                    Some(json!({"inline_keyboard": [[button("❌ Cancel", "gset:backparams", "danger")]]})),
                );
                return;
            }
            {
                let mut st = app.state.lock().unwrap();
                match kind {
                    "gw" => st.session.params.workers = val.parse().unwrap_or(6).clamp(1, 24),
                    "gt" => st.session.params.target = val.parse().unwrap_or(6).clamp(1, 200),
                    "gr" => st.session.params.rounds = val.parse().unwrap_or(1).clamp(1, 20),
                    "gp" => st.session.params.proxy_policy = val.to_string(),
                    "gd" => {
                        st.session.params.domain = if val == "any" {
                            None
                        } else {
                            val.parse::<usize>()
                                .ok()
                                .and_then(|i| app.cfg.gen_domains.get(i).cloned())
                        };
                    }
                    _ => {}
                }
                app.persist(&st);
            }
            app.render_gen_params(msg_id);
            return;
        }
    }

    // Generate Run
    if data == "genrun:go" {
        app.tg.ack(cb_id, "");
        let (provider, mode, params) = {
            let mut st = app.state.lock().unwrap();
            let r = App::ensure_gen_params(&mut st);
            app.persist(&st);
            r
        };
        let prov_spec = get_provider(&provider);
        let domain_aware = prov_spec.map(|p| p.domain_aware).unwrap_or(false);
        let domain = if domain_aware { params.domain.clone() } else { None };
        let (req_w, eff_w, _) = effective_workers(prov_spec, params.workers);

        if mode == "dry" {
            app.launch_bg_job(
                msg_id,
                &provider,
                "dry",
                params.target,
                req_w,
                params.rounds,
                domain.as_deref(),
                true,
            );
            return;
        }
        let mut fields = vec![
            ("Provider", hl(&provider)),
            ("Mode", capitalize(&mode)),
            ("Workers", format!("{req_w} → {eff_w}")),
            ("Target", params.target.to_string()),
            ("Rounds", params.rounds.to_string()),
        ];
        if mode == "farm" {
            fields.push(("Est. Slots", format!("~{}", params.target * params.rounds)));
        }
        fields.push(("Proxy", format!("{} · UI only", params.proxy_policy)));
        let note = if mode == "farm" {
            "⚠️ **Mass farm run** — verify proxy & target before confirming."
        } else {
            "Confirm to launch background generation job."
        };
        app.request_confirm(
            msg_id,
            &format!("Generate · {provider} · {mode}"),
            &fields,
            note,
            ConfirmAction::GenerateRun {
                provider,
                mode: mode.clone(),
                target: params.target,
                workers: req_w,
                max_rounds: params.rounds,
                proxy_policy: params.proxy_policy,
                auto_import: params.auto_import,
                domain,
            },
            mode == "farm",
        );
        return;
    }

    // Settings
    if let Some(key) = data.strip_prefix("set:") {
        app.tg.ack(cb_id, "");
        match key {
            "notify" => {
                {
                    let mut st = app.state.lock().unwrap();
                    st.settings.notify_mode = if st.settings.notify_mode == "simple" {
                        "full".to_string()
                    } else {
                        "simple".to_string()
                    };
                    app.persist(&st);
                }
                app.render_settings(msg_id);
            }
            "autoimport" => {
                {
                    let mut st = app.state.lock().unwrap();
                    st.settings.auto_import_default = !st.settings.auto_import_default;
                    app.persist(&st);
                }
                app.render_settings(msg_id);
            }
            "provider" => {
                let btns: Vec<Value> = PROVIDERS
                    .iter()
                    .map(|p| {
                        if p.ready {
                            button(p.label, &format!("setp:{}", p.id), "success")
                        } else {
                            button(&format!("{} · Soon", p.label), &format!("provsoon:{}", p.id), "")
                        }
                    })
                    .collect();
                let mut kb_rows = rows(btns).as_array().cloned().unwrap_or_default();
                kb_rows.push(json!([button("↩ Settings", "nav:settings", "primary")]));
                app.tg.present(
                    msg_id,
                    "**⚙️ Default Provider**\n\nSelect a ready provider.",
                    Some(json!({"inline_keyboard": kb_rows})),
                );
            }
            "workers" => show_chip_picker(
                app,
                msg_id,
                "Default Workers",
                "Select default worker count.",
                "sw",
                &[("1", "1"), ("2", "2"), ("3", "3"), ("6", "6"), ("12", "12")],
                "nav:settings",
            ),
            "batch" => show_chip_picker(
                app,
                msg_id,
                "Default Batch Target",
                "Select default batch target.",
                "sb",
                &[("6", "6"), ("12", "12"), ("24", "24"), ("48", "48")],
                "nav:settings",
            ),
            "smoke" => show_chip_picker(
                app,
                msg_id,
                "Default Smoke Target",
                "Select default smoke target.",
                "ss",
                &[("1", "1"), ("3", "3"), ("6", "6")],
                "nav:settings",
            ),
            "proxy" => show_chip_picker(
                app,
                msg_id,
                "Default Proxy Policy",
                "Select default proxy policy.",
                "sp",
                &[("winners", "winners"), ("ready", "ready"), ("sticky", "sticky")],
                "nav:settings",
            ),
            _ => app.render_settings(msg_id),
        }
        return;
    }

    if let Some((kind, val)) = data.split_once(':') {
        if matches!(kind, "setp" | "sw" | "sb" | "ss" | "sp") {
            app.tg.ack(cb_id, "Saved");
            {
                let mut st = app.state.lock().unwrap();
                match kind {
                    "setp" => st.settings.default_provider = val.to_string(),
                    "sw" => st.settings.default_workers = val.parse().unwrap_or(6).clamp(1, 24),
                    "sb" => st.settings.default_target_batch = val.parse().unwrap_or(12).clamp(1, 200),
                    "ss" => st.settings.default_target_smoke = val.parse().unwrap_or(6).clamp(1, 48),
                    "sp" => st.settings.default_proxy_policy = val.to_string(),
                    _ => {}
                }
                app.persist(&st);
            }
            app.render_settings(msg_id);
            return;
        }
    }

    // Pending CPA multi-select & 1-tap import
    if let Some(idx_str) = data.strip_prefix("impsel:") {
        app.tg.ack(cb_id, "");
        if let Ok(idx) = idx_str.parse::<usize>() {
            let (items, selected) = {
                let mut st = app.state.lock().unwrap();
                if let Some(it) = st.session.pending_items.get(idx).cloned() {
                    let key = it.email.to_ascii_lowercase();
                    if st.session.selected_emails.contains(&key) {
                        st.session.selected_emails.remove(&key);
                    } else {
                        st.session.selected_emails.insert(key);
                    }
                    app.persist(&st);
                }
                (st.session.pending_items.clone(), st.session.selected_emails.clone())
            };
            render_pending_selection_inplace(app, msg_id, &items, &selected);
        }
        return;
    }
    if let Some(idx_str) = data.strip_prefix("imp:") {
        app.tg.ack(cb_id, "Importing…");
        if let Ok(idx) = idx_str.parse::<usize>() {
            let item = app.state.lock().unwrap().session.pending_items.get(idx).cloned();
            if let Some(it) = item {
                let a = Arc::clone(app);
                thread::spawn(move || a.exec_import_one(msg_id, &it.email, Some(&it.file)));
            }
        }
        return;
    }
    if let Some(action) = data.strip_prefix("impbatch:") {
        match action {
            "top3" => {
                app.tg.ack(cb_id, "Selected first 3");
                let (items, selected) = {
                    let mut st = app.state.lock().unwrap();
                    let picks: Vec<String> = st
                        .session
                        .pending_items
                        .iter()
                        .take(3)
                        .map(|i| i.email.to_ascii_lowercase())
                        .collect();
                    for p in picks {
                        st.session.selected_emails.insert(p);
                    }
                    app.persist(&st);
                    (st.session.pending_items.clone(), st.session.selected_emails.clone())
                };
                render_pending_selection_inplace(app, msg_id, &items, &selected);
            }
            "clear" => {
                app.tg.ack(cb_id, "Cleared");
                let (items, selected) = {
                    let mut st = app.state.lock().unwrap();
                    st.session.selected_emails.clear();
                    app.persist(&st);
                    (st.session.pending_items.clone(), st.session.selected_emails.clone())
                };
                render_pending_selection_inplace(app, msg_id, &items, &selected);
            }
            "run" => {
                app.tg.ack(cb_id, "Starting batch import…");
                let a = Arc::clone(app);
                thread::spawn(move || a.exec_import_batch(msg_id));
            }
            _ => {
                app.tg.ack(cb_id, "");
            }
        }
        return;
    }

    // Probe
    if data == "probe:last" {
        app.tg.ack(cb_id, "Probing…");
        let a = Arc::clone(app);
        thread::spawn(move || a.exec_chat_probe(msg_id, None));
        return;
    }

    // Jobs
    if let Some(jid) = data.strip_prefix("jobopen:") {
        app.tg.ack(cb_id, "");
        app.render_job_detail(msg_id, jid);
        return;
    }
    if let Some(jid) = data.strip_prefix("jobstop:") {
        let ok = app.jobs.stop(jid);
        app.tg.ack(cb_id, if ok { "SIGTERM sent" } else { "Job not running" });
        app.render_job_detail(msg_id, jid);
        return;
    }

    // Direct commands
    if let Some(cmd) = data.strip_prefix("cmd:") {
        app.tg.ack(cb_id, "");
        let a = Arc::clone(app);
        match cmd {
            "help" => app.render_help(msg_id),
            "status" => {
                thread::spawn(move || a.exec_status(msg_id));
            }
            "usable" => {
                thread::spawn(move || a.exec_usable(msg_id, false));
            }
            "pool" => {
                thread::spawn(move || a.exec_pool(msg_id));
            }
            "proxy" => {
                thread::spawn(move || a.exec_proxy(msg_id, false));
            }
            "proxy_sync" => {
                thread::spawn(move || a.exec_proxy(msg_id, true));
            }
            "archive_quick" => {
                thread::spawn(move || a.exec_archive_quick(msg_id, false, None));
            }
            "archive_quick_apply" => {
                let prov = app.current_provider();
                app.request_confirm(
                    msg_id,
                    "Archive Quick Apply",
                    &[
                        ("Provider", hl(&prov)),
                        ("Scope", "quota + denied + auth_dead".into()),
                        ("Router Delete", "Not yet (separate /delete batch)".into()),
                    ],
                    "Archive all quota/denied accounts from the pool?",
                    ConfirmAction::ArchiveQuickApply { provider: prov },
                    true,
                );
            }
            "ops_reprio" => {
                thread::spawn(move || a.exec_reprio_plan(msg_id));
            }
            "ops_reprio_apply" => {
                let prov = app.current_provider();
                app.request_confirm(
                    msg_id,
                    "Apply Priority Reorder",
                    &[
                        ("Provider", hl(&prov)),
                        ("Order", "fresh/transient first · quota/denied last".into()),
                        ("Backup", "Pre-reprio snapshot saved automatically".into()),
                    ],
                    "Apply new priorities to 9Router?",
                    ConfirmAction::ReprioApply { provider: prov },
                    true,
                );
            }
            "ops_postsmoke" => {
                thread::spawn(move || a.exec_post_generate(msg_id));
            }
            _ => app.render_home(msg_id),
        }
        return;
    }

    app.tg.ack(cb_id, "");
    app.render_home(msg_id);
}

fn show_chip_picker(
    app: &App,
    msg_id: Option<i64>,
    title: &str,
    desc: &str,
    prefix: &str,
    options: &[(&str, &str)],
    back_cb: &str,
) {
    {
        let mut st = app.state.lock().unwrap();
        st.session.screen = if back_cb == "nav:settings" {
            "settings_chip".to_string()
        } else {
            "chip".to_string()
        };
        app.persist(&st);
    }
    let btns: Vec<Value> = options
        .iter()
        .map(|(lbl, val)| button(lbl, &format!("{prefix}:{val}"), ""))
        .collect();
    let mut kb_rows = rows(btns).as_array().cloned().unwrap_or_default();
    kb_rows.push(json!([
        button("↩ Back", back_cb, "primary"),
        button("🏠 Home", "nav:home", "primary"),
    ]));
    let md = format!("**🎚 {title}**\n\n{desc}");
    app.tg
        .present(msg_id, &md, Some(json!({"inline_keyboard": kb_rows})));
}

fn render_pending_selection_inplace(
    app: &App,
    msg_id: Option<i64>,
    items: &[PendingCpaItem],
    selected: &HashSet<String>,
) {
    let prov = app.current_provider();
    let list_md = if items.is_empty() {
        "_No pending CPA files._".to_string()
    } else {
        let trows: Vec<Vec<String>> = items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let mark = if selected.contains(&it.email.to_ascii_lowercase()) {
                    "✓"
                } else {
                    "○"
                };
                vec![
                    format!("{mark} #{}", i + 1),
                    truncate_str(&it.email, 28),
                    truncate_str(&it.farm, 16),
                ]
            })
            .collect();
        table_markdown(&["#", "Email", "Farm"], &trows)
    };
    let md = format!(
        "**➡ Post-Generate · Pending CPA**\n\n{}\n\n{}\n\nTap `○` to toggle · `▶ 1-Tap` to import one · `Import Selected` for batch (max {BATCH_IMPORT_MAX}).",
        field_grid(&[
            ("Provider", hl(&prov)),
            ("Pending", items.len().to_string()),
            ("Selected", hl(&selected.len().to_string())),
        ]),
        list_md
    );
    app.tg
        .present(msg_id, &md, Some(App::kb_pending_import(items, selected)));
}

fn handle_message(app: &Arc<App>, msg: &Value) {
    let chat_id = msg.pointer("/chat/id").and_then(Value::as_i64).unwrap_or(0);
    if !app.tg.is_owner(chat_id) {
        return;
    }
    let text = msg.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() {
        return;
    }

    if !text.starts_with('/') {
        let awaiting = app.state.lock().unwrap().session.awaiting.clone();
        if let Some(field) = awaiting {
            match text.parse::<u32>() {
                Ok(n) if n > 0 => {
                    {
                        let mut st = app.state.lock().unwrap();
                        st.session.awaiting = None;
                        match field.as_str() {
                            "custom_workers" => st.session.params.workers = n.clamp(1, 24),
                            "custom_target" => st.session.params.target = n.clamp(1, 200),
                            "custom_rounds" => st.session.params.rounds = n.clamp(1, 20),
                            _ => {}
                        }
                        app.persist(&st);
                    }
                    app.render_gen_params(None);
                }
                _ => {
                    app.tg.send(
                        "**❌ Invalid Number**\n\nEnter a positive integer, or tap Cancel.",
                        Some(json!({"inline_keyboard": [[button("❌ Cancel", "gset:backparams", "danger")]]})),
                    );
                }
            }
            return;
        }
        app.render_home(None);
        return;
    }

    let parts: Vec<&str> = text.split_whitespace().collect();
    let head = parts[0].split('@').next().unwrap_or("").to_ascii_lowercase();
    let args = &parts[1..];
    let a = Arc::clone(app);

    match head.as_str() {
        "/start" | "/menu" => app.render_home(None),
        "/help" => app.render_help(None),
        "/settings" => app.render_settings(None),
        "/playbooks" | "/playbook" => app.render_playbooks(None),
        "/status" => {
            thread::spawn(move || a.exec_status(None));
        }
        "/usable" => {
            let full = args.first().map(|s| s.eq_ignore_ascii_case("full")).unwrap_or(false);
            thread::spawn(move || a.exec_usable(None, full));
        }
        "/pool" => {
            thread::spawn(move || a.exec_pool(None));
        }
        "/proxy" => {
            let sync = args.first().map(|s| s.eq_ignore_ascii_case("sync")).unwrap_or(false);
            thread::spawn(move || a.exec_proxy(None, sync));
        }
        "/archive" => {
            if args.first().map(|s| s.eq_ignore_ascii_case("quick")).unwrap_or(false) {
                let apply = args.get(1).map(|s| s.eq_ignore_ascii_case("apply")).unwrap_or(false);
                if apply {
                    let prov = app.current_provider();
                    app.request_confirm(
                        None,
                        "Archive Quick Apply",
                        &[
                            ("Provider", hl(&prov)),
                            ("Scope", "quota + denied + auth_dead".into()),
                        ],
                        "Archive all quota/denied accounts from the pool?",
                        ConfirmAction::ArchiveQuickApply { provider: prov },
                        true,
                    );
                } else {
                    thread::spawn(move || a.exec_archive_quick(None, false, None));
                }
            } else {
                let apply = args.first().map(|s| s.eq_ignore_ascii_case("apply")).unwrap_or(false);
                thread::spawn(move || a.exec_archive_quota(None, apply));
            }
        }
        "/import" => {
            let email = args.first().copied().unwrap_or("").to_string();
            thread::spawn(move || a.exec_import_one(None, &email, None));
        }
        "/probe" => {
            let target = args.first().map(|s| s.to_string());
            thread::spawn(move || a.exec_chat_probe(None, target.as_deref()));
        }
        "/jobs" => app.render_jobs(None),
        "/job" => {
            if let Some(jid) = args.first() {
                app.render_job_detail(None, jid);
            } else {
                app.render_jobs(None);
            }
        }
        "/stop" | "/kill" => {
            if let Some(jid) = args.first() {
                let ok = app.jobs.stop(jid);
                let md = format!(
                    "**🛑 Stop Job**\n\n{}",
                    field_grid(&[
                        ("Job ID", format!("`{}`", md_text(jid))),
                        ("Result", if ok { hl("SIGTERM sent") } else { "Not running / not found".into() }),
                    ])
                );
                app.tg.send(&md, Some(App::kb_result()));
            } else {
                app.tg.send("Usage: `/stop <job_id>`", Some(App::kb_result()));
            }
        }
        "/smoke" => {
            let live = args.first().map(|s| s.eq_ignore_ascii_case("run")).unwrap_or(false);
            let (prov, target, workers, max_rounds) = {
                let st = app.state.lock().unwrap();
                (
                    app.current_provider(),
                    st.settings.default_target_smoke,
                    st.settings.default_workers,
                    st.settings.default_rounds,
                )
            };
            if live {
                app.request_confirm(
                    None,
                    "Smoke Live Run",
                    &[
                        ("Provider", hl(&prov)),
                        ("Target", target.to_string()),
                        ("Workers", workers.to_string()),
                        ("Rounds", max_rounds.to_string()),
                    ],
                    "Launch live smoke run using current default settings?",
                    ConfirmAction::SmokeRun {
                        provider: prov,
                        target,
                        workers,
                        max_rounds,
                    },
                    false,
                );
            } else {
                app.launch_bg_job(None, &prov, "smoke-dry", target, workers, max_rounds, None, true);
            }
        }
        "/ops" => {
            let sub = args.first().copied().unwrap_or("daily").to_ascii_lowercase();
            match sub.as_str() {
                "daily" => {
                    thread::spawn(move || {
                        a.exec_proxy(None, true);
                        a.exec_usable(None, false);
                    });
                }
                "cleanup" => {
                    thread::spawn(move || a.exec_cleanup_playbook(None));
                }
                "postsmoke" => {
                    thread::spawn(move || a.exec_post_generate(None));
                }
                "reprio" => {
                    let apply = args.iter().any(|x| x.eq_ignore_ascii_case("apply"));
                    if apply {
                        let prov = app.current_provider();
                        app.request_confirm(
                            None,
                            "Apply Priority Reorder",
                            &[
                                ("Provider", hl(&prov)),
                                ("Order", "fresh/transient first · quota/denied last".into()),
                            ],
                            "Apply new priorities to 9Router?",
                            ConfirmAction::ReprioApply { provider: prov },
                            true,
                        );
                    } else {
                        thread::spawn(move || a.exec_reprio_plan(None));
                    }
                }
                _ => {
                    app.tg.send(
                        "**⚙️ Ops Subcommands**\n\n`/ops daily` · `/ops cleanup` · `/ops postsmoke` · `/ops reprio [apply]`",
                        Some(App::kb_result()),
                    );
                }
            }
        }
        "/delete" => {
            if args.first().map(|s| s.eq_ignore_ascii_case("batch")).unwrap_or(false) {
                if let Some(batch_id) = args.get(1) {
                    if !is_safe_batch_id(batch_id) {
                        app.tg.send("**❌ Invalid Batch ID**", Some(App::kb_result()));
                        return;
                    }
                    match load_batch_ids(&app.cfg.af_root, batch_id) {
                        Ok(man) => {
                            if man.deleted_from_pool {
                                let md = format!(
                                    "**ℹ️ Batch Already Deleted**\n\n{}",
                                    field_grid(&[
                                        ("Batch ID", format!("`{}`", md_text(&man.batch_id))),
                                        ("IDs", man.ids.len().to_string()),
                                        ("Status", "Already marked deleted_from_pool".into()),
                                    ])
                                );
                                app.tg.send(&md, Some(App::kb_result()));
                            } else {
                                app.request_confirm(
                                    None,
                                    "DELETE From 9Router",
                                    &[
                                        ("Batch ID", format!("`{}`", md_text(&man.batch_id))),
                                        ("Connections", hl(&man.ids.len().to_string())),
                                        ("Effect", "Permanently deletes connections from 9Router".into()),
                                    ],
                                    "⚠️ Verify the local archive exists before confirming deletion.",
                                    ConfirmAction::DeleteBatch {
                                        batch_id: man.batch_id,
                                        ids: man.ids,
                                    },
                                    true,
                                );
                            }
                        }
                        Err(e) => {
                            app.tg.send(
                                &format!("**❌ Delete Batch Failed**\n\n{}", md_text(&e)),
                                Some(App::kb_result()),
                            );
                        }
                    }
                    return;
                }
            }
            thread::spawn(move || {
                let res = run_af(&a.cfg.af_bin, &a.cfg.af_root, &svec(&["archive", "list", "--json"]), 30);
                let md = format!(
                    "**🗑 Delete Archived Batch**\n\n{}\n\n{}",
                    field_grid(&[("Usage", "`/delete batch <batch_id>`".into())]),
                    details_block(
                        "Available Archive Batches",
                        &format!("```json\n{}\n```", compact_json(res.json.as_ref(), &res.output, 1400))
                    )
                );
                a.deliver(None, "delete-usage", &md, App::kb_result());
            });
        }
        _ => app.render_help(None),
    }
}

// ── formatting helpers ──────────────────────────────────────────────────────
fn svec(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn health_icon(h: &str) -> &'static str {
    match h {
        "healthy" => "🟢",
        "watch" => "🟡",
        "degraded" => "🟠",
        "attention" => "🔴",
        _ => "⚪",
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

fn json_num_str(v: Option<&Value>, key: &str) -> String {
    v.and_then(|obj| obj.get(key))
        .map(|val| match val {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => "?".to_string(),
        })
        .unwrap_or_else(|| "?".to_string())
}

fn compact_json(val: Option<&Value>, fallback: &str, max_chars: usize) -> String {
    let text = match val {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| fallback.to_string()),
        None => fallback.to_string(),
    };
    truncate_str(&text, max_chars)
}

fn format_usable_report_brief(data: Option<&Value>) -> String {
    let Some(d) = data else {
        return "_Report unavailable._".to_string();
    };
    if d.get("ok").and_then(Value::as_bool) == Some(false) {
        return d
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Usable report failed")
            .to_string();
    }
    let health = d
        .get("health")
        .or_else(|| d.pointer("/view/health"))
        .and_then(Value::as_str)
        .unwrap_or("n/a");
    let total = json_num_str(Some(d), "total");
    let counts = d
        .get("counts_line")
        .or_else(|| d.pointer("/view/counts_line"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            d.get("summary")
                .or_else(|| d.pointer("/view/summary"))
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k} {v}"))
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .unwrap_or_else(|| "empty".to_string())
        });
    let mut lines = vec![
        format!("- **Health:** `{health}` · **Total:** `{total}`"),
        format!("- **Counts:** {counts}"),
    ];
    if let Some(scanned) = d.get("scanned_at").and_then(Value::as_str) {
        lines.push(format!("- **Scanned:** `{scanned}`"));
    }
    lines.join("\n")
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-abuse {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                let af_ok = cfg.af_bin.exists();
                let root_ok = cfg.af_root.is_dir();
                let cpa_ok = cfg
                    .pending_cpa_root
                    .as_ref()
                    .map(|p| p.is_dir())
                    .unwrap_or(false);
                println!(
                    "OK: px-abuse {VERSION} | af_bin={} (exists={af_ok}) | af_root={} (dir={root_ok}) | pending_cpa_root_ok={cpa_ok} | jobs_dir={}",
                    cfg.af_bin.display(),
                    cfg.af_root.display(),
                    cfg.jobs_dir.display()
                );
                if !af_ok || !root_ok {
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

    if let Some(parent) = cfg.state_file.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let app = App::new(cfg);
    log_info(&format!(
        "PX-Abuse v{VERSION} (Rust) started | af_bin={} | af_root={}",
        app.cfg.af_bin.display(),
        app.cfg.af_root.display()
    ));

    app.tg.set_my_commands(&[
        ("start", "Open PX-Abuse dashboard"),
        ("status", "Usable pool & proxy overview"),
        ("usable", "Scan usable accounts & report"),
        ("pool", "Inspect pool & priority order"),
        ("proxy", "Proxy pool status or sync"),
        ("playbooks", "Quick operator playbooks"),
        ("import", "Import pending CPA account"),
        ("probe", "Chat-probe imported connection"),
        ("archive", "Archive quota/denied accounts"),
        ("delete", "Delete archived batch from router"),
        ("smoke", "Run smoke dry-run or live job"),
        ("ops", "Daily / cleanup / postsmoke / reprio"),
        ("jobs", "List background jobs"),
        ("job", "Inspect job log by ID"),
        ("stop", "Stop a running background job"),
        ("settings", "Configure default parameters"),
        ("help", "Command reference"),
    ]);

    app.render_home(None);

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
