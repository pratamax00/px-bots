use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tg_core::http::build_api_agent;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::tail_str;

const MAX_CMD_OUTPUT: usize = 512 * 1024;
const MAX_JOB_LOG: usize = 1024 * 1024;

// ── provider catalog ────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub ready: bool,
    pub workers_dynamic: bool,
    pub workers_effective_fixed: Option<u32>,
    pub domain_aware: bool,
}

pub const PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec {
        id: "grok",
        label: "Grok",
        ready: true,
        workers_dynamic: false,
        workers_effective_fixed: Some(6),
        domain_aware: false,
    },
    ProviderSpec {
        id: "template",
        label: "Template",
        ready: false,
        workers_dynamic: true,
        workers_effective_fixed: None,
        domain_aware: true,
    },
];

pub fn get_provider(id: &str) -> Option<&'static ProviderSpec> {
    PROVIDERS.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

pub fn effective_workers(provider: Option<&ProviderSpec>, requested: u32) -> (u32, u32, String) {
    let req = requested.max(1);
    if let Some(p) = provider {
        if !p.workers_dynamic {
            if let Some(fixed) = p.workers_effective_fixed {
                return (
                    req,
                    fixed,
                    format!("provider bridge uses fixed {fixed} workers"),
                );
            }
        }
    }
    (req, req, "dynamic workers".to_string())
}

// ── CLI runner with strict timeout ──────────────────────────────────────────
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CmdResult {
    pub exit_code: i32,
    pub output: String,
    pub json: Option<Value>,
    pub duration_ms: u64,
    pub timed_out: bool,
}

fn read_capped<R: Read>(mut reader: R, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < cap {
                    let take = n.min(cap - out.len());
                    out.extend_from_slice(&buf[..take]);
                }
            }
            Err(_) => break,
        }
    }
    out
}

pub fn parse_json_loose(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    if let Some(start) = trimmed.rfind('\n') {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed[start..].trim()) {
            return Some(v);
        }
    }
    if let (Some(a), Some(b)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if a < b {
            if let Ok(v) = serde_json::from_str::<Value>(&trimmed[a..=b]) {
                return Some(v);
            }
        }
    }
    None
}

pub fn run_af(af_bin: &Path, af_root: &Path, args: &[String], timeout_secs: u64) -> CmdResult {
    let t0 = Instant::now();
    let mut child = match Command::new(af_bin)
        .args(args)
        .current_dir(af_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CmdResult {
                exit_code: 127,
                output: format!("Failed to spawn {}: {e}", af_bin.display()),
                json: None,
                duration_ms: t0.elapsed().as_millis() as u64,
                timed_out: false,
            };
        }
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let t_out = thread::spawn(move || {
        stdout_pipe
            .map(|r| read_capped(r, MAX_CMD_OUTPUT))
            .unwrap_or_default()
    });
    let t_err = thread::spawn(move || {
        stderr_pipe
            .map(|r| read_capped(r, MAX_CMD_OUTPUT / 2))
            .unwrap_or_default()
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(5));
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break 124;
                }
                thread::sleep(Duration::from_millis(80));
            }
            Err(_) => break 1,
        }
    };

    let out_bytes = t_out.join().unwrap_or_default();
    let err_bytes = t_err.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&out_bytes).to_string();
    let stderr = String::from_utf8_lossy(&err_bytes).to_string();

    let json = parse_json_loose(&stdout);
    let combined = if stdout.trim().is_empty() {
        stderr
    } else if stderr.trim().is_empty() || json.is_some() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };

    CmdResult {
        exit_code,
        output: if timed_out {
            format!("Timed out after {timeout_secs}s\n{combined}")
        } else {
            combined
        },
        json,
        duration_ms: t0.elapsed().as_millis() as u64,
        timed_out,
    }
}

pub fn smoke_args(
    provider: &str,
    target: u32,
    workers: u32,
    max_rounds: u32,
    domain: Option<&str>,
    dry: bool,
) -> Vec<String> {
    let mut args = vec![
        "registrar".to_string(),
        "smoke".to_string(),
        "--provider".to_string(),
        provider.to_string(),
        "--target".to_string(),
        target.max(1).to_string(),
        "--workers".to_string(),
        workers.max(1).to_string(),
        "--max-rounds".to_string(),
        max_rounds.max(1).to_string(),
    ];
    if let Some(d) = domain.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--domain".to_string());
        args.push(d.to_string());
    }
    args.push("--no-notify".to_string());
    args.push("--json".to_string());
    if dry {
        args.push("--dry-run".to_string());
    }
    args
}

