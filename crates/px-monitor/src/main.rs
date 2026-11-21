use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tg_core::bot::TgClient;
use tg_core::http::build_agent;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, format_int, hl, progress_bar, rows, stamp_wib,
    table_markdown, tg_button_row, tg_cb_button, truncate_str,
};
use tg_core::{log_error, log_info, log_warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0 Safari/537.36";

// ── konfigurasi (100% env-only) ─────────────────────────────────────────────
#[derive(Clone, Debug, PartialEq)]
struct HttpTarget {
    name: String,
    url: String,
}

#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    state_file: PathBuf,
    poll_seconds: u64,
    inferhub_key: String,
    inferhub_base: String,
    low_balance_usdc: f64,
    budget_min_pct: f64,
    budget_max_pct: f64,
    targets: Vec<HttpTarget>,
}

fn parse_targets(raw: &str) -> Vec<HttpTarget> {
    let mut out = Vec::new();
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if let Some((name, url)) = entry.split_once('|') {
            let n = name.trim();
            let u = url.trim();
            if !n.is_empty() && !u.is_empty() {
                out.push(HttpTarget {
                    name: n.to_string(),
                    url: u.to_string(),
                });
            }
        }
    }
    out
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let tg_token = env::var("TG_TOKEN")
            .or_else(|_| env::var("TELEGRAM_BOT_TOKEN"))
            .map_err(|_| "Env TG_TOKEN wajib diset".to_string())?;
        if tg_token.trim().is_empty() {
            return Err("Env TG_TOKEN kosong".to_string());
        }
        let owner_raw = env::var("OWNER_ID")
            .or_else(|_| env::var("TELEGRAM_OWNER_ID"))
            .map_err(|_| "Env OWNER_ID wajib diset".to_string())?;
        let owner_id = owner_raw
            .trim()
            .parse::<i64>()
            .map_err(|e| format!("OWNER_ID tidak valid: {e}"))?;

        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| "./state/px-monitor.json".to_string()),
        );
        let poll_seconds = env::var("POLL_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v >= 30)
            .unwrap_or(300);
        let inferhub_key = env::var("INFERHUB_API_KEY").unwrap_or_default();
        let inferhub_base = env::var("INFERHUB_BASE_URL")
            .unwrap_or_else(|_| "https://inferhub.dev/api".to_string())
            .trim_end_matches('/')
            .to_string();
        let low_balance_usdc = env::var("LOW_BALANCE_USDC")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.10);
        let budget_min_pct = env::var("BUDGET_MIN_PCT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(50.0);
        let budget_max_pct = env::var("BUDGET_MAX_PCT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(99.9);
        let targets = parse_targets(&env::var("MONITOR_TARGETS").unwrap_or_default());

        Ok(Self {
            tg_token,
            owner_id,
            state_file,
            poll_seconds,
            inferhub_key,
            inferhub_base,
            low_balance_usdc,
            budget_min_pct,
            budget_max_pct,
            targets,
        })
    }

    fn has_inferhub(&self) -> bool {
        !self.inferhub_key.trim().is_empty()
    }
}

// ── state ───────────────────────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    last_low: bool,
    #[serde(default)]
    last_error: String,
    #[serde(default)]
    target_down: HashMap<String, bool>,
}

#[derive(Debug, Clone)]
struct PendingBudget {
    pct: String,
    before: String,
}

// ── app context ─────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    tg: Arc<TgClient>,
    http: ureq::Agent,
    state: Mutex<BotState>,
    pending_budget: Mutex<Option<PendingBudget>>,
}

impl App {
    fn new(cfg: Config) -> Arc<Self> {
        let tg = TgClient::new(cfg.tg_token.clone(), cfg.owner_id);
        let http = build_agent(10, 25);
        let initial_state: BotState = load_json(&cfg.state_file);
        Arc::new(Self {
            cfg,
            tg,
            http,
            state: Mutex::new(initial_state),
            pending_budget: Mutex::new(None),
        })
    }

    fn persist_state(&self, s: &BotState) {
        save_json_atomic(&self.cfg.state_file, s);
    }

