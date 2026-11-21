mod api;
mod parser;

use api::MailApi;
use parser::{
    clean_email_body, extract_codes, extract_links, is_mailbox_ref, mailbox_ref,
    normalize_local_part, normalize_retention, retention_label, RETENTION_24H, RETENTION_PERMANENT,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tg_core::bot::TgClient;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, hl, md_text, rows, stamp_wib, table_markdown, tg_button_row,
    tg_cb_button, tg_copy_button, tg_url_button, truncate_str,
};
use tg_core::{log_error, log_info, log_warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PAGE_SIZE: usize = 8;
const MSG_PAGE_SIZE: usize = 8;
const BULK_MAX: usize = 10;
const SYNC_TTL: Duration = Duration::from_secs(30);
const MAX_AUTO_CLAIM: usize = 5;

// ── configuration (100% env-only) ───────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    api_base: String,
    admin_token: String,
    domains: Vec<String>,
    state_file: PathBuf,
    mailboxes_file: PathBuf,
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
            .or_else(|_| env::var("OWNER_TELEGRAM_ID"))
            .map_err(|_| "OWNER_ID environment variable is required".to_string())?;
        let owner_id = owner_raw
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("Invalid OWNER_ID: {e}"))?;
        let api_base = env::var("MAIL_API_BASE")
            .or_else(|_| env::var("VTEMPMAIL_API_BASE"))
            .map_err(|_| "MAIL_API_BASE environment variable is required".to_string())?;
        let admin_token = env::var("MAIL_ADMIN_TOKEN")
            .or_else(|_| env::var("VTEMPMAIL_ADMIN_TOKEN"))
            .map_err(|_| "MAIL_ADMIN_TOKEN environment variable is required".to_string())?;
        let domains_raw = env::var("MAIL_DOMAINS")
            .or_else(|_| env::var("VTEMPMAIL_MAIL_DOMAIN"))
            .unwrap_or_default();
        let domains: Vec<String> = domains_raw
            .split(',')
            .map(|d| d.trim().to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if domains.is_empty() {
            return Err("MAIL_DOMAINS must contain at least one domain".to_string());
        }

        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| "./state/px-mail.json".to_string()),
        );
        let mailboxes_file = PathBuf::from(
            env::var("MAILBOXES_FILE")
                .unwrap_or_else(|_| "./state/px-mail-mailboxes.json".to_string()),
        );

        Ok(Self {
            tg_token,
            owner_id,
            api_base,
            admin_token,
            domains,
            state_file,
            mailboxes_file,
        })
    }
}

// ── persistent state (100% compatible with legacy mailboxes.json) ───────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxRow {
    address: String,
    jwt: String,
    #[serde(default)]
    created_at: String,
    #[serde(default = "default_ret")]
    retention: String,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