// ── pool classification & reprio planner ────────────────────────────────────
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClassifiedItem {
    pub id: String,
    pub email: String,
    pub priority: i64,
    pub test_status: String,
    pub last_error: String,
    pub usable_class: &'static str,
    pub usable_code: String,
    pub locks: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PoolSummary {
    pub total: usize,
    pub fresh: usize,
    pub transient: usize,
    pub quota: usize,
    pub denied: usize,
    pub other: usize,
    pub unknown: usize,
    pub health: &'static str,
    pub counts_line: String,
    pub hints: Vec<String>,
    pub items: Vec<ClassifiedItem>,
}

pub fn classify_pool_item(item: &Value) -> (&'static str, String, Vec<String>) {
    static RE_CODE: OnceLock<Regex> = OnceLock::new();
    let re_code = RE_CODE.get_or_init(|| Regex::new(r"\[(\d{3})\]").unwrap());

    let ec = item
        .get("errorCode")
        .or_else(|| item.get("error_code"))
        .and_then(|v| {
            v.as_str()
                .map(ToString::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .unwrap_or_default()
        .trim()
        .to_string();
    let le = item
        .get("lastError")
        .or_else(|| item.get("last_error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let low = le.to_ascii_lowercase();

    let mut locks = Vec::new();
    if let Some(map) = item.as_object() {
        for (k, v) in map {
            if k.starts_with("modelLock_") && v.as_bool().unwrap_or(false) {
                locks.push(k.clone());
            }
        }
    }

    let code_in_text = re_code
        .captures(le)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let code = if !ec.is_empty() { ec } else { code_in_text };

    if matches!(code.as_str(), "402" | "429")
        || low.contains("exhausted")
        || low.contains("run out of credits")
        || low.contains("quota")
    {
        return (
            "quota",
            if code.is_empty() { "quota".into() } else { code },
            locks,
        );
    }
    if matches!(code.as_str(), "401" | "403")
        || low.contains("permission-denied")
        || low.contains("unauthorized")
        || low.contains("auth")
    {
        return (
            "denied",
            if code.is_empty() { "denied".into() } else { code },
            locks,
        );
    }
    if matches!(code.as_str(), "502" | "503")
        || low.contains("overloaded")
        || low.contains("bad gateway")
        || low.contains("service unavailable")
    {
        return (
            "transient",
            if code.is_empty() { "503".into() } else { code },
            locks,
        );
    }
    if code.is_empty() && locks.is_empty() && le.trim().is_empty() {
        let ts = item
            .get("testStatus")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ts == "unavailable" {
            return ("unknown", "-".into(), locks);
        }
        return ("fresh", "-".into(), locks);
    }
    let display_code = if code.is_empty() { "-".into() } else { code };
    ("other", display_code, locks)
}

pub fn summarize_pool_items(raw_items: &[Value]) -> PoolSummary {
    let mut fresh = 0usize;
    let mut transient = 0usize;
    let mut quota = 0usize;
    let mut denied = 0usize;
    let mut other = 0usize;
    let mut unknown = 0usize;
    let mut items = Vec::with_capacity(raw_items.len());

    for it in raw_items {
        let (cls, code, locks) = classify_pool_item(it);
        match cls {
            "fresh" => fresh += 1,
            "transient" => transient += 1,
            "quota" => quota += 1,
            "denied" => denied += 1,
            "unknown" => unknown += 1,
            _ => other += 1,
        }
        let id = it
            .get("id")
            .or_else(|| it.get("connectionId"))
            .or_else(|| it.get("connection_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let email = it
            .get("email")
            .or_else(|| it.get("identity"))
            .or_else(|| it.get("displayName"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let priority = it.get("priority").and_then(Value::as_i64).unwrap_or(999);
        let test_status = it
            .get("testStatus")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        let last_error = it
            .get("lastError")
            .or_else(|| it.get("last_error"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        items.push(ClassifiedItem {
            id,
            email,
            priority,
            test_status,
            last_error,
            usable_class: cls,
            usable_code: code,
            locks,
        });
    }

    let total = items.len();
    let mut by_prio = items.clone();
    by_prio.sort_by_key(|x| x.priority);
    let bad_front = by_prio
        .iter()
        .take(4)
        .filter(|x| matches!(x.usable_class, "quota" | "denied" | "transient"))
        .count();

    let health = if total == 0 {
        "empty"
    } else if fresh == 0 {
        "attention"
    } else if bad_front >= 2 || (quota + denied) >= total.div_ceil(2) {
        "degraded"
    } else if quota > 0 || denied > 0 || transient > 0 {
        "watch"
    } else {
        "healthy"
    };

    let mut parts = Vec::new();
    for (k, v) in [
        ("fresh", fresh),
        ("transient", transient),
        ("quota", quota),
        ("denied", denied),
        ("other", other),
        ("unknown", unknown),
    ] {
        if v > 0 {
            parts.push(format!("{k} {v}"));
        }
    }
    let counts_line = if parts.is_empty() {
        "empty".to_string()
    } else {
        parts.join(" · ")
    };

    let mut hints = Vec::new();
    if quota > 0 || denied > 0 {
        hints.push("Archive quota/denied accounts, then run /delete batch <id>".to_string());
    }
    if bad_front >= 2 {
        hints.push("Front priorities degraded — run Reprio to move fresh accounts to p1".to_string());
    }
    if fresh > 0 && fresh <= 3 {
        hints.push("Fresh stock is running low".to_string());
    }
    if hints.is_empty() {
        hints.push("Usable pool looks healthy — no farm required".to_string());
    }

    PoolSummary {
        total,
        fresh,
        transient,
        quota,
        denied,
        other,
        unknown,
        health,
        counts_line,
        hints,
        items,
    }
}

pub fn extract_pool_items(json: Option<&Value>) -> Vec<Value> {
    let Some(v) = json else {
        return Vec::new();
    };
    v.get("items")
        .or_else(|| v.get("connections"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReprioRow {
    pub id: String,
    pub email: String,
    pub usable_class: String,
    pub usable_code: String,
    pub old_priority: i64,
    pub new_priority: i64,
    pub changed: bool,
}

fn class_rank(c: &str) -> u8 {
    match c {
        "fresh" => 0,
        "transient" => 1,
        "other" => 2,
        "unknown" => 3,
        "quota" => 4,
        "denied" => 5,
        _ => 9,
    }
}

pub fn plan_reprio_from_items(items: &[ClassifiedItem]) -> Vec<ReprioRow> {
    let mut ordered = items.to_vec();
    ordered.sort_by_key(|it| (class_rank(it.usable_class), it.priority));
    ordered
        .into_iter()
        .enumerate()
        .map(|(idx, it)| {
            let new_p = (idx + 1) as i64;
            let old_p = if it.priority == 999 { 0 } else { it.priority };
            ReprioRow {
                id: it.id,
                email: it.email,
                usable_class: it.usable_class.to_string(),
                usable_code: it.usable_code,
                old_priority: old_p,
                new_priority: new_p,
                changed: old_p != new_p,
            }
        })
        .collect()
}

pub fn apply_reprio_router(
    router_env_file: &Path,
    router_backup_dir: Option<&Path>,
    changes: &[ReprioRow],
) -> Result<Value, String> {
    let raw_env = fs::read_to_string(router_env_file)
        .map_err(|e| format!("Cannot read router env file: {e}"))?;
    let mut env_map = HashMap::new();
    for line in raw_env.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            env_map.insert(
                k.trim().to_string(),
                v.trim().trim_matches('"').trim_matches('\'').to_string(),
            );
        }
    }
    let base_url = env_map
        .get("BASE_URL")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:20128".to_string())
        .trim_end_matches('/')
        .to_string();
    let password = env_map
        .get("INITIAL_PASSWORD")
        .cloned()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "INITIAL_PASSWORD not found in router env file".to_string())?;

    let ts = utc_compact_stamp();
    let mut backup_path_str = String::new();
    if let Some(bdir) = router_backup_dir {
        let _ = fs::create_dir_all(bdir);
        let bpath = bdir.join(format!("9router-pre-reprio-pxabuse-{ts}.json"));
        let backup_doc = json!({"at": ts, "changes": changes});
        save_json_atomic(&bpath, &backup_doc);
        backup_path_str = bpath.display().to_string();
    }

    let agent = build_api_agent(20);
    let login_url = format!("{base_url}/api/auth/login");
    let login_resp = agent
        .post(&login_url)
        .set("Content-Type", "application/json")
        .set("Connection", "close")
        .send_json(json!({"password": password}))
        .map_err(|e| format!("Router login failed: {e}"))?;

    let cookies: Vec<String> = login_resp
        .all("set-cookie")
        .into_iter()
        .filter_map(|hdr| hdr.split(';').next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    let cookie_header = cookies.join("; ");
    let _ = login_resp.into_string();
    if cookie_header.is_empty() {
        return Err("Router login returned no session cookie".to_string());
    }

    let mut ok_count = 0usize;
    let mut fail_count = 0usize;
    let mut results = Vec::with_capacity(changes.len());
    for ch in changes {
        let url = format!("{base_url}/api/providers/{}", ch.id);
        match agent
            .put(&url)
            .set("Content-Type", "application/json")
            .set("Cookie", &cookie_header)
            .set("Connection", "close")
            .send_json(json!({"priority": ch.new_priority}))
        {
            Ok(r) => {
                let status = r.status();
                let _ = r.into_string();
                ok_count += 1;
                results.push(json!({
                    "id": ch.id,
                    "email": ch.email,
                    "ok": true,
                    "status": status,
                }));
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                fail_count += 1;
                results.push(json!({
                    "id": ch.id,
                    "email": ch.email,
                    "ok": false,
                    "status": code,
                    "error": body.chars().take(180).collect::<String>(),
                }));
            }
            Err(e) => {
                fail_count += 1;
                results.push(json!({
                    "id": ch.id,
                    "email": ch.email,
                    "ok": false,
                    "error": format!("{e}"),
                }));
            }
        }
    }

    Ok(json!({
        "ok": fail_count == 0,
        "updated_ok": ok_count,
        "failed": fail_count,
        "total_changes": changes.len(),
        "backup": backup_path_str,
        "results": results,
    }))
}

// ── pending CPA scanner (pure Rust) ─────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCpaItem {
    pub farm: String,
    pub email: String,
    pub file: String,
}

pub fn list_pending_cpa(pending_root: Option<&Path>, limit: usize) -> (usize, Vec<PendingCpaItem>) {
    let Some(root) = pending_root else {
        return (0, Vec::new());
    };
    let farms_dir = root.join("account_lists").join("farm");
    let Ok(entries) = fs::read_dir(&farms_dir) else {
        return (0, Vec::new());
    };

    let mut farms: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let meta = e.metadata().ok()?;
            if meta.is_dir() {
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                Some((mtime, p))
            } else {
                None
            }
        })
        .collect();
    farms.sort_by(|a, b| b.0.cmp(&a.0));

    let mut rows = Vec::new();
    for (_, farm_path) in farms {
        let farm_name = farm_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let pend_dir = farm_path.join("pending_cpa");
        let Ok(files) = fs::read_dir(&pend_dir) else {
            continue;
        };
        let mut cpa_files: Vec<PathBuf> = files
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("xai-") && n.ends_with(".json"))
                    .unwrap_or(false)
            })
            .collect();
        cpa_files.sort();

        for f in cpa_files {
            let fname = f
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let email = fs::read_to_string(&f)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("email").and_then(Value::as_str).map(ToString::to_string))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    fname
                        .trim_start_matches("xai-")
                        .trim_end_matches(".json")
                        .to_string()
                });
            rows.push(PendingCpaItem {
                farm: farm_name.clone(),
                email,
                file: f.display().to_string(),
            });
        }
    }

    let total = rows.len();
    rows.truncate(limit);
    (total, rows)
}

// ── archive batch manifest helpers (pure Rust) ──────────────────────────────
pub fn is_safe_batch_id(batch_id: &str) -> bool {
    !batch_id.is_empty()
        && !batch_id.contains("..")
        && batch_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[derive(Debug, Clone)]
pub struct BatchManifest {
    pub batch_id: String,
    pub ids: Vec<String>,
    pub deleted_from_pool: bool,
}

pub fn load_batch_ids(af_root: &Path, batch_id: &str) -> Result<BatchManifest, String> {
    if !is_safe_batch_id(batch_id) {
        return Err("Invalid batch ID format".to_string());
    }
    let path = af_root
        .join("data")
        .join("archive")
        .join("batches")
        .join(batch_id)
        .join("manifest.json");
    if !path.exists() {
        return Err(format!("Batch `{batch_id}` manifest not found"));
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("Read manifest failed: {e}"))?;
    let val: Value =
        serde_json::from_str(&text).map_err(|e| format!("Manifest JSON invalid: {e}"))?;
    let ids: Vec<String> = val
        .get("rows")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| r.get("id").and_then(Value::as_str).map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();
    let deleted_from_pool = val
        .get("deleted_from_pool")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(BatchManifest {
        batch_id: batch_id.to_string(),
        ids,
        deleted_from_pool,
    })
}

pub fn mark_batch_deleted(af_root: &Path, batch_id: &str, deleted_count: usize) -> Result<(), String> {
    if !is_safe_batch_id(batch_id) {
        return Err("Invalid batch ID".to_string());
    }
    let dir = af_root
        .join("data")
        .join("archive")
        .join("batches")
        .join(batch_id);
    let path = dir.join("manifest.json");
    let text = fs::read_to_string(&path).map_err(|e| format!("Read manifest failed: {e}"))?;
    let mut val: Value =
        serde_json::from_str(&text).map_err(|e| format!("Manifest JSON invalid: {e}"))?;
    if let Some(obj) = val.as_object_mut() {
        obj.insert("deleted_from_pool".into(), json!(true));
        obj.insert("deleted_count".into(), json!(deleted_count));
        obj.insert("deleted_at".into(), json!(utc_iso_now()));
    }
    save_json_atomic(&path, &val);
    let readme = format!(
        "# Archive {batch_id}\n\n- deleted_from_pool: true\n- deleted_count: {deleted_count}\n"
    );
    let _ = fs::write(dir.join("README.md"), readme);
    Ok(())
}

// ── honest farm output parser ───────────────────────────────────────────────
#[derive(Debug, Clone, Default)]
pub struct FarmSummary {
    pub pass: Option<u64>,
    pub fail: Option<u64>,
    pub attempts: Option<u64>,
    pub target: Option<u64>,
    pub farm_status: Option<String>,
    pub elapsed_s: Option<u64>,
    pub outcome: &'static str,
}

pub fn parse_job_farm_summary(text: &str) -> FarmSummary {
    static RE_ROUND: OnceLock<Regex> = OnceLock::new();
    static RE_FINAL: OnceLock<Regex> = OnceLock::new();
    let re_round = RE_ROUND
        .get_or_init(|| Regex::new(r"(?i)round\s+\d+\s+summary[^\n]*\bpass=(\d+)\s+fail=(\d+)").unwrap());
    let re_final = RE_FINAL.get_or_init(|| Regex::new(r"\[farm\]\s*FINAL\s+(\{[^\n]*\})").unwrap());

    let bridge = parse_json_loose(text);
    let stdout = bridge
        .as_ref()
        .and_then(|b| b.get("stdout_tail").and_then(Value::as_str))
        .unwrap_or(text);

    let final_obj: Option<Value> = re_final
        .captures_iter(stdout)
        .last()
        .and_then(|c| c.get(1))
        .and_then(|m| serde_json::from_str::<Value>(m.as_str()).ok());
    let last_round = re_round.captures_iter(stdout).last();

    let pass = final_obj
        .as_ref()
        .and_then(|f| f.get("total_new").and_then(Value::as_u64))
        .or_else(|| {
            last_round
                .as_ref()
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u64>().ok())
        });
    let fail = last_round
        .as_ref()
        .and_then(|c| c.get(2))
        .and_then(|m| m.as_str().parse::<u64>().ok());
    let attempts = final_obj.as_ref().and_then(|f| {
        f.get("attempts")
            .or_else(|| f.get("attempt_count"))
            .and_then(Value::as_u64)
    });
    let target = final_obj
        .as_ref()
        .and_then(|f| f.get("target_new").and_then(Value::as_u64))
        .or_else(|| {
            bridge
                .as_ref()
                .and_then(|b| b.get("target").and_then(Value::as_u64))
        });
    let farm_status = final_obj
        .as_ref()
        .and_then(|f| f.get("status").and_then(Value::as_str).map(ToString::to_string));
    let elapsed_s = bridge
        .as_ref()
        .and_then(|b| b.get("elapsed_s").and_then(Value::as_f64).map(|x| x.round() as u64));

    let outcome = match pass {
        Some(0) => "zero_pass",
        Some(p) => match target {
            Some(t) if p >= t => "hit_target",
            _ => "partial",
        },
        None => "unknown",
    };

    FarmSummary {
        pass,
        fail,
        attempts,
        target,
        farm_status,
        elapsed_s,
        outcome,
    }
}

pub fn format_farm_summary_line(s: &FarmSummary) -> Option<String> {
    let mut bits = Vec::new();
    if let Some(p) = s.pass {
        bits.push(format!("pass {p}"));
    }
    if let Some(f) = s.fail {
        bits.push(format!("fail {f}"));
    }
    if let Some(t) = s.target {
        bits.push(format!("target {t}"));
    }
    if let Some(a) = s.attempts {
        bits.push(format!("attempts {a}"));
    }
    if let Some(ref st) = s.farm_status {
        bits.push(st.clone());
    }
    if let Some(el) = s.elapsed_s {
        bits.push(format!("{el}s"));
    }
    if bits.is_empty() {
        None
    } else {
        Some(bits.join(" · "))
    }
}

pub fn extract_interesting_log_tail(text: &str, max_chars: usize) -> String {
    static RE_INTERESTING: OnceLock<Regex> = OnceLock::new();
    let re = RE_INTERESTING.get_or_init(|| {
        Regex::new(r"(?i)\[farm\]|round\s+\d+|pass=|fail=|Xvfb|FINAL|error|timeout|proxy|smoke|workers|target").unwrap()
    });
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let interesting: Vec<&str> = lines.iter().copied().filter(|l| re.is_match(l)).collect();
    let picked = if !interesting.is_empty() {
        let start = interesting.len().saturating_sub(8);
        interesting[start..].join("\n")
    } else {
        let start = lines.len().saturating_sub(6);
        lines[start..].join("\n")
    };
    if picked.is_empty() {
        "(waiting for output…)".to_string()
    } else {
        tail_str(&picked, max_chars)
    }
}

// ── normalized import result ────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub ok: bool,
    pub connection_id: Option<String>,
    pub test_valid: Option<bool>,
    pub email: String,
    pub error: Option<String>,
}

pub fn normalize_import_result(
    res: &CmdResult,
    email_hint: &str,
) -> ImportOutcome {
    let data = res.json.as_ref();
    let connection_id = data
        .and_then(|d| {
            d.get("id")
                .or_else(|| d.get("connectionId"))
                .or_else(|| d.get("connection_id"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        });
    let test_valid = data.and_then(|d| {
        d.get("test_valid")
            .or_else(|| d.get("testValid"))
            .or_else(|| d.pointer("/test/valid"))
            .and_then(Value::as_bool)
    });
    let ok = data
        .and_then(|d| d.get("ok").and_then(Value::as_bool))
        .unwrap_or_else(|| {
            let added = data
                .and_then(|d| d.get("action").and_then(Value::as_str))
                .map(|a| a == "added")
                .unwrap_or(false);
            added || connection_id.is_some() || res.exit_code == 0
        });
    let email = data
        .and_then(|d| d.get("email").and_then(Value::as_str).map(ToString::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| email_hint.to_string());
    let error = data.and_then(|d| {
        d.get("error")
            .or_else(|| d.get("message"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });

    ImportOutcome {
        ok,
        connection_id,
        test_valid,
        email,
        error,
    }
}

// ── background job manager ──────────────────────────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    pub label: String,
    pub command: String,
    pub status: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub started_at: String,
    #[serde(default)]
    pub started_epoch: u64,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub output_file: String,
    #[serde(default)]
    pub progress_message_id: Option<i64>,
}

pub struct JobManager {
    jobs_dir: PathBuf,
    index_file: PathBuf,
    jobs: Mutex<HashMap<String, JobRecord>>,
    live_pids: Mutex<HashMap<String, u32>>,
    seq: AtomicU64,
}

impl JobManager {
    pub fn new(jobs_dir: PathBuf) -> Arc<Self> {
        let _ = fs::create_dir_all(&jobs_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&jobs_dir, fs::Permissions::from_mode(0o700));
        }
        let index_file = jobs_dir.join("jobs.json");
        let mut stored: HashMap<String, JobRecord> = load_json(&index_file);
        let mut changed = false;
        for job in stored.values_mut() {
            if job.status == "running" {
                job.status = "interrupted".to_string();
                changed = true;
            }
        }
        if changed {
            save_json_atomic(&index_file, &stored);
        }
        Arc::new(Self {
            jobs_dir,
            index_file,
            jobs: Mutex::new(stored),
            live_pids: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
        })
    }

    #[allow(dead_code)]
    pub fn jobs_dir(&self) -> &Path {
        &self.jobs_dir
    }

    fn persist(&self, map: &HashMap<String, JobRecord>) {
        save_json_atomic(&self.index_file, map);
    }

    pub fn list(&self) -> Vec<JobRecord> {
        let map = self.jobs.lock().unwrap();
        let mut rows: Vec<JobRecord> = map.values().cloned().collect();
        rows.sort_by(|a, b| b.started_epoch.cmp(&a.started_epoch).then_with(|| b.id.cmp(&a.id)));
        rows
    }

    pub fn running_count(&self) -> usize {
        self.live_pids.lock().unwrap().len()
    }

    pub fn get(&self, id: &str) -> Option<JobRecord> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn read_log(&self, job: &JobRecord) -> String {
        fs::read_to_string(&job.output_file).unwrap_or_else(|_| "(no log yet)".to_string())
    }

    pub fn set_progress_message(&self, id: &str, message_id: i64) {
        let mut map = self.jobs.lock().unwrap();
        if let Some(j) = map.get_mut(id) {
            j.progress_message_id = Some(message_id);
            self.persist(&map);
        }
    }

    pub fn stop(&self, id: &str) -> bool {
        let pid = { self.live_pids.lock().unwrap().get(id).copied() };
        if let Some(p) = pid {
            let _ = Command::new("kill")
                .args(["-TERM", &p.to_string()])
                .status();
            return true;
        }
        false
    }

    pub fn append_audit(&self, action: &str, target: &str, exit_code: i32, duration_ms: u64, summary: &str) {
        let path = self.jobs_dir.join("audit.jsonl");
        let entry = json!({
            "at": utc_iso_now(),
            "action": action,
            "target": target,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "summary": summary,
        });
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{entry}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }

    pub fn start<Fp, Fd>(
        self: &Arc<Self>,
        af_bin: PathBuf,
        af_root: PathBuf,
        args: Vec<String>,
        label: String,
        on_progress: Fp,
        on_done: Fd,
    ) -> Result<JobRecord, String>
    where
        Fp: Fn(JobRecord, String) + Send + Sync + 'static,
        Fd: Fn(JobRecord, String) + Send + Sync + 'static,
    {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:08x}", ((now_secs & 0xFFFF_FF) << 8) ^ (n & 0xFF));
        let log_path = self.jobs_dir.join(format!("{id}.log"));
        let _ = fs::write(&log_path, b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600));
        }

        let mut child = Command::new(&af_bin)
            .args(&args)
            .current_dir(&af_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to launch background job: {e}"))?;

        let pid = child.id();
        let cmd_preview = format!("{} {}", af_bin.display(), args.join(" "));
        let job = JobRecord {
            id: id.clone(),
            label,
            command: cmd_preview.chars().take(480).collect(),
            status: "running".to_string(),
            pid: Some(pid),
            exit_code: None,
            started_at: utc_iso_now(),
            started_epoch: now_secs,
            ended_at: None,
            output_file: log_path.display().to_string(),
            progress_message_id: None,
        };

        {
            let mut map = self.jobs.lock().unwrap();
            map.insert(id.clone(), job.clone());
            // Keep at most 40 recent job records to keep state compact.
            if map.len() > 40 {
                let mut by_age: Vec<(u64, String)> = map
                    .iter()
                    .map(|(k, v)| (v.started_epoch, k.clone()))
                    .collect();
                by_age.sort();
                for (_, old_id) in by_age.into_iter().take(map.len().saturating_sub(40)) {
                    if old_id != id {
                        map.remove(&old_id);
                    }
                }
            }
            self.persist(&map);
        }
        self.live_pids.lock().unwrap().insert(id.clone(), pid);

        let log_writer = Arc::new(Mutex::new((log_path.clone(), 0usize)));
        if let Some(stdout) = child.stdout.take() {
            let lw = Arc::clone(&log_writer);
            thread::spawn(move || pump_log(stdout, lw));
        }
        if let Some(stderr) = child.stderr.take() {
            let lw = Arc::clone(&log_writer);
            thread::spawn(move || pump_log(stderr, lw));
        }

        let mgr = Arc::clone(self);
        let jid = id.clone();
        let lp = log_path.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3600);
            let mut next_tick = Instant::now() + Duration::from_secs(2);
            let mut last_sig = String::new();
            let mut killed_by_timeout = false;

            let (code, killed_signal) = loop {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        let c = st.code();
                        break (c, c.is_none());
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            killed_by_timeout = true;
                            let _ = child.kill();
                            let _ = child.wait();
                            break (Some(124), true);
                        }
                        if Instant::now() >= next_tick {
                            next_tick = Instant::now() + Duration::from_secs(5);
                            if let Ok(text) = fs::read_to_string(&lp) {
                                let sig = format!(
                                    "{}:{}",
                                    text.len(),
                                    tail_str(&text, 120)
                                );
                                if sig != last_sig {
                                    last_sig = sig;
                                    if let Some(cur) = mgr.get(&jid) {
                                        if cur.progress_message_id.is_some() {
                                            on_progress(cur, text);
                                        }
                                    }
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(250));
                    }
                    Err(_) => break (Some(1), false),
                }
            };

            mgr.live_pids.lock().unwrap().remove(&jid);
            // Brief settle for pipe flush
            thread::sleep(Duration::from_millis(150));

            let status_str = if killed_signal || killed_by_timeout {
                "killed"
            } else if code == Some(0) {
                "success"
            } else {
                "failed"
            };
            let final_job = {
                let mut map = mgr.jobs.lock().unwrap();
                let rec = map.get_mut(&jid).map(|j| {
                    j.status = status_str.to_string();
                    j.exit_code = code;
                    j.ended_at = Some(utc_iso_now());
                    j.clone()
                });
                mgr.persist(&map);
                rec
            };
            if let Some(fj) = final_job {
                let text = fs::read_to_string(&lp).unwrap_or_default();
                on_done(fj, text);
            }
        });

        Ok(job)
    }
}