    fn ih_get(&self, path: &str) -> Result<Value, String> {
        if !self.cfg.has_inferhub() {
            return Err("INFERHUB_API_KEY belum diset".to_string());
        }
        let url = format!("{}{path}", self.cfg.inferhub_base);
        let resp = match self
            .http
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.cfg.inferhub_key))
            .set("Accept", "application/json")
            .set("User-Agent", UA)
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {}", truncate_str(&body, 160)));
            }
            Err(e) => return Err(format!("{e}")),
        };
        let raw = resp.into_string().map_err(|e| format!("{e}"))?;
        if raw.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&raw).map_err(|e| format!("JSON decode gagal: {e}"))
    }

    fn ih_put(&self, path: &str, payload: Value) -> Result<Value, String> {
        if !self.cfg.has_inferhub() {
            return Err("INFERHUB_API_KEY belum diset".to_string());
        }
        let url = format!("{}{path}", self.cfg.inferhub_base);
        let resp = match self
            .http
            .put(&url)
            .set("Authorization", &format!("Bearer {}", self.cfg.inferhub_key))
            .set("Accept", "application/json")
            .set("Content-Type", "application/json")
            .set("User-Agent", UA)
            .send_json(payload)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {}", truncate_str(&body, 160)));
            }
            Err(e) => return Err(format!("{e}")),
        };
        let raw = resp.into_string().map_err(|e| format!("{e}"))?;
        if raw.is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&raw).map_err(|e| format!("JSON decode gagal: {e}"))
    }

    fn probe_target(&self, t: &HttpTarget) -> (bool, String, u128) {
        let t0 = Instant::now();
        match self.http.get(&t.url).set("User-Agent", UA).call() {
            Ok(r) => {
                let code = r.status();
                let ms = t0.elapsed().as_millis();
                (code < 400, format!("HTTP {code}"), ms)
            }
            Err(ureq::Error::Status(code, _)) => {
                let ms = t0.elapsed().as_millis();
                (false, format!("HTTP {code}"), ms)
            }
            Err(e) => {
                let ms = t0.elapsed().as_millis();
                (false, truncate_str(&format!("{e}"), 36), ms)
            }
        }
    }

    // ── renderers Bot API 10.3+ ─────────────────────────────────────────────
    fn dashboard_text(&self) -> String {
        let ih_summary = if !self.cfg.has_inferhub() {
            "⏸️ Nonaktif".to_string()
        } else {
            match self.ih_get("/me") {
                Ok(me) => {
                    let st = me.get("status").and_then(Value::as_str).unwrap_or("unknown");
                    let bal = val_f64(me.pointer("/balances/consumer_balance"));
                    let icon = if st == "active" && bal > self.cfg.low_balance_usdc {
                        "🟢"
                    } else {
                        "⚠️"
                    };
                    format!("{} · {}", hl(&format!("{icon} {st}")), hl(&fmt_usdc(bal)))
                }
                Err(_) => hl("❌ Error"),
            }
        };

        let targets_summary = if self.cfg.targets.is_empty() {
            "Belum diset".to_string()
        } else {
            let ok_cnt = self
                .cfg
                .targets
                .iter()
                .filter(|t| self.probe_target(t).0)
                .count();
            let total = self.cfg.targets.len();
            if ok_cnt == total {
                hl(&format!("🟢 {ok_cnt}/{total} Online"))
            } else {
                hl(&format!("⚠️ {ok_cnt}/{total} Online"))
            }
        };

        let grid = field_grid(&[
            ("InferHub", ih_summary),
            ("Targets", targets_summary),
            ("Interval", hl(&format!("{} dtk", self.cfg.poll_seconds))),
            (
                "Threshold",
                format!("`{}`", fmt_usdc(self.cfg.low_balance_usdc)),
            ),
        ]);

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:menu"),
            tg_cb_button("📊 Status", "cmd:status"),
            tg_cb_button("📈 Usage", "cmd:usage"),
            tg_cb_button("🌐 Targets", "cmd:targets"),
        ]);

        let guide = table_markdown(
            &["Command", "Fungsi"],
            &[
                vec!["`/menu`".into(), "Dashboard utama".into()],
                vec!["`/status` · `/check`".into(), "Akun & balance InferHub".into()],
                vec!["`/usage` · `/today`".into(), "Pemakaian 30 hari & hari ini".into()],
                vec!["`/breakdown`".into(), "Rincian per model".into()],
                vec!["`/budget` · `/setbudget`".into(), "Diskon default akun".into()],
                vec!["`/keys`".into(), "Daftar API key".into()],
                vec!["`/targets`".into(), "Cek endpoint HTTP global".into()],
            ],
        );

        [
            format!("**📡 PX-Monitor** · {}", hl(&format!("v{VERSION}"))),
            String::new(),
            grid,
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Panduan & Command", &guide),
        ]
        .join("\n")
    }

    fn status_text(&self) -> Result<String, String> {
        let me = self.ih_get("/me")?;
        let name = me
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let status = me.get("status").and_then(Value::as_str).unwrap_or("unknown");
        let bal = val_f64(me.pointer("/balances/consumer_balance"));
        let pending = val_f64(me.pointer("/balances/fiat_pendings"));
        let ok_acc = status == "active";
        let ok_bal = bal > self.cfg.low_balance_usdc;

        let grid = field_grid(&[
            ("Account", format!("`{name}`")),
            (
                "Status",
                hl(&format!("{} {status}", if ok_acc { "✅" } else { "❌" })),
            ),
            (
                "Balance",
                hl(&format!(
                    "{} {}",
                    if ok_bal { "✅" } else { "⚠️" },
                    fmt_usdc(bal)
                )),
            ),
            ("Pending", format!("`{}`", fmt_usdc(pending))),
            (
                "Threshold",
                format!("`{}`", fmt_usdc(self.cfg.low_balance_usdc)),
            ),
        ]);

        let chk_acc = if ok_acc {
            "- [x] API account reachable & active"
        } else {
            "- [ ] API account tidak aktif"
        };
        let chk_bal = if ok_bal {
            "- [x] Balance di atas ambang aman"
        } else {
            "- [ ] Balance di bawah ambang aman — perlu top-up"
        };

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:status"),
            tg_cb_button("📈 Usage", "cmd:usage"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        Ok([
            format!("**📊 PX-Monitor · Account Status** · {}", hl("InferHub")),
            String::new(),
            grid,
            String::new(),
            chk_acc.to_string(),
            chk_bal.to_string(),
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .join("\n"))
    }

    fn usage_text(&self) -> Result<String, String> {
        let d = self.ih_get("/usage/logs?limit=25")?;
        let reqs = val_i64(d.get("rangeTotal").or_else(|| d.get("total")));
        let total_tokens = val_i64(d.get("totalTokens"));
        let total_cost = val_f64(d.get("totalCostUsdc"));
        let total_saved = val_f64(d.get("totalSavedUsdc"));

        let mut by_model: HashMap<String, (i64, i64, f64)> = HashMap::new();
        if let Some(arr) = d.get("rows").and_then(Value::as_array) {
            for r in arr {
                let m = r
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let entry = by_model.entry(m).or_insert((0, 0, 0.0));
                entry.0 += 1;
                entry.1 += val_i64(r.get("prompt_tokens")) + val_i64(r.get("completion_tokens"));
                entry.2 += val_f64(r.get("cost_consumer_usdc"));
            }
        }
        let mut sorted: Vec<_> = by_model.into_iter().collect();
        sorted.sort_by(|a, b| b.1 .1.cmp(&a.1 .1));
        sorted.truncate(8);

        let model_rows: Vec<Vec<String>> = if sorted.is_empty() {
            vec![vec!["—".into(), "0".into(), "0".into(), "0".into()]]
        } else {
            sorted
                .iter()
                .map(|(m, (c, tok, cost))| {
                    vec![
                        format!("`{}`", truncate_str(m, 22)),
                        format_int(*c),
                        format_int(*tok),
                        format!("{cost:.5}"),
                    ]
                })
                .collect()
        };

        let summary = field_grid(&[
            ("Requests", hl(&format_int(reqs))),
            ("Total Tokens", hl(&format_int(total_tokens))),
            ("Total Cost", hl(&fmt_usdc(total_cost))),
            ("Saved", hl(&fmt_usdc(total_saved))),
        ]);

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:usage"),
            tg_cb_button("📅 Hari Ini", "cmd:today"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        Ok([
            format!("**📈 PX-Monitor · Usage 30 Hari** · {}", hl("InferHub")),
            String::new(),
            summary,
            String::new(),
            "- [x] Usage API reachable".to_string(),
            "- [x] Token & cost terukur langsung dari server".to_string(),
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block(
                "Model Breakdown (25 log terakhir)",
                &table_markdown(&["Model", "Req", "Tokens", "USDC"], &model_rows),
            ),
        ]
        .join("\n"))
    }

    fn today_text(&self) -> Result<String, String> {
        let d = self.ih_get("/pricing/usage/today")?;
        let reqs = d.get("requests").and_then(Value::as_i64);
        let inp = val_i64(d.get("inputTokens"));
        let cached = val_i64(d.get("cachedInputTokens"));
        let out = val_i64(d.get("outputTokens"));
        let total_tok = inp + out;

        let mut pairs = Vec::new();
        if let Some(r) = reqs {
            pairs.push(("Requests", hl(&format_int(r))));
        }
        pairs.push(("Input", format!("`{}`", format_int(inp))));
        pairs.push(("Output", format!("`{}`", format_int(out))));
        pairs.push(("Total Tokens", hl(&format_int(total_tok))));
        if cached > 0 && inp > 0 {
            let ratio = cached as f64 / inp as f64;
            let pct = ratio * 100.0;
            pairs.push((
                "Cache Hit",
                format!(
                    "{} {} ({})",
                    progress_bar(ratio, 8),
                    hl(&format!("{pct:.1}%")),
                    format_int(cached)
                ),
            ));
        }

        let from = truncate_str(d.get("from").and_then(Value::as_str).unwrap_or("—"), 16)
            .replace('T', " ");
        let to =
            truncate_str(d.get("to").and_then(Value::as_str).unwrap_or("—"), 16).replace('T', " ");

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:today"),
            tg_cb_button("📋 Breakdown", "cmd:breakdown"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        Ok([
            format!("**📅 PX-Monitor · Hari Ini** · {}", hl("UTC 00:00")),
            String::new(),
            field_grid(&pairs),
            String::new(),
            format!("Rentang: `{from} → {to}`"),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .join("\n"))
    }

    fn breakdown_text(&self) -> Result<String, String> {
        let d = self.ih_get("/usage/breakdown")?;
        let rng = d.get("range").and_then(Value::as_str).unwrap_or("—");
        let arr = d
            .get("byModel")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut model_rows = Vec::new();
        if arr.is_empty() {
            model_rows.push(vec!["—".into(), "0".into(), "0".into(), "0".into()]);
        } else {
            for r in arr.iter().take(12) {
                let m = r
                    .get("label")
                    .or_else(|| r.get("key"))
                    .and_then(Value::as_str)
                    .unwrap_or("—");
                let reqs = val_i64(r.get("reqs"));
                let toks = val_i64(r.get("inputTokens")) + val_i64(r.get("outputTokens"));
                let cost = val_f64(r.get("costUsdc"));
                model_rows.push(vec![
                    format!("`{}`", truncate_str(m, 22)),
                    format_int(reqs),
                    format_int(toks),
                    format!("{cost:.5}"),
                ]);
            }
        }
        let extra = if arr.len() > 12 {
            format!("\n_…dan {} model lainnya._", arr.len() - 12)
        } else {
            String::new()
        };

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:breakdown"),
            tg_cb_button("📈 Usage", "cmd:usage"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        Ok([
            format!("**📋 PX-Monitor · Breakdown** · {}", hl(rng)),
            String::new(),
            table_markdown(&["Model", "Req", "Tokens", "USDC"], &model_rows),
            extra,
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
    }

    fn budget_text(&self) -> Result<String, String> {
        let d = self.ih_get("/budgets/default")?;
        let pct_str = val_as_string(d.get("minDiscountPct"));
        let wait = d.get("queueWaitSeconds");
        let total = val_i64(d.get("modelCount"));
        let covered = val_i64(d.get("coveredCount"));
        let gap = (total - covered).max(0);
        let pct_ok = pct_str
            .parse::<f64>()
            .map(|v| v >= self.cfg.budget_min_pct)
            .unwrap_or(false);
        let wait_txt = match wait {
            Some(Value::Number(n)) => format!("{n} dtk"),
            Some(Value::String(s)) if !s.is_empty() => format!("{s} dtk"),
            _ => "tanpa antre".to_string(),
        };
        let cov_ratio = if total > 0 {
            covered as f64 / total as f64
        } else {
            0.0
        };

        let grid = field_grid(&[
            (
                "Diskon Min",
                hl(&format!("{} {pct_str}%", if pct_ok { "✅" } else { "⚠️" })),
            ),
            (
                "Batas Aman",
                format!(
                    "`{} – {}%`",
                    self.cfg.budget_min_pct, self.cfg.budget_max_pct
                ),
            ),
            ("Queue Wait", format!("`{wait_txt}`")),
            (
                "Cakupan",
                format!(
                    "{} {}",
                    progress_bar(cov_ratio, 8),
                    hl(&format!("{covered}/{total}"))
                ),
            ),
        ]);

        let warn_line = if gap > 0 {
            format!("⚠️ **{gap} model belum ter-cover** oleh budget default.")
        } else {
            "✅ Seluruh model ter-cover budget default.".to_string()
        };

        Ok([
            format!("**💰 PX-Monitor · Budget Default** · {}", hl("InferHub")),
            String::new(),
            grid,
            String::new(),
            warn_line,
            format!("_{}_", stamp_wib()),
        ]
        .join("\n"))
    }

    fn keys_text(&self) -> Result<String, String> {
        let d = self.ih_get("/keys")?;
        let arr = if let Some(a) = d.as_array() {
            a.clone()
        } else {
            d.get("keys")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };

        let rows_data: Vec<Vec<String>> = if arr.is_empty() {
            vec![vec!["—".into(), "—".into(), "—".into()]]
        } else {
            arr.iter()
                .take(10)
                .map(|k| {
                    let nm = k.get("name").and_then(Value::as_str).unwrap_or("—");
                    let pre = k.get("keyPrefix").and_then(Value::as_str).unwrap_or("—");
                    let lu = k
                        .get("lastUsedAt")
                        .and_then(Value::as_str)
                        .map(|s| truncate_str(s, 16).replace('T', " "))
                        .unwrap_or_else(|| "belum".to_string());
                    vec![
                        format!("`{}`", truncate_str(nm, 18)),
                        format!("`{pre}…`"),
                        format!("`{lu}`"),
                    ]
                })
                .collect()
        };

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Segarkan", "cmd:keys"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        Ok([
            format!(
                "**🔑 PX-Monitor · API Keys** · {}",
                hl(&format!("{} terdaftar", arr.len()))
            ),
            String::new(),
            table_markdown(&["Nama", "Prefix", "Terakhir Dipakai"], &rows_data),
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .join("\n"))
    }

    fn targets_text(&self) -> String {
        if self.cfg.targets.is_empty() {
            return [
                format!("**🌐 PX-Monitor · Global Targets** · {}", hl("Kosong")),
                String::new(),
                "Belum ada endpoint tambahan di `MONITOR_TARGETS`.".to_string(),
                String::new(),
                format!("_{}_", stamp_wib()),
                String::new(),
                "---".to_string(),
                String::new(),
                details_block(
                    "Cara Tambah Target",
                    "Set variabel `MONITOR_TARGETS` di `runtime.env`:\n`MONITOR_TARGETS=Web|https://example.com;API|http://127.0.0.1:8080/health`",
                ),
            ]
            .join("\n");
        }

        let mut rows_data = Vec::new();
        let mut ok_cnt = 0usize;
        for t in &self.cfg.targets {
            let (ok, label, ms) = self.probe_target(t);
            if ok {
                ok_cnt += 1;
            }
            rows_data.push(vec![
                format!("**{}**", truncate_str(&t.name, 18)),
                if ok {
                    hl(&format!("✅ {label}"))
                } else {
                    hl(&format!("❌ {label}"))
                },
                format!("`{ms} ms`"),
            ]);
        }

        let pills = tg_button_row(&[
            tg_cb_button("🔄 Cek Ulang", "cmd:targets"),
            tg_cb_button("📁 Menu", "cmd:menu"),
        ]);

        [
            format!(
                "**🌐 PX-Monitor · Global Targets** · {}",
                hl(&format!("{ok_cnt}/{} Online", self.cfg.targets.len()))
            ),
            String::new(),
            table_markdown(&["Target", "Status", "Latency"], &rows_data),
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
        ]
        .join("\n")
    }

    fn parse_pct(&self, raw: &str) -> Result<String, String> {
        let v = raw
            .trim()
            .trim_end_matches('%')
            .parse::<f64>()
            .map_err(|_| "Angka tidak valid. Contoh: `/setbudget 95`".to_string())?;
        if v < self.cfg.budget_min_pct || v > self.cfg.budget_max_pct {
            return Err(format!(
                "Ditolak: harus antara `{}%` dan `{}%` (diberikan `{v}%`).",
                self.cfg.budget_min_pct, self.cfg.budget_max_pct
            ));
        }
        Ok(format!("{v:.2}"))
    }

    fn propose_budget(&self, pct: String, msg_id: Option<i64>) {
        match self.ih_get("/budgets/default") {
            Ok(cur) => {
                let before = val_as_string(cur.get("minDiscountPct"));
                let total = val_i64(cur.get("modelCount"));
                {
                    *self.pending_budget.lock().unwrap() = Some(PendingBudget {
                        pct: pct.clone(),
                        before: before.clone(),
                    });
                }
                let card = [
                    format!("**⚠️ PX-Monitor · Konfirmasi Budget** · {}", hl("Akun-Wide")),
                    String::new(),
                    field_grid(&[
                        ("Diskon Sekarang", format!("`{before}%`")),
                        ("Diskon Baru", hl(&format!("{pct}%"))),
                        ("Model Terdampak", format!("`{total}`")),
                    ]),
                    String::new(),
                    "Tekan tombol di bawah untuk menerapkan atau membatalkan.".to_string(),
                    format!("_{}_", stamp_wib()),
                ]
                .join("\n");
                self.tg.present(msg_id, &card, Some(kb_confirm(&pct)));
            }
            Err(e) => {
                self.tg
                    .present(msg_id, &error_card("Gagal baca budget", &e), Some(kb_main()));
            }
        }
    }

    fn periodic_check(&self) {
        // 1) InferHub balance & API health
        if self.cfg.has_inferhub() {
            match self.ih_get("/me") {
                Ok(me) => {
                    let bal = val_f64(me.pointer("/balances/consumer_balance"));
                    let low = bal <= self.cfg.low_balance_usdc;
                    let mut st = self.state.lock().unwrap();
                    if low && !st.last_low {
                        let card = [
                            format!("**⚠️ PX-Monitor · Balance Rendah** · {}", hl("Alert")),
                            String::new(),
                            field_grid(&[
                                ("Layanan", "InferHub".into()),
                                ("Balance", hl(&fmt_usdc(bal))),
                                ("Threshold", format!("`{}`", fmt_usdc(self.cfg.low_balance_usdc))),
                            ]),
                            String::new(),
                            format!("_{}_", stamp_wib()),
                        ]
                        .join("\n");
                        self.tg.send(&card, Some(kb_back()));
                    } else if !low && st.last_low {
                        let card = format!(
                            "**✅ PX-Monitor · Balance Pulih** · {}\n\nBalance InferHub kini {}.\n\n_{}_",
                            hl("Pulih"),
                            hl(&fmt_usdc(bal)),
                            stamp_wib()
                        );
                        self.tg.send(&card, None);
                    }
                    if !st.last_error.is_empty() {
                        st.last_error.clear();
                    }
                    st.last_low = low;
                    self.persist_state(&st);
                }
                Err(e) => {
                    let sig = truncate_str(&e, 60);
                    let mut st = self.state.lock().unwrap();
                    if st.last_error != sig {
                        let card = error_card("InferHub API Error (Periodic)", &e);
                        self.tg.send(&card, Some(kb_back()));
                        st.last_error = sig;
                        self.persist_state(&st);
                    }
                }
            }
        }

        // 2) Global HTTP targets
        for t in &self.cfg.targets {
            let (ok, label, ms) = self.probe_target(t);
            let mut st = self.state.lock().unwrap();
            let was_down = *st.target_down.get(&t.name).unwrap_or(&false);
            if !ok && !was_down {
                let card = [
                    format!("**❌ PX-Monitor · Target Down** · {}", hl(&t.name)),
                    String::new(),
                    field_grid(&[
                        ("Target", format!("**{}**", t.name)),
                        ("Status", hl(&format!("❌ {label}"))),
                        ("Latency", format!("`{ms} ms`")),
                    ]),
                    String::new(),
                    format!("_{}_", stamp_wib()),
                ]
                .join("\n");
                self.tg.send(&card, Some(kb_back()));
                st.target_down.insert(t.name.clone(), true);
                self.persist_state(&st);
            } else if ok && was_down {
                let card = format!(
                    "**✅ PX-Monitor · Target Pulih** · {}\n\n**{}** kembali online ({}, `{ms} ms`).\n\n_{}_",
                    hl("Pulih"),
                    t.name,
                    hl(&label),
                    stamp_wib()
                );
                self.tg.send(&card, None);
                st.target_down.insert(t.name.clone(), false);
                self.persist_state(&st);
            }
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────
fn val_f64(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn val_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn val_as_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "—".to_string(),
    }
}

fn fmt_usdc(val: f64) -> String {
    format!("{val:.6} USDC")
}

fn error_card(title: &str, err: &str) -> String {
    [
        format!("**❌ PX-Monitor · {title}** · {}", hl("Error")),
        String::new(),
        format!("_{}_", stamp_wib()),
        String::new(),
        "---".to_string(),
        String::new(),
        details_block("Detail Error", &format!("`{}`", truncate_str(err, 280))),
    ]
    .join("\n")
}

// ── keyboards ───────────────────────────────────────────────────────────────
fn kb_main() -> Value {
    json!({
        "inline_keyboard": rows(vec![
            button("📊 Status", "cmd:status", "primary"),
            button("📈 Usage", "cmd:usage", "primary"),
            button("📅 Hari Ini", "cmd:today", "primary"),
            button("📋 Breakdown", "cmd:breakdown", "primary"),
            button("💰 Budget", "cmd:budget", "primary"),
            button("🔑 Keys", "cmd:keys", "primary"),
            button("🌐 Targets", "cmd:targets", "primary"),
            button("🔄 Cek Semua", "cmd:menu", "success"),
        ])
    })
}

fn kb_back() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Menu Utama", "cmd:menu", "primary"),
            button("🔄 Cek Status", "cmd:status", ""),
        ]]
    })
}

fn kb_budget() -> Value {
    json!({
        "inline_keyboard": [
            [
                button("95%", "set:95.00", "primary"),
                button("90%", "set:90.00", "primary"),
                button("85%", "set:85.00", "primary"),
            ],
            [
                button("98%", "set:98.00", ""),
                button("99%", "set:99.00", ""),
            ],
            [button("📁 Menu Utama", "cmd:menu", "primary")],
        ]
    })
}

fn kb_confirm(pct: &str) -> Value {
    json!({
        "inline_keyboard": [
            [
                button(&format!("✅ Terapkan {pct}%"), "do:confirm", "success"),
                button("❌ Batal", "do:cancel", "danger"),
            ],
            [button("📁 Menu Utama", "cmd:menu", "primary")],
        ]
    })
}

// ── router aksi ─────────────────────────────────────────────────────────────
fn run_action(app: &Arc<App>, action: &str, msg_id: Option<i64>) {
    match action {
        "menu" | "start" | "help" => {
            app.tg.present(msg_id, &app.dashboard_text(), Some(kb_main()));
        }
        "status" | "check" => match app.status_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_back()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Status Gagal", &e), Some(kb_main()));
            }
        },
        "usage" => match app.usage_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_back()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Usage Gagal", &e), Some(kb_main()));
            }
        },
        "today" => match app.today_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_back()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Hari Ini Gagal", &e), Some(kb_main()));
            }
        },
        "breakdown" => match app.breakdown_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_back()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Breakdown Gagal", &e), Some(kb_main()));
            }
        },
        "keys" => match app.keys_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_back()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Keys Gagal", &e), Some(kb_main()));
            }
        },
        "targets" => {
            app.tg.present(msg_id, &app.targets_text(), Some(kb_back()));
        }
        "budget" => match app.budget_text() {
            Ok(txt) => {
                app.tg.present(msg_id, &txt, Some(kb_budget()));
            }
            Err(e) => {
                app.tg
                    .present(msg_id, &error_card("Budget Gagal", &e), Some(kb_main()));
            }
        },
        "cancel" => {
            app.pending_budget.lock().unwrap().take();
            app.tg.present(msg_id, &app.dashboard_text(), Some(kb_main()));
        }
        "confirm" => {
            let pending = app.pending_budget.lock().unwrap().take();
            let Some(p) = pending else {
                app.tg.present(msg_id, &app.dashboard_text(), Some(kb_main()));
                return;
            };
            match app
                .ih_put("/budgets/default", json!({"minDiscountPct": p.pct}))
                .and_then(|_| app.ih_get("/budgets/default"))
            {
                Ok(after) => {
                    let card = [
                        format!("**✅ PX-Monitor · Budget Diterapkan** · {}", hl("Selesai")),
                        String::new(),
                        field_grid(&[
                            ("Sebelum", format!("`{}%`", p.before)),
                            ("Sesudah", hl(&format!("{}%", val_as_string(after.get("minDiscountPct"))))),
                            (
                                "Model Ter-cover",
                                hl(&format!(
                                    "{} / {}",
                                    val_i64(after.get("coveredCount")),
                                    val_i64(after.get("modelCount"))
                                )),
                            ),
                        ]),
                        String::new(),
                        format!("_{}_", stamp_wib()),
                    ]
                    .join("\n");
                    app.tg.present(msg_id, &card, Some(kb_main()));
                }
                Err(e) => {
                    app.tg
                        .present(msg_id, &error_card("Terapkan Budget Gagal", &e), Some(kb_main()));
                }
            }
        }
        _ => {
            app.tg.present(msg_id, &app.dashboard_text(), Some(kb_main()));
        }
    }
}