fn default_ret() -> String {
    RETENTION_24H.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MailboxesDoc {
    #[serde(default)]
    slot_limit: u64,
    #[serde(default)]
    active_address: String,
    #[serde(default)]
    mailboxes: Vec<MailboxRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Draft {
    CreateName { retention: String },
    CreateDomain { retention: String, local_part: String },
    BulkCount { retention: String, domain: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    draft: Option<Draft>,
}

#[derive(Debug, Clone, Default)]
struct SyncSummary {
    reachable: bool,
    removed: usize,
    repaired: usize,
    checked: usize,
}

#[derive(Debug, Clone)]
struct MsgRef {
    id: String,
    address: String,
    page: usize,
}

// ── app context ─────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    tg: Arc<TgClient>,
    api: MailApi,
    state: Mutex<BotState>,
    store: Mutex<MailboxesDoc>,
    sync_meta: Mutex<(Option<Instant>, Option<SyncSummary>)>,
    msg_refs: Mutex<HashMap<String, MsgRef>>,
    msg_seq: AtomicU64,
}

impl App {
    fn new(cfg: Config) -> Arc<Self> {
        let tg = TgClient::new(cfg.tg_token.clone(), cfg.owner_id);
        let api = MailApi::new(cfg.api_base.clone(), cfg.admin_token.clone());
        let state: BotState = load_json(&cfg.state_file);
        let store: MailboxesDoc = load_json(&cfg.mailboxes_file);
        Arc::new(Self {
            cfg,
            tg,
            api,
            state: Mutex::new(state),
            store: Mutex::new(store),
            sync_meta: Mutex::new((None, None)),
            msg_refs: Mutex::new(HashMap::new()),
            msg_seq: AtomicU64::new(1),
        })
    }

    fn persist_state(&self, s: &BotState) {
        save_json_atomic(&self.cfg.state_file, s);
    }

    fn persist_store(&self, doc: &MailboxesDoc) {
        save_json_atomic(&self.cfg.mailboxes_file, doc);
    }

    fn set_draft(&self, draft: Option<Draft>) {
        let mut st = self.state.lock().unwrap();
        st.draft = draft;
        self.persist_state(&st);
    }

    fn remember_msg(&self, id: &str, address: &str, page: usize) -> String {
        let seq = self.msg_seq.fetch_add(1, Ordering::Relaxed);
        let key = format!("m{seq}");
        let mut map = self.msg_refs.lock().unwrap();
        if map.len() > 250 {
            map.clear();
        }
        map.insert(
            key.clone(),
            MsgRef {
                id: id.to_string(),
                address: address.to_ascii_lowercase(),
                page,
            },
        );
        key
    }

    fn lookup_msg(&self, key: &str) -> Option<MsgRef> {
        self.msg_refs.lock().unwrap().get(key).cloned()
    }

    fn resolve_address(&self, token: &str) -> Option<String> {
        let t = token.trim();
        if t.is_empty() {
            return None;
        }
        let doc = self.store.lock().unwrap();
        if t.contains('@') {
            let lower = t.to_ascii_lowercase();
            return doc
                .mailboxes
                .iter()
                .find(|m| m.address.eq_ignore_ascii_case(&lower))
                .map(|m| m.address.clone());
        }
        if is_mailbox_ref(t) {
            return doc
                .mailboxes
                .iter()
                .find(|m| mailbox_ref(&m.address) == t)
                .map(|m| m.address.clone());
        }
        None
    }

    fn get_mailbox(&self, address: &str) -> Option<MailboxRow> {
        let lower = address.to_ascii_lowercase();
        self.store
            .lock()
            .unwrap()
            .mailboxes
            .iter()
            .find(|m| m.address.eq_ignore_ascii_case(&lower))
            .cloned()
    }

    fn upsert_mailbox(&self, row: MailboxRow) {
        let mut doc = self.store.lock().unwrap();
        let addr = row.address.to_ascii_lowercase();
        let mut new_row = row;
        new_row.address = addr.clone();
        doc.mailboxes.retain(|m| !m.address.eq_ignore_ascii_case(&addr));
        doc.mailboxes.insert(0, new_row);
        doc.active_address = addr;
        self.persist_store(&doc);
    }

    fn remove_mailbox(&self, address: &str) -> bool {
        let mut doc = self.store.lock().unwrap();
        let before = doc.mailboxes.len();
        doc.mailboxes
            .retain(|m| !m.address.eq_ignore_ascii_case(address));
        let removed = doc.mailboxes.len() != before;
        if removed {
            if doc.active_address.eq_ignore_ascii_case(address) {
                doc.active_address = doc
                    .mailboxes
                    .first()
                    .map(|m| m.address.clone())
                    .unwrap_or_default();
            }
            self.persist_store(&doc);
        }
        removed
    }

    /// Synchronizes local mailbox store with Cloudflare D1 via `/admin/mailboxes/probe`.
    /// Never removes mailboxes on transient/network errors.
    fn sync_d1(&self, force: bool) -> SyncSummary {
        {
            let meta = self.sync_meta.lock().unwrap();
            if !force {
                if let (Some(at), Some(prev)) = (&meta.0, &meta.1) {
                    if at.elapsed() < SYNC_TTL {
                        return prev.clone();
                    }
                }
            }
        }

        let items: Vec<(String, String)> = {
            self.store
                .lock()
                .unwrap()
                .mailboxes
                .iter()
                .map(|m| (m.address.clone(), m.jwt.clone()))
                .collect()
        };
        if items.is_empty() {
            let s = SyncSummary {
                reachable: true,
                removed: 0,
                repaired: 0,
                checked: 0,
            };
            *self.sync_meta.lock().unwrap() = (Some(Instant::now()), Some(s.clone()));
            return s;
        }

        let mut summary = SyncSummary {
            reachable: true,
            removed: 0,
            repaired: 0,
            checked: items.len(),
        };
        let mut probe_map: HashMap<String, Value> = HashMap::new();
        for chunk in items.chunks(100) {
            match self.api.probe_mailboxes(chunk) {
                Ok(rows) => {
                    for r in rows {
                        if let Some(a) = r.get("address").and_then(Value::as_str) {
                            probe_map.insert(a.to_ascii_lowercase(), r);
                        }
                    }
                }
                Err(_) => {
                    summary.reachable = false;
                    *self.sync_meta.lock().unwrap() = (Some(Instant::now()), Some(summary.clone()));
                    return summary;
                }
            }
        }

        let mut drop_set: HashSet<String> = HashSet::new();
        let mut stale_jwts: Vec<(String, String)> = Vec::new();
        let mut patches: HashMap<String, (Option<String>, Option<Option<String>>, Option<String>)> =
            HashMap::new();

        for (addr, jwt) in &items {
            let lower = addr.to_ascii_lowercase();
            let Some(p) = probe_map.get(&lower) else {
                continue;
            };
            let exists = p.get("exists").and_then(Value::as_bool).unwrap_or(true);
            let expired = p.get("expired").and_then(Value::as_bool).unwrap_or(false);
            if !exists || expired {
                drop_set.insert(lower);
                continue;
            }
            let jwt_valid = p.get("jwtValid").and_then(Value::as_bool).unwrap_or(true);
            if !jwt_valid && stale_jwts.len() < MAX_AUTO_CLAIM {
                stale_jwts.push((lower.clone(), jwt.clone()));
            }
            let ret = p.get("retention").and_then(Value::as_str).map(|s| normalize_retention(s).to_string());
            let exp = p.get("expiresAt").map(|v| v.as_str().map(ToString::to_string));
            patches.insert(lower, (ret, exp, None));
        }

        for (addr, old_jwt) in stale_jwts {
            if let Ok(claimed) = self.api.claim_address(&addr, Some(&old_jwt)) {
                if let Some(new_jwt) = claimed.get("jwt").and_then(Value::as_str) {
                    patches
                        .entry(addr)
                        .and_modify(|e| e.2 = Some(new_jwt.to_string()))
                        .or_insert((None, None, Some(new_jwt.to_string())));
                    summary.repaired += 1;
                }
            }
        }

        {
            let mut doc = self.store.lock().unwrap();
            let before = doc.mailboxes.len();
            doc.mailboxes
                .retain(|m| !drop_set.contains(&m.address.to_ascii_lowercase()));
            summary.removed = before.saturating_sub(doc.mailboxes.len());
            for m in &mut doc.mailboxes {
                let lower = m.address.to_ascii_lowercase();
                if let Some((ret, exp, new_jwt)) = patches.get(&lower) {
                    if let Some(r) = ret {
                        m.retention = r.clone();
                    }
                    if let Some(e) = exp {
                        m.expires_at = if m.retention == RETENTION_PERMANENT {
                            None
                        } else {
                            e.clone()
                        };
                    }
                    if let Some(j) = new_jwt {
                        m.jwt = j.clone();
                    }
                }
            }
            if !doc
                .mailboxes
                .iter()
                .any(|m| m.address.eq_ignore_ascii_case(&doc.active_address))
            {
                doc.active_address = doc
                    .mailboxes
                    .first()
                    .map(|m| m.address.clone())
                    .unwrap_or_default();
            }
            if summary.removed > 0 || summary.repaired > 0 {
                self.persist_store(&doc);
            }
        }

        *self.sync_meta.lock().unwrap() = (Some(Instant::now()), Some(summary.clone()));
        summary
    }

    // ── renderers (Bot API 10.3+) ───────────────────────────────────────────
    fn home_text(&self, sync: &SyncSummary) -> String {
        let doc = self.store.lock().unwrap();
        let total = doc.mailboxes.len();
        let perm = doc
            .mailboxes
            .iter()
            .filter(|m| normalize_retention(&m.retention) == RETENTION_PERMANENT)
            .count();
        let temp = total.saturating_sub(perm);
        drop(doc);

        let sync_val = if !sync.reachable {
            hl("⚠️ Offline (local list kept)")
        } else if sync.removed > 0 || sync.repaired > 0 {
            hl(&format!(
                "🔄 Cleaned {} · Repaired {}",
                sync.removed, sync.repaired
            ))
        } else {
            hl("🟢 Synced with D1")
        };

        let grid = field_grid(&[
            ("Mailboxes", hl(&format!("{total} total · {temp} 24h · {perm} perm"))),
            ("Domains", hl(&format!("{} available", self.cfg.domains.len()))),
            ("D1 Sync", sync_val),
        ]);

        let pills = tg_button_row(&[
            tg_cb_button("➕ Create", "create:start"),
            tg_cb_button("⚡ Bulk", "gen:start"),
            tg_cb_button("📧 Emails", "mail:list"),
            tg_cb_button("📥 Inbox", "inbox:pick"),
        ]);

        let dom_list = self
            .cfg
            .domains
            .iter()
            .map(|d| format!("- `{d}`"))
            .collect::<Vec<_>>()
            .join("\n");

        let cmds = table_markdown(
            &["Command", "Description"],
            &[
                vec!["`/start` · `/menu`".into(), "Mail dashboard".into()],
                vec!["`/create`".into(), "Create a new address".into()],
                vec!["`/generate`".into(), format!("Bulk create (up to {BULK_MAX})")],
                vec!["`/emails`".into(), "Manage saved addresses".into()],
                vec!["`/inbox`".into(), "Read inbox & extract OTPs".into()],
            ],
        );

        [
            format!("**✉️ PX-Mail** · {}", hl(&format!("v{VERSION}"))),
            String::new(),
            grid,
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Domains & Commands", &format!("**Configured domains:**\n{dom_list}\n\n{cmds}")),
        ]
        .join("\n")
    }

    fn show_home(&self, msg_id: Option<i64>, force_sync: bool) {
        self.set_draft(None);
        let sync = self.sync_d1(force_sync);
        self.tg
            .present(msg_id, &self.home_text(&sync), Some(kb_main()));
    }

    fn show_create_retention(&self, msg_id: Option<i64>) {
        self.set_draft(None);
        let card = [
            format!("**➕ PX-Mail · Create Address** · {}", hl("Step 1/3")),
            String::new(),
            field_grid(&[
                ("⏱ 24 Hours", "Auto-expires on server after 24h".into()),
                ("♾ Permanent", "Kept until manually deleted".into()),
            ]),
            String::new(),
            "Choose retention policy.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [
                    button("⏱ 24 Hours", "create:ret:24h", "primary"),
                    button("♾ Permanent", "create:ret:permanent", "success"),
                ],
                [button("🏠 Dashboard", "menu:home", "primary")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_create_name(&self, retention: &str, msg_id: Option<i64>) {
        let ret = normalize_retention(retention).to_string();
        self.set_draft(Some(Draft::CreateName {
            retention: ret.clone(),
        }));
        let card = [
            format!("**➕ PX-Mail · Local-Part** · {}", hl("Step 2/3")),
            String::new(),
            field_grid(&[
                ("Retention", hl(retention_label(&ret))),
                ("Format", "`a-z`, `0-9`, `. _ + -` (max 64 chars)".into()),
            ]),
            String::new(),
            "Send a custom name (before `@`), or tap **Skip · Random**.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [button("⏭ Skip · Random Name", "create:skip", "success")],
                [button("❌ Cancel", "menu:home", "danger")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_create_domain(&self, retention: &str, local_part: String, msg_id: Option<i64>) {
        if self.cfg.domains.len() == 1 {
            let dom = self.cfg.domains[0].clone();
            self.do_create_one(&dom, &local_part, retention, msg_id);
            return;
        }
        let ret = normalize_retention(retention).to_string();
        self.set_draft(Some(Draft::CreateDomain {
            retention: ret.clone(),
            local_part: local_part.clone(),
        }));
        let name_show = if local_part.is_empty() {
            "random".to_string()
        } else {
            format!("`{local_part}`")
        };
        let card = [
            format!("**➕ PX-Mail · Choose Domain** · {}", hl("Step 3/3")),
            String::new(),
            field_grid(&[
                ("Retention", hl(retention_label(&ret))),
                ("Name", name_show),
            ]),
            String::new(),
            "Select the domain for this address.".to_string(),
        ]
        .join("\n");
        let mut kb_rows: Vec<Value> = self
            .cfg
            .domains
            .iter()
            .map(|d| json!([button(&format!("@{d}"), &format!("create:dom:{d}"), "primary")]))
            .collect();
        kb_rows.push(json!([button("❌ Cancel", "menu:home", "danger")]));
        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn do_create_one(&self, domain: &str, local_part: &str, retention: &str, msg_id: Option<i64>) {
        self.set_draft(None);
        let lp = if local_part.is_empty() {
            None
        } else {
            Some(local_part)
        };
        match self.api.create_address(Some(domain), lp, retention) {
            Ok(res) => {
                let addr = res
                    .get("address")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let jwt = res
                    .get("jwt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let ret = normalize_retention(
                    res.get("retention")
                        .and_then(Value::as_str)
                        .unwrap_or(retention),
                )
                .to_string();
                let exp = res
                    .get("expiresAt")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let created_at = res
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.upsert_mailbox(MailboxRow {
                    address: addr.clone(),
                    jwt,
                    created_at,
                    retention: ret.clone(),
                    expires_at: if ret == RETENTION_PERMANENT { None } else { exp },
                    label: None,
                });

                let mref = mailbox_ref(&addr);
                let pills = tg_button_row(&[
                    tg_copy_button("📋 Copy Address", &addr),
                    tg_cb_button("📥 Open Inbox", &format!("inbox:open:{mref}:0")),
                ]);
                let card = [
                    format!("**✅ PX-Mail · Address Created** · {}", hl("Active")),
                    String::new(),
                    field_grid(&[
                        ("Address", format!("`{addr}`")),
                        ("Retention", hl(retention_label(&ret))),
                    ]),
                    String::new(),
                    format!("_{}_", stamp_wib()),
                    String::new(),
                    pills,
                ]
                .join("\n");
                let kb = json!({
                    "inline_keyboard": [
                        [
                            button("📥 Open Inbox", &format!("inbox:open:{mref}:0"), "primary"),
                            button("➕ Create Another", "create:start", "success"),
                        ],
                        [
                            button("🗑 Delete", &format!("mail:del:{mref}"), "danger"),
                            button("🏠 Dashboard", "menu:home", "primary"),
                        ],
                    ]
                });
                self.tg.present(msg_id, &card, Some(kb));
            }
            Err(e) => {
                self.tg
                    .present(msg_id, &error_card("Create Failed", &e.message), Some(kb_main()));
            }
        }
    }

    fn show_bulk_retention(&self, msg_id: Option<i64>) {
        self.set_draft(None);
        let card = [
            format!("**⚡ PX-Mail · Bulk Generate** · {}", hl("Step 1/3")),
            String::new(),
            format!("Generate up to **{BULK_MAX}** random addresses at once."),
            String::new(),
            "Choose retention policy.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [
                    button("⏱ 24 Hours", "bulk:ret:24h", "primary"),
                    button("♾ Permanent", "bulk:ret:permanent", "success"),
                ],
                [button("🏠 Dashboard", "menu:home", "primary")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_bulk_domain(&self, retention: &str, msg_id: Option<i64>) {
        if self.cfg.domains.len() == 1 {
            let dom = self.cfg.domains[0].clone();
            self.show_bulk_count(retention, &dom, msg_id);
            return;
        }
        let ret = normalize_retention(retention);
        let card = [
            format!("**⚡ PX-Mail · Bulk Domain** · {}", hl("Step 2/3")),
            String::new(),
            field_grid(&[("Retention", hl(retention_label(ret)))]),
            String::new(),
            "Select the target domain.".to_string(),
        ]
        .join("\n");
        let mut kb_rows: Vec<Value> = self
            .cfg
            .domains
            .iter()
            .map(|d| json!([button(&format!("@{d}"), &format!("bulk:dom:{ret}:{d}"), "primary")]))
            .collect();
        kb_rows.push(json!([button("❌ Cancel", "menu:home", "danger")]));
        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn show_bulk_count(&self, retention: &str, domain: &str, msg_id: Option<i64>) {
        let ret = normalize_retention(retention).to_string();
        self.set_draft(Some(Draft::BulkCount {
            retention: ret.clone(),
            domain: domain.to_string(),
        }));
        let card = [
            format!("**⚡ PX-Mail · Batch Size** · {}", hl("Step 3/3")),
            String::new(),
            field_grid(&[
                ("Retention", hl(retention_label(&ret))),
                ("Domain", format!("`@{domain}`")),
                ("Max Batch", hl(&format!("{BULK_MAX}"))),
            ]),
            String::new(),
            format!("Tap a preset below or send a number `1–{BULK_MAX}`."),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [
                    button("3x", &format!("bulk:n:3:{ret}:{domain}"), "primary"),
                    button("5x", &format!("bulk:n:5:{ret}:{domain}"), "primary"),
                    button("10x", &format!("bulk:n:10:{ret}:{domain}"), "success"),
                ],
                [button("❌ Cancel", "menu:home", "danger")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn do_bulk(&self, retention: &str, domain: &str, count: usize, msg_id: Option<i64>) {
        self.set_draft(None);
        let n = count.clamp(1, BULK_MAX);
        let mut created = Vec::new();
        let mut errors = 0usize;
        for _ in 0..n {
            match self.api.create_address(Some(domain), None, retention) {
                Ok(res) => {
                    let addr = res
                        .get("address")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let jwt = res
                        .get("jwt")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let ret = normalize_retention(
                        res.get("retention")
                            .and_then(Value::as_str)
                            .unwrap_or(retention),
                    )
                    .to_string();
                    let exp = res
                        .get("expiresAt")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    let created_at = res
                        .get("createdAt")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !addr.is_empty() {
                        self.upsert_mailbox(MailboxRow {
                            address: addr.clone(),
                            jwt,
                            created_at,
                            retention: ret.clone(),
                            expires_at: if ret == RETENTION_PERMANENT { None } else { exp },
                            label: None,
                        });
                        created.push(addr);
                    }
                }
                Err(_) => errors += 1,
            }
        }

        let rows_data: Vec<Vec<String>> = created
            .iter()
            .enumerate()
            .map(|(i, a)| vec![format!("{}", i + 1), format!("`{a}`")])
            .collect();
        let all_copy = created.join("\n");
        let pills = if !created.is_empty() {
            tg_button_row(&[
                tg_copy_button("📋 Copy All", &all_copy),
                tg_cb_button("📧 My Emails", "mail:list"),
            ])
        } else {
            String::new()
        };

        let card = [
            format!(
                "**⚡ PX-Mail · Bulk Complete** · {}",
                hl(&format!("{}/{n} created", created.len()))
            ),
            if errors > 0 {
                format!("⚠️ {errors} requests failed.")
            } else {
                String::new()
            },
            table_markdown(&["#", "Address"], &rows_data),
            format!("_{}_", stamp_wib()),
            pills,
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

        self.tg.present(msg_id, &card, Some(kb_main()));
    }

    fn show_mailbox_list(
        &self,
        for_inbox: bool,
        retention_filter: Option<&str>,
        page: usize,
        msg_id: Option<i64>,
    ) {
        self.set_draft(None);
        let _ = self.sync_d1(false);
        let all = { self.store.lock().unwrap().mailboxes.clone() };
        let filtered: Vec<MailboxRow> = all
            .into_iter()
            .filter(|m| {
                retention_filter
                    .map(|r| normalize_retention(&m.retention) == r)
                    .unwrap_or(true)
            })
            .collect();

        let total = filtered.len();
        let pages = total.div_ceil(PAGE_SIZE).max(1);
        let p = page.min(pages - 1);
        let slice = &filtered[p * PAGE_SIZE..(p * PAGE_SIZE + PAGE_SIZE).min(total)];

        let mode_title = if for_inbox { "📥 Inbox" } else { "📧 My Emails" };
        let ret_tag = match retention_filter {
            Some(RETENTION_24H) => "24h",
            Some(RETENTION_PERMANENT) => "Permanent",
            _ => "All",
        };
        let rows_data: Vec<Vec<String>> = if slice.is_empty() {
            vec![vec!["_No mailboxes found_".into(), "—".into()]]
        } else {
            slice
                .iter()
                .map(|m| {
                    vec![
                        format!("`{}`", truncate_str(&m.address, 30)),
                        retention_label(&m.retention).to_string(),
                    ]
                })
                .collect()
        };

        let card = [
            format!(
                "**{mode_title}** · {} · {} · {}",
                hl(ret_tag),
                hl(&format!("{total} total")),
                hl(&format!("{}/{pages}", p + 1))
            ),
            String::new(),
            table_markdown(&["Address", "Retention"], &rows_data),
            String::new(),
            format!("_{}_", stamp_wib()),
        ]
        .join("\n");

        let ret_key = retention_filter.unwrap_or("all");
        let list_prefix = if for_inbox { "inbox" } else { "mail" };

        let mut kb_rows: Vec<Value> = slice
            .iter()
            .map(|m| {
                let r = mailbox_ref(&m.address);
                let cb = if for_inbox {
                    format!("inbox:open:{r}:0")
                } else {
                    format!("mail:pick:{r}")
                };
                json!([button(&truncate_str(&m.address, 38), &cb, "primary")])
            })
            .collect();

        // Filter row
        kb_rows.push(json!([
            button(
                "All",
                &format!("{list_prefix}:pg:all:0"),
                if retention_filter.is_none() { "success" } else { "primary" }
            ),
            button(
                "⏱ 24h",
                &format!("{list_prefix}:pg:24h:0"),
                if retention_filter == Some(RETENTION_24H) { "success" } else { "primary" }
            ),
            button(
                "♾ Perm",
                &format!("{list_prefix}:pg:permanent:0"),
                if retention_filter == Some(RETENTION_PERMANENT) { "success" } else { "primary" }
            ),
        ]));

        let mut nav = Vec::new();
        if p > 0 {
            nav.push(button(
                &format!("← Page {p}"),
                &format!("{list_prefix}:pg:{ret_key}:{}", p - 1),
                "primary",
            ));
        }
        if p + 1 < pages {
            nav.push(button(
                &format!("Page {} →", p + 2),
                &format!("{list_prefix}:pg:{ret_key}:{}", p + 1),
                "primary",
            ));
        }
        if !nav.is_empty() {
            kb_rows.push(Value::Array(nav));
        }

        if !for_inbox && total > 0 {
            kb_rows.push(json!([
                button("➕ Create", "create:start", "success"),
                button("🗑 Delete All", "mail:delall", "danger"),
            ]));
        }
        kb_rows.push(json!([button("🏠 Dashboard", "menu:home", "primary")]));

        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn show_mailbox_detail(&self, address: &str, msg_id: Option<i64>) {
        let Some(box_row) = self.get_mailbox(address) else {
            self.tg.present(
                msg_id,
                &error_card("Not Found", "Mailbox no longer exists."),
                Some(kb_main()),
            );
            return;
        };
        let mref = mailbox_ref(&box_row.address);
        let pills = tg_button_row(&[
            tg_copy_button("📋 Copy Address", &box_row.address),
            tg_cb_button("📥 Open Inbox", &format!("inbox:open:{mref}:0")),
        ]);
        let mut pairs = vec![
            ("Address", format!("`{}`", box_row.address)),
            ("Retention", hl(retention_label(&box_row.retention))),
        ];
        if !box_row.created_at.is_empty() {
            pairs.push((
                "Created",
                format!("`{}`", truncate_str(&box_row.created_at, 19).replace('T', " ")),
            ));
        }
        if let Some(exp) = &box_row.expires_at {
            pairs.push((
                "Expires",
                format!("`{}`", truncate_str(exp, 19).replace('T', " ")),
            ));
        }
        let card = [
            format!("**📧 PX-Mail · Address Detail** · {}", hl("Active")),
            String::new(),
            field_grid(&pairs),
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [
                    button("📥 Open Inbox", &format!("inbox:open:{mref}:0"), "primary"),
                    button("🗑 Delete", &format!("mail:del:{mref}"), "danger"),
                ],
                [
                    button("← My Emails", "mail:list", "primary"),
                    button("🏠 Dashboard", "menu:home", "primary"),
                ],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_inbox(&self, address: &str, page: usize, msg_id: Option<i64>) {
        let Some(box_row) = self.get_mailbox(address) else {
            self.tg.present(
                msg_id,
                &error_card("Not Found", "Mailbox not in local store."),
                Some(kb_main()),
            );
            return;
        };
        let mref = mailbox_ref(&box_row.address);
        match self.api.list_messages(&box_row.jwt) {
            Ok(msgs) => {
                let total = msgs.len();
                let pages = total.div_ceil(MSG_PAGE_SIZE).max(1);
                let p = page.min(pages - 1);
                let slice = &msgs[p * MSG_PAGE_SIZE..(p * MSG_PAGE_SIZE + MSG_PAGE_SIZE).min(total)];

                let rows_data: Vec<Vec<String>> = if slice.is_empty() {
                    vec![vec!["_No messages yet_".into(), "—".into()]]
                } else {
                    slice
                        .iter()
                        .map(|m| {
                            let from = m
                                .get("from_address")
                                .or_else(|| m.get("from"))
                                .and_then(Value::as_str)
                                .unwrap_or("—");
                            let subj = m
                                .get("subject")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .unwrap_or("(no subject)");
                            vec![
                                truncate_str(&md_text(from), 18),
                                truncate_str(&md_text(subj), 26),
                            ]
                        })
                        .collect()
                };

                let pills = tg_button_row(&[
                    tg_copy_button("📋 Copy Address", &box_row.address),
                    tg_cb_button("🔄 Refresh", &format!("inbox:open:{mref}:{p}")),
                ]);

                let card = [
                    format!(
                        "**📥 PX-Mail · Inbox** · {}",
                        hl(&format!("{total} messages"))
                    ),
                    String::new(),
                    field_grid(&[
                        ("Address", format!("`{}`", box_row.address)),
                        ("Page", hl(&format!("{}/{pages}", p + 1))),
                    ]),
                    String::new(),
                    table_markdown(&["From", "Subject"], &rows_data),
                    String::new(),
                    format!("_{}_", stamp_wib()),
                    String::new(),
                    pills,
                ]
                .join("\n");

                let mut kb_rows: Vec<Value> = slice
                    .iter()
                    .filter_map(|m| {
                        let id = m.get("id").and_then(Value::as_str)?;
                        let subj = m
                            .get("subject")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("(no subject)");
                        let key = self.remember_msg(id, &box_row.address, p);
                        Some(json!([button(
                            &format!("📨 {}", truncate_str(subj, 36)),
                            &format!("msg:{key}"),
                            "primary"
                        )]))
                    })
                    .collect();

                let mut nav = Vec::new();
                if p > 0 {
                    nav.push(button(
                        &format!("← Page {p}"),
                        &format!("inbox:open:{mref}:{}", p - 1),
                        "primary",
                    ));
                }
                if p + 1 < pages {
                    nav.push(button(
                        &format!("Page {} →", p + 2),
                        &format!("inbox:open:{mref}:{}", p + 1),
                        "primary",
                    ));
                }
                if !nav.is_empty() {
                    kb_rows.push(Value::Array(nav));
                }
                kb_rows.push(json!([
                    button("🔄 Refresh", &format!("inbox:open:{mref}:{p}"), "success"),
                    button("🗑 Delete Email", &format!("mail:del:{mref}"), "danger"),
                ]));
                kb_rows.push(json!([
                    button("← All Inboxes", "inbox:pick", "primary"),
                    button("🏠 Dashboard", "menu:home", "primary"),
                ]));

                self.tg
                    .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
            }
            Err(e) => {
                self.tg.present(
                    msg_id,
                    &error_card("Inbox Error", &e.message),
                    Some(json!({
                        "inline_keyboard": [[
                            button("🔄 Retry", &format!("inbox:open:{mref}:0"), "primary"),
                            button("🏠 Dashboard", "menu:home", "primary"),
                        ]]
                    })),
                );
            }
        }
    }

    fn render_full_message(&self, msg: &Value, fallback_addr: &str, back_page: usize, msg_id: Option<i64>) {
        let to_addr = msg
            .get("inbox_address")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_addr);
        let from = msg
            .get("from_address")
            .or_else(|| msg.get("from"))
            .and_then(Value::as_str)
            .unwrap_or("—");
        let subject = msg
            .get("subject")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("(no subject)");
        let received = msg
            .get("received_at")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let raw_body = msg.get("body").and_then(Value::as_str).unwrap_or("");

        let codes = extract_codes(raw_body);
        let links = extract_links(raw_body, 4);
        let body = clean_email_body(raw_body, 2800);

        let mut pills = Vec::new();
        for c in &codes {
            pills.push(tg_copy_button(&format!("📋 OTP {c}"), c));
        }
        for (lbl, url) in &links {
            pills.push(tg_url_button(&format!("🔗 {lbl}"), url));
        }

        let code_line = if codes.is_empty() {
            String::new()
        } else {
            let chips = codes
                .iter()
                .map(|c| hl(&format!("`{c}`")))
                .collect::<Vec<_>>()
                .join(" · ");
            format!("🔑 **Codes:** {chips}")
        };

        let meta = field_grid(&[
            ("To", format!("`{to_addr}`")),
            ("From", md_text(from)),
            ("Subject", format!("**{}**", md_text(subject))),
            ("Time", format!("`{received}`")),
        ]);

        let mut parts = vec![
            "**📨 PX-Mail · Message**".to_string(),
            meta,
        ];
        if !code_line.is_empty() {
            parts.push(code_line);
        }
        if !pills.is_empty() {
            parts.push(tg_button_row(&pills));
        }
        parts.push("---".to_string());
        parts.push(if body.is_empty() {
            "_(empty message body)_".to_string()
        } else {
            body
        });

        let mref = mailbox_ref(to_addr);
        let kb = json!({
            "inline_keyboard": [
                [
                    button("📥 Back to Inbox", &format!("inbox:open:{mref}:{back_page}"), "primary"),
                    button("🗑 Delete Email", &format!("mail:del:{mref}"), "danger"),
                ],
                [button("🏠 Dashboard", "menu:home", "primary")],
            ]
        });
        self.tg.present(msg_id, &parts.join("\n\n"), Some(kb));
    }

    fn show_latest_message_push(&self, address: &str, msg_id: Option<i64>) {
        let Some(box_row) = self.get_mailbox(address) else {
            self.tg.present(
                msg_id,
                &error_card("Not Found", "Address not in local list."),
                Some(kb_main()),
            );
            return;
        };
        match self.api.list_messages(&box_row.jwt) {
            Ok(msgs) if !msgs.is_empty() => {
                if let Some(id) = msgs[0].get("id").and_then(Value::as_str) {
                    match self.api.get_message(&box_row.jwt, id) {
                        Ok(full) => self.render_full_message(&full, &box_row.address, 0, msg_id),
                        Err(e) => {
                            self.tg.present(
                                msg_id,
                                &error_card("Read Failed", &e.message),
                                Some(kb_main()),
                            );
                        }
                    }
                }
            }
            Ok(_) => {
                self.show_inbox(&box_row.address, 0, msg_id);
            }
            Err(e) => {
                self.tg.present(
                    msg_id,
                    &error_card("Inbox Error", &e.message),
                    Some(kb_main()),
                );
            }
        }
    }

    fn show_full_message_by_key(&self, key: &str, msg_id: Option<i64>) {
        let Some(mref) = self.lookup_msg(key) else {
            self.tg.present(
                msg_id,
                &error_card("Expired Button", "Re-open Inbox to refresh the message list."),
                Some(kb_main()),
            );
            return;
        };
        let Some(box_row) = self.get_mailbox(&mref.address) else {
            self.tg.present(
                msg_id,
                &error_card("Not Found", "Mailbox was removed."),
                Some(kb_main()),
            );
            return;
        };
        match self.api.get_message(&box_row.jwt, &mref.id) {
            Ok(full) => self.render_full_message(&full, &box_row.address, mref.page, msg_id),
            Err(e) => {
                self.tg.present(
                    msg_id,
                    &error_card("Read Failed", &e.message),
                    Some(kb_main()),
                );
            }
        }
    }

    fn confirm_delete_one(&self, address: &str, msg_id: Option<i64>) {
        let mref = mailbox_ref(address);
        let card = [
            format!("**⚠️ PX-Mail · Confirm Delete** · {}", hl("Irreversible")),
            String::new(),
            field_grid(&[
                ("Address", format!("`{address}`")),
                ("Scope", "Deletes inbox & all messages on server + local".into()),
            ]),
            String::new(),
            "Proceed with deletion?".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [[
                button("🗑 Yes, Delete", &format!("mail:del:confirm:{mref}"), "danger"),
                button("❌ Cancel", &format!("mail:pick:{mref}"), "primary"),
            ]]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn do_delete_one(&self, address: &str, msg_id: Option<i64>) {
        let jwt = self.get_mailbox(address).map(|m| m.jwt);
        let server_note = match self.api.delete_inbox(address, jwt.as_deref()) {
            Ok(_) => "✅ Deleted on server",
            Err(e) if e.status == Some(404) => "✅ Already gone on server",
            Err(e) => {
                self.tg.present(
                    msg_id,
                    &error_card("Delete Failed", &e.message),
                    Some(kb_main()),
                );
                return;
            }
        };
        self.remove_mailbox(address);
        let card = [
            format!("**🗑 PX-Mail · Deleted** · {}", hl("Done")),
            String::new(),
            field_grid(&[
                ("Address", format!("`{address}`")),
                ("Server", server_note.to_string()),
            ]),
            String::new(),
            format!("_{}_", stamp_wib()),
        ]
        .join("\n");
        self.tg.present(msg_id, &card, Some(kb_main()));
    }

    fn confirm_delete_all(&self, msg_id: Option<i64>) {
        let count = self.store.lock().unwrap().mailboxes.len();
        let card = [
            format!("**⚠️ PX-Mail · Delete ALL Mailboxes** · {}", hl(&format!("{count} addresses"))),
            String::new(),
            "This deletes every mailbox on the server AND clears your local list.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [[
                button("🗑 Yes, Delete All", "mail:delall:yes", "danger"),
                button("❌ Cancel", "mail:list", "primary"),
            ]]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn do_delete_all(&self, msg_id: Option<i64>) {
        let all = { self.store.lock().unwrap().mailboxes.clone() };
        let mut deleted = 0usize;
        for m in &all {
            let _ = self.api.delete_inbox(&m.address, Some(&m.jwt));
            deleted += 1;
        }
        {
            let mut doc = self.store.lock().unwrap();
            doc.mailboxes.clear();
            doc.active_address.clear();
            self.persist_store(&doc);
        }
        let card = format!(
            "**🗑 PX-Mail · All Deleted** · {}\n\nRemoved **{deleted}** mailboxes.\n\n_{}_",
            hl("Cleared"),
            stamp_wib()
        );
        self.tg.present(msg_id, &card, Some(kb_main()));
    }
}

// ── keyboards & errors ──────────────────────────────────────────────────────
fn kb_main() -> Value {
    json!({
        "inline_keyboard": rows(vec![
            button("➕ Create Email", "create:start", "success"),
            button("⚡ Bulk Generate", "gen:start", "primary"),
            button("📧 My Emails", "mail:list", "primary"),
            button("📥 Inbox", "inbox:pick", "primary"),
            button("🔄 Sync D1", "sync:force", "primary"),
            button("🏠 Refresh", "menu:home", ""),
        ])
    })
}

fn error_card(title: &str, msg: &str) -> String {
    [
        format!("**❌ PX-Mail · {title}** · {}", hl("Error")),
        String::new(),
        format!("`{}`", truncate_str(msg, 240)),
        String::new(),
        format!("_{}_", stamp_wib()),
    ]
    .join("\n")
}

// ── event handlers ──────────────────────────────────────────────────────────
fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let data = cb.get("data").and_then(Value::as_str).unwrap_or_default().trim();
    let chat_id = cb.pointer("/message/chat/id").and_then(Value::as_i64).unwrap_or(0);
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);

    if !app.tg.is_owner(chat_id) {
        app.tg.ack(cb_id, "Owner access only.");
        return;
    }

    if data == "menu:home" {
        app.tg.ack(cb_id, "");
        app.show_home(msg_id, false);
    } else if data == "sync:force" {
        app.tg.ack(cb_id, "Syncing D1…");
        app.show_home(msg_id, true);
    } else if data == "create:start" {
        app.tg.ack(cb_id, "");
        app.show_create_retention(msg_id);
    } else if let Some(ret) = data.strip_prefix("create:ret:") {
        app.tg.ack(cb_id, "");
        app.show_create_name(ret, msg_id);
    } else if data == "create:skip" {
        app.tg.ack(cb_id, "");
        let ret = match app.state.lock().unwrap().draft.clone() {
            Some(Draft::CreateName { retention }) => retention,
            _ => RETENTION_24H.to_string(),
        };
        app.show_create_domain(&ret, String::new(), msg_id);
    } else if let Some(dom) = data.strip_prefix("create:dom:") {
        app.tg.ack(cb_id, "Creating…");
        let (ret, lp) = match app.state.lock().unwrap().draft.clone() {
            Some(Draft::CreateDomain {
                retention,
                local_part,
            }) => (retention, local_part),
            _ => (RETENTION_24H.to_string(), String::new()),
        };
        app.do_create_one(dom, &lp, &ret, msg_id);
    } else if data == "gen:start" || data == "gen:pick" {
        app.tg.ack(cb_id, "");
        app.show_bulk_retention(msg_id);
    } else if let Some(ret) = data.strip_prefix("bulk:ret:") {
        app.tg.ack(cb_id, "");
        app.show_bulk_domain(ret, msg_id);
    } else if let Some(rest) = data.strip_prefix("bulk:dom:") {
        app.tg.ack(cb_id, "");
        if let Some((ret, dom)) = rest.split_once(':') {
            app.show_bulk_count(ret, dom, msg_id);
        } else {
            app.show_bulk_count(RETENTION_24H, rest, msg_id);
        }
    } else if let Some(rest) = data.strip_prefix("bulk:n:") {
        app.tg.ack(cb_id, "Generating…");
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        if parts.len() == 3 {
            let n = parts[0].parse::<usize>().unwrap_or(3);
            app.do_bulk(parts[1], parts[2], n, msg_id);
        } else {
            app.show_bulk_retention(msg_id);
        }
    } else if data == "mail:list" {
        app.tg.ack(cb_id, "");
        app.show_mailbox_list(false, None, 0, msg_id);
    } else if let Some(rest) = data.strip_prefix("mail:pg:") {
        app.tg.ack(cb_id, "");
        let (ret_s, pg_s) = rest.split_once(':').unwrap_or(("all", "0"));
        let ret = if ret_s == "all" { None } else { Some(ret_s) };
        let pg = pg_s.parse::<usize>().unwrap_or(0);
        app.show_mailbox_list(false, ret, pg, msg_id);
    } else if let Some(tok) = data.strip_prefix("mail:pick:") {
        app.tg.ack(cb_id, "");
        match app.resolve_address(tok) {
            Some(addr) => app.show_mailbox_detail(&addr, msg_id),
            None => app.show_mailbox_list(false, None, 0, msg_id),
        }
    } else if let Some(tok) = data.strip_prefix("mail:del:confirm:") {
        app.tg.ack(cb_id, "Deleting…");
        match app.resolve_address(tok) {
            Some(addr) => app.do_delete_one(&addr, msg_id),
            None => app.show_home(msg_id, false),
        }
    } else if let Some(tok) = data.strip_prefix("mail:del:") {
        app.tg.ack(cb_id, "");
        match app.resolve_address(tok) {
            Some(addr) => app.confirm_delete_one(&addr, msg_id),
            None => app.show_mailbox_list(false, None, 0, msg_id),
        }
    } else if data == "mail:delall" {
        app.tg.ack(cb_id, "");
        app.confirm_delete_all(msg_id);
    } else if data == "mail:delall:yes" {
        app.tg.ack(cb_id, "Deleting all…");
        app.do_delete_all(msg_id);
    } else if data == "inbox:pick" {
        app.tg.ack(cb_id, "");
        app.show_mailbox_list(true, None, 0, msg_id);
    } else if let Some(rest) = data.strip_prefix("inbox:pg:") {
        app.tg.ack(cb_id, "");
        let (ret_s, pg_s) = rest.split_once(':').unwrap_or(("all", "0"));
        let ret = if ret_s == "all" { None } else { Some(ret_s) };
        let pg = pg_s.parse::<usize>().unwrap_or(0);
        app.show_mailbox_list(true, ret, pg, msg_id);
    } else if let Some(rest) = data.strip_prefix("inbox:open:") {
        app.tg.ack(cb_id, "");
        let (tok, pg) = match rest.rsplit_once(':') {
            Some((t, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (t, p.parse::<usize>().unwrap_or(0))
            }
            _ => (rest, 0),
        };
        match app.resolve_address(tok) {
            Some(addr) => app.show_inbox(&addr, pg, msg_id),
            None => app.show_mailbox_list(true, None, 0, msg_id),
        }
    } else if let Some(tok) = data.strip_prefix("inbox:msg:") {
        // Worker push-notification direct open button!
        app.tg.ack(cb_id, "Opening…");
        match app.resolve_address(tok) {
            Some(addr) => app.show_latest_message_push(&addr, msg_id),
            None => app.show_mailbox_list(true, None, 0, msg_id),
        }
    } else if let Some(key) = data.strip_prefix("msg:") {
        app.tg.ack(cb_id, "");
        app.show_full_message_by_key(key, msg_id);
    } else {
        app.tg.ack(cb_id, "");
        app.show_home(msg_id, false);
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
        let cmd = text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let cmd = cmd.split('@').next().unwrap_or("");
        match cmd {
            "/start" | "/menu" | "/help" => app.show_home(None, false),
            "/create" => app.show_create_retention(None),
            "/generate" => app.show_bulk_retention(None),
            "/emails" => app.show_mailbox_list(false, None, 0, None),
            "/inbox" => app.show_mailbox_list(true, None, 0, None),
            _ => app.show_home(None, false),
        }
        return;
    }

    let draft = { app.state.lock().unwrap().draft.clone() };
    if let Some(d) = draft {
        match d {
            Draft::CreateName { retention } => match normalize_local_part(text) {
                Ok(lp) => app.show_create_domain(&retention, lp, None),
                Err(e) => {
                    app.tg.send(
                        &error_card("Invalid Name", &e),
                        Some(json!({
                            "inline_keyboard": [
                                [button("⏭ Skip · Random Name", "create:skip", "success")],
                                [button("❌ Cancel", "menu:home", "danger")],
                            ]
                        })),
                    );
                }
            },
            Draft::BulkCount { retention, domain } => match text.parse::<usize>() {
                Ok(n) if (1..=BULK_MAX).contains(&n) => {
                    app.do_bulk(&retention, &domain, n, None);
                }
                _ => {
                    app.tg.send(
                        &error_card("Invalid Count", &format!("Enter a number between 1 and {BULK_MAX}.")),
                        Some(kb_main()),
                    );
                }
            },
            Draft::CreateDomain { .. } => app.show_home(None, false),
        }
        return;
    }

    app.show_home(None, false);
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-mail {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                let doc: MailboxesDoc = load_json(&cfg.mailboxes_file);
                println!(
                    "OK: px-mail {VERSION} | domains={} | mailboxes={} | state={}",
                    cfg.domains.len(),
                    doc.mailboxes.len(),
                    cfg.state_file.display()
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
    log_info(&format!(
        "PX-Mail v{VERSION} (Rust) started | domains={} | mailboxes={}",
        app.cfg.domains.len(),
        app.store.lock().unwrap().mailboxes.len()
    ));

    app.tg.set_my_commands(&[
        ("start", "Open dashboard"),
        ("menu", "Mail dashboard"),
        ("create", "Create a new email address"),
        ("generate", "Bulk generate addresses"),
        ("emails", "Manage saved emails"),
        ("inbox", "Read inbox & extract OTPs"),
        ("help", "Help & dashboard"),
    ]);
    let sync = app.sync_d1(false);
    app.tg.send(&app.home_text(&sync), Some(kb_main()));

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