fn pump_log<R: Read>(mut reader: R, sink: Arc<Mutex<(PathBuf, usize)>>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut guard = sink.lock().unwrap();
                if guard.1 < MAX_JOB_LOG {
                    let take = n.min(MAX_JOB_LOG - guard.1);
                    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&guard.0) {
                        let _ = f.write_all(&buf[..take]);
                    }
                    guard.1 += take;
                }
            }
            Err(_) => break,
        }
    }
}

// ── UTC time helpers ────────────────────────────────────────────────────────
pub fn utc_compact_stamp() -> String {
    let (y, m, d, hh, mm, ss) = utc_ymd_hms();
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

pub fn utc_iso_now() -> String {
    let (y, m, d, hh, mm, ss) = utc_ymd_hms();
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn utc_ymd_hms() -> (i64, u64, u64, i64, i64, i64) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let hh = tod / 3600;
    let mm = (tod % 3600) / 60;
    let ss = tod % 60;

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hh, mm, ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_pool_items() {
        let items = vec![
            json!({"id": "1", "email": "a@example.com", "priority": 1, "lastError": "[429] quota exceeded"}),
            json!({"id": "2", "email": "b@example.com", "priority": 2, "lastError": ""}),
            json!({"id": "3", "email": "c@example.com", "priority": 3, "errorCode": "403"}),
            json!({"id": "4", "email": "d@example.com", "priority": 4, "lastError": "503 overloaded"}),
        ];
        let s = summarize_pool_items(&items);
        assert_eq!(s.total, 4);
        assert_eq!(s.fresh, 1);
        assert_eq!(s.quota, 1);
        assert_eq!(s.denied, 1);
        assert_eq!(s.transient, 1);
        assert_eq!(s.health, "degraded");

        let plan = plan_reprio_from_items(&s.items);
        assert_eq!(plan[0].email, "b@example.com");
        assert_eq!(plan[0].new_priority, 1);
        assert_eq!(plan[1].email, "d@example.com");
        assert_eq!(plan[2].email, "a@example.com");
        assert_eq!(plan[3].email, "c@example.com");
    }

    #[test]
    fn test_parse_job_farm_summary() {
        let zero = r#"{"ok":true,"exit_code":0,"target":6,"elapsed_s":42.3,"stdout_tail":"round 1 summary pass=0 fail=6\n[farm] FINAL {\"total_new\":0,\"target_new\":6,\"attempts\":6,\"status\":\"done\"}"}"#;
        let s0 = parse_job_farm_summary(zero);
        assert_eq!(s0.pass, Some(0));
        assert_eq!(s0.fail, Some(6));
        assert_eq!(s0.outcome, "zero_pass");

        let hit = r#"{"ok":true,"target":3,"stdout_tail":"[farm] FINAL {\"total_new\":3,\"target_new\":3,\"status\":\"hit_target\"}"}"#;
        let s1 = parse_job_farm_summary(hit);
        assert_eq!(s1.pass, Some(3));
        assert_eq!(s1.outcome, "hit_target");
    }

    #[test]
    fn test_safe_batch_id() {
        assert!(is_safe_batch_id("pxabuse_quick_20261121T030000Z"));
        assert!(!is_safe_batch_id("../secret"));
        assert!(!is_safe_batch_id("a/b"));
        assert!(!is_safe_batch_id(""));
    }
}