fn handle_callback(app: &Arc<App>, cb: &Value) {
    let cb_id = cb.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
    let data = cb.get("data").and_then(Value::as_str).unwrap_or_default().trim();
    let chat_id = cb.pointer("/message/chat/id").and_then(Value::as_i64).unwrap_or(0);
    let msg_id = cb.pointer("/message/message_id").and_then(Value::as_i64);

    if !app.tg.is_owner(chat_id) {
        app.tg.ack(cb_id, "Akses khusus owner.");
        return;
    }

    if let Some(act) = data.strip_prefix("cmd:") {
        app.tg.ack(cb_id, "");
        run_action(app, act, msg_id);
    } else if let Some(pct) = data.strip_prefix("set:") {
        match app.parse_pct(pct) {
            Ok(val) => {
                app.tg.ack(cb_id, &format!("Konfirmasi {val}%"));
                app.propose_budget(val, msg_id);
            }
            Err(err) => {
                app.tg.ack(cb_id, "Nilai tidak valid");
                app.tg.present(
                    msg_id,
                    &format!("**❌ Ditolak**\n\n{err}"),
                    Some(kb_budget()),
                );
            }
        }
    } else if let Some(act) = data.strip_prefix("do:") {
        app.tg.ack(cb_id, "");
        run_action(app, act, msg_id);
    } else {
        app.tg.ack(cb_id, "");
        run_action(app, "menu", msg_id);
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

    let mut parts = text.split_whitespace();
    let raw_cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let cmd = raw_cmd.split('@').next().unwrap_or("");
    let arg = parts.next().unwrap_or("");

    if cmd == "/setbudget" {
        if arg.is_empty() {
            app.tg.send(
                "**❌ Ditolak**\n\nButuh angka. Contoh: `/setbudget 95`",
                Some(kb_budget()),
            );
            return;
        }
        match app.parse_pct(arg) {
            Ok(val) => app.propose_budget(val, None),
            Err(err) => {
                app.tg
                    .send(&format!("**❌ Ditolak**\n\n{err}"), Some(kb_budget()));
            }
        }
    } else if let Some(sub) = cmd.strip_prefix('/') {
        run_action(app, sub, None);
    } else {
        run_action(app, "menu", None);
    }
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-monitor {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env() {
            Ok(cfg) => {
                println!(
                    "OK: px-monitor {VERSION} | inferhub={} | targets={} | poll={}s | state={}",
                    cfg.has_inferhub(),
                    cfg.targets.len(),
                    cfg.poll_seconds,
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
        "PX-Monitor v{VERSION} (Rust) mulai | inferhub={} | targets={}",
        app.cfg.has_inferhub(),
        app.cfg.targets.len()
    ));

    app.tg.set_my_commands(&[
        ("menu", "Dashboard monitor utama"),
        ("status", "Akun & balance InferHub"),
        ("usage", "Pemakaian 30 hari"),
        ("today", "Pemakaian hari ini"),
        ("breakdown", "Rincian per model"),
        ("budget", "Budget default akun"),
        ("setbudget", "Ubah diskon default"),
        ("keys", "Daftar API key"),
        ("targets", "Cek endpoint HTTP global"),
        ("check", "Cek live sekarang"),
        ("help", "Bantuan"),
    ]);
    app.tg.send(&app.dashboard_text(), Some(kb_main()));

    // Background periodic checker (tidak memblokir long-polling Telegram)
    let bg = Arc::clone(&app);
    thread::spawn(move || loop {
        bg.periodic_check();
        thread::sleep(Duration::from_secs(bg.cfg.poll_seconds));
    });

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
    fn test_parse_targets() {
        let t = parse_targets("Web|https://example.com/health; API | http://127.0.0.1:8080/ping ; invalid");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].name, "Web");
        assert_eq!(t[0].url, "https://example.com/health");
        assert_eq!(t[1].name, "API");
        assert_eq!(t[1].url, "http://127.0.0.1:8080/ping");
    }

    #[test]
    fn test_fmt_usdc_and_vals() {
        assert_eq!(fmt_usdc(1.25), "1.250000 USDC");
        assert_eq!(val_i64(Some(&json!("42"))), 42);
        assert_eq!(val_f64(Some(&json!("0.15"))), 0.15);
    }
}
