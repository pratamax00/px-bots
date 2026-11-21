use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tg_core::http::build_api_agent;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{human_bytes, tail_str, truncate_str};

const MAX_CMD_OUT: usize = 256 * 1024;
const MAX_JOB_LOG: usize = 1024 * 1024;

// ── pure-Rust SHA-256 (zero external crate) ─────────────────────────────────
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = String::with_capacity(64);
    for word in h {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("Read failed: {e}"))?;
    Ok(sha256_hex(&bytes))
}

// ── external JSON configs (loaded from OPS_CONFIG_DIR) ──────────────────────
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceDef {
    pub id: String,
    pub label: String,
    #[serde(default = "default_manager")]
    pub manager: String,
    pub unit: String,
    #[serde(default)]
    pub containers: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub alert: Option<bool>,
}

fn default_manager() -> String {
    "systemd-user".to_string()
}

impl ServiceDef {
    pub fn is_app(&self) -> bool {
        self.group.as_deref() == Some("apps")
    }
    pub fn is_soft(&self) -> bool {
        self.alert == Some(false)
    }
    pub fn docker_containers(&self) -> Vec<String> {
        if !self.containers.is_empty() {
            self.containers.clone()
        } else {
            vec![self.unit.clone()]
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupTargetDef {
    pub id: String,
    pub label: String,
    pub paths: Vec<String>,
    #[serde(default = "default_retention")]
    pub retention: usize,
}

fn default_retention() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThresholdsConfig {
    #[serde(default = "def_interval")]
    pub interval_seconds: u64,
    #[serde(default = "def_cooldown")]
    pub cooldown_seconds: u64,
    #[serde(default = "def_load")]
    pub load_per_cpu: f64,
    #[serde(default = "def_ram")]
    pub ram_percent: f64,
    #[serde(default = "def_disk")]
    pub disk_percent: f64,
    #[serde(default = "def_temp")]
    pub temperature_c: f64,
}

fn def_interval() -> u64 {
    60
}
fn def_cooldown() -> u64 {
    900
}
fn def_load() -> f64 {
    1.5
}
fn def_ram() -> f64 {
    90.0
}
fn def_disk() -> f64 {
    90.0
}
fn def_temp() -> f64 {
    85.0
}

impl Default for ThresholdsConfig {
    fn default() -> Self {
        Self {
            interval_seconds: def_interval(),
            cooldown_seconds: def_cooldown(),
            load_per_cpu: def_load(),
            ram_percent: def_ram(),
            disk_percent: def_disk(),
            temperature_c: def_temp(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpsNotifyConfig {
    #[serde(default = "def_route")]
    pub jobs: String,
    #[serde(default = "def_route")]
    pub alerts: String,
}

fn def_route() -> String {
    "dm".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct OpsForumConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub topics: HashMap<String, Option<i64>>,
    #[serde(default)]
    pub notify: OpsNotifyConfig,
}

impl OpsForumConfig {
    pub fn parsed_chat_id(&self) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        self.chat_id.trim().parse::<i64>().ok()
    }
    pub fn topic(&self, name: &str) -> Option<i64> {
        self.topics
            .get(name)
            .and_then(|v| *v)
            .filter(|&t| t > 0)
    }
}

pub fn load_services(cfg_dir: &Path) -> Vec<ServiceDef> {
    load_json(&cfg_dir.join("services.json"))
}

pub fn load_backup_targets(cfg_dir: &Path) -> Vec<BackupTargetDef> {
    load_json(&cfg_dir.join("backup-targets.json"))
}

pub fn load_thresholds(cfg_dir: &Path) -> ThresholdsConfig {
    let p = cfg_dir.join("thresholds.json");
    if p.exists() {
        load_json(&p)
    } else {
        ThresholdsConfig::default()
    }
}

pub fn load_ops_forum(cfg_dir: &Path) -> OpsForumConfig {
    load_json(&cfg_dir.join("ops-forum.json"))
}

// ── command execution with timeout ──────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub exit_code: i32,
    pub output: String,
    pub duration_ms: u64,
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

pub fn run_argv(bin: &str, args: &[String], cwd: Option<&Path>, timeout_secs: u64) -> CmdOutput {
    let t0 = Instant::now();
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CmdOutput {
                exit_code: 127,
                output: format!("Failed to spawn {bin}: {e}"),
                duration_ms: t0.elapsed().as_millis() as u64,
            };
        }
    };

    let out_p = child.stdout.take();
    let err_p = child.stderr.take();
    let h_out = thread::spawn(move || out_p.map(|r| read_capped(r, MAX_CMD_OUT)).unwrap_or_default());
    let h_err = thread::spawn(move || err_p.map(|r| read_capped(r, MAX_CMD_OUT / 2)).unwrap_or_default());

    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(3));
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code().unwrap_or(1),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break 124;
                }
                thread::sleep(Duration::from_millis(60));
            }
            Err(_) => break 1,
        }
    };

    let stdout = String::from_utf8_lossy(&h_out.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&h_err.join().unwrap_or_default()).to_string();
    let combined = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, _) => stderr,
        (_, true) => stdout,
        (false, false) => format!("{stdout}\n{stderr}"),
    };
    CmdOutput {
        exit_code: code,
        output: if timed_out {
            format!("Timed out after {timeout_secs}s\n{combined}")
        } else {
            combined
        },
        duration_ms: t0.elapsed().as_millis() as u64,
    }
}

pub fn run_shell(script: &str, cwd: Option<&Path>, timeout_secs: u64) -> CmdOutput {
    run_argv(
        "/bin/bash",
        &["-lc".to_string(), script.to_string()],
        cwd,
        timeout_secs,
    )
}

// ── native Linux host metrics ───────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct HostMetrics {
    pub hostname: String,
    pub uptime_secs: u64,
    pub cpu_pct: f64,
    pub load1: f64,
    pub cpu_cores: usize,
    pub ram_used: u64,
    pub ram_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    pub disk_pct: f64,
    pub temp_c: Option<f64>,
}

fn read_cpu_ticks() -> Option<(u64, u64)> {
    let raw = fs::read_to_string("/proc/stat").ok()?;
    let line = raw.lines().next()?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if vals.len() < 5 {
        return None;
    }
    let total: u64 = vals.iter().sum();
    let idle = vals[3] + vals[4];
    Some((total, idle))
}

pub fn read_host_metrics() -> HostMetrics {
    let hostname = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());

    let uptime_secs = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|v| v.max(0.0) as u64)
        .unwrap_or(0);

    let load1 = fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);

    let cpu_cores = fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(1)
        .max(1);

    let cpu_pct = if let Some((t1, i1)) = read_cpu_ticks() {
        thread::sleep(Duration::from_millis(60));
        if let Some((t2, i2)) = read_cpu_ticks() {
            let dt = t2.saturating_sub(t1);
            let di = i2.saturating_sub(i1);
            if dt > 0 {
                ((1.0 - (di as f64 / dt as f64)) * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else {
        0.0
    };

    let mut mem_map: HashMap<String, u64> = HashMap::new();
    if let Ok(raw) = fs::read_to_string("/proc/meminfo") {
        for line in raw.lines() {
            if let Some((k, rest)) = line.split_once(':') {
                if let Some(kb) = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()) {
                    mem_map.insert(k.trim().to_string(), kb * 1024);
                }
            }
        }
    }
    let ram_total = *mem_map.get("MemTotal").unwrap_or(&0);
    let ram_avail = *mem_map
        .get("MemAvailable")
        .or_else(|| mem_map.get("MemFree"))
        .unwrap_or(&0);
    let ram_used = ram_total.saturating_sub(ram_avail);
    let swap_total = *mem_map.get("SwapTotal").unwrap_or(&0);
    let swap_free = *mem_map.get("SwapFree").unwrap_or(&0);
    let swap_used = swap_total.saturating_sub(swap_free);

    let temp_c = read_temp_c();

    let df = run_argv(
        "df",
        &svec(&["-B1", "--output=size,used,pcent", "/"]),
        None,
        4,
    );
    let (disk_total, disk_used, disk_pct) = df
        .output
        .lines()
        .last()
        .map(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            let tot = p.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let used = p.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let pct = p
                .get(2)
                .and_then(|s| s.trim_end_matches('%').parse::<f64>().ok())
                .unwrap_or_else(|| {
                    if tot > 0 {
                        (used as f64 / tot as f64) * 100.0
                    } else {
                        0.0
                    }
                });
            (tot, used, pct)
        })
        .unwrap_or((0, 0, 0.0));

    HostMetrics {
        hostname,
        uptime_secs,
        cpu_pct,
        load1,
        cpu_cores,
        ram_used,
        ram_total,
        swap_used,
        swap_total,
        disk_used,
        disk_total,
        disk_pct,
        temp_c,
    }
}

pub fn read_temp_c() -> Option<f64> {
    let entries = fs::read_dir("/sys/class/thermal").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("thermal_zone") {
            let p = entry.path().join("temp");
            if let Ok(raw) = fs::read_to_string(p) {
                if let Ok(mv) = raw.trim().parse::<f64>() {
                    if mv > 0.0 {
                        return Some(mv / 1000.0);
                    }
                }
            }
        }
    }
    None
}

pub fn format_uptime_compact(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

// ── service status collector (systemd user + system + docker) ───────────────
#[derive(Debug, Clone)]
pub struct ServiceSnap {
    pub def: ServiceDef,
    pub state: String,
    pub sub: String,
    pub pid: String,
    pub restarts: String,
    pub mem_bytes: Option<u64>,
    pub uptime_secs: Option<u64>,
}

impl ServiceSnap {
    pub fn tone(&self) -> &'static str {
        let s = self.state.to_ascii_lowercase();
        if matches!(s.as_str(), "active" | "running") {
            "ok"
        } else if s.contains("fail") || s == "degraded" || s == "error" {
            "bad"
        } else if is_transitional(&s) {
            "warn"
        } else if matches!(s.as_str(), "inactive" | "dead" | "stopped" | "parked") {
            "off"
        } else {
            "unknown"
        }
    }

    pub fn status_badge(&self) -> String {
        match self.tone() {
            "ok" => "✅ active".to_string(),
            "bad" => format!("❌ {}", self.state),
            "warn" => format!("⏳ {}", self.state),
            "off" => {
                if self.def.is_soft() || self.def.is_app() {
                    "⏸ parked".to_string()
                } else {
                    "⏸ off".to_string()
                }
            }
            _ => format!("❔ {}", self.state),
        }
    }

    pub fn btn_style(&self) -> &'static str {
        match self.tone() {
            "ok" => "success",
            "bad" => "danger",
            "warn" => "primary",
            _ => "",
        }
    }
}

pub fn is_transitional(state: &str) -> bool {
    matches!(
        state.trim().to_ascii_lowercase().as_str(),
        "activating" | "deactivating" | "reloading" | "reload" | "refreshing" | "auto-restart"
    )
}

fn parse_kv_block(block: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in block.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn parse_mem_current(raw: Option<&String>) -> Option<u64> {
    let s = raw?.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("[not set]") || s.eq_ignore_ascii_case("infinity") {
        return None;
    }
    let v = s.parse::<u64>().ok()?;
    if v >= (1u64 << 52) {
        None
    } else {
        Some(v)
    }
}

fn collect_systemd_group(
    services: &[ServiceDef],
    manager: &str,
) -> HashMap<String, HashMap<String, String>> {
    let units: Vec<String> = services
        .iter()
        .filter(|s| s.manager == manager)
        .map(|s| s.unit.clone())
        .collect();
    let mut out = HashMap::new();
    if units.is_empty() {
        return out;
    }

    let props = [
        "-p",
        "Id",
        "-p",
        "ActiveState",
        "-p",
        "SubState",
        "-p",
        "MainPID",
        "-p",
        "MemoryCurrent",
        "-p",
        "NRestarts",
        "-p",
        "ActiveEnterTimestampMonotonic",
    ];
    let (bin, mut args) = if manager == "systemd-system" {
        ("sudo", svec(&["-n", "systemctl", "show", "--no-pager"]))
    } else {
        ("systemctl", svec(&["--user", "show", "--no-pager"]))
    };
    args.extend(props.iter().map(|s| (*s).to_string()));
    args.extend(units.iter().cloned());

    let res = run_argv(bin, &args, None, 8);
    for block in res.output.split("\n\n") {
        let kv = parse_kv_block(block);
        if let Some(id) = kv.get("Id").cloned() {
            out.insert(id, kv);
        }
    }
    out
}

fn collect_docker_group(services: &[ServiceDef]) -> HashMap<String, HashMap<String, String>> {
    let targets: Vec<&ServiceDef> = services.iter().filter(|s| s.manager == "docker").collect();
    let mut out = HashMap::new();
    if targets.is_empty() {
        return out;
    }
    let mut all_containers = Vec::new();
    for t in &targets {
        for c in t.docker_containers() {
            if !all_containers.contains(&c) {
                all_containers.push(c);
            }
        }
    }
    let mut args = svec(&[
        "inspect",
        "--format",
        "{{.Name}}|{{.State.Status}}|{{.State.Pid}}|{{.Id}}",
    ]);
    args.extend(all_containers);
    let res = run_argv("docker", &args, None, 8);

    let mut by_name: HashMap<String, (String, u64, String)> = HashMap::new();
    for line in res.output.lines() {
        let p: Vec<&str> = line.split('|').collect();
        if p.len() >= 4 {
            let name = p[0].trim_start_matches('/').to_string();
            let status = p[1].to_string();
            let pid = p[2].parse::<u64>().unwrap_or(0);
            let cid = p[3].to_string();
            by_name.insert(name, (status, pid, cid));
        }
    }

    for svc in targets {
        let list = svc.docker_containers();
        let mut running = 0usize;
        let mut pid = 0u64;
        let mut mem = 0u64;
        for cname in &list {
            if let Some((st, p, cid)) = by_name.get(cname) {
                if st == "running" {
                    running += 1;
                    if pid == 0 && *p > 0 {
                        pid = *p;
                    }
                }
                if !cid.is_empty() {
                    for cg in [
                        format!("/sys/fs/cgroup/system.slice/docker-{cid}.scope/memory.current"),
                        format!("/sys/fs/cgroup/docker/{cid}/memory.current"),
                    ] {
                        if let Ok(raw) = fs::read_to_string(cg) {
                            if let Ok(b) = raw.trim().parse::<u64>() {
                                mem += b;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let (act, sub) = if running > 0 && running == list.len() {
            ("active", "running")
        } else if running > 0 {
            ("degraded", "partial")
        } else {
            ("inactive", "dead")
        };
        let mut kv = HashMap::new();
        kv.insert("ActiveState".into(), act.into());
        kv.insert("SubState".into(), sub.into());
        kv.insert("MainPID".into(), pid.to_string());
        kv.insert("NRestarts".into(), "0".into());
        if mem > 0 {
            kv.insert("MemoryCurrent".into(), mem.to_string());
        }
        out.insert(svc.id.clone(), kv);
    }
    out
}

pub fn collect_service_snaps(services: &[ServiceDef]) -> Vec<ServiceSnap> {
    let host_up = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);

    let user_map = collect_systemd_group(services, "systemd-user");
    let sys_map = collect_systemd_group(services, "systemd-system");
    let dock_map = collect_docker_group(services);

    let mut snaps: Vec<ServiceSnap> = services
        .iter()
        .map(|def| {
            let kv = match def.manager.as_str() {
                "docker" => dock_map.get(&def.id),
                "systemd-system" => sys_map.get(&def.unit),
                _ => user_map.get(&def.unit),
            };
            let mut state = kv
                .and_then(|m| m.get("ActiveState"))
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string());
            if def.is_soft() && matches!(state.as_str(), "inactive" | "dead") {
                state = "parked".to_string();
            }
            let sub = kv
                .and_then(|m| m.get("SubState"))
                .cloned()
                .unwrap_or_default();
            let pid = kv
                .and_then(|m| m.get("MainPID"))
                .cloned()
                .unwrap_or_else(|| "0".to_string());
            let restarts = kv
                .and_then(|m| m.get("NRestarts"))
                .cloned()
                .unwrap_or_else(|| "0".to_string());
            let mem_bytes = kv.and_then(|m| parse_mem_current(m.get("MemoryCurrent")));
            let uptime_secs = kv
                .and_then(|m| m.get("ActiveEnterTimestampMonotonic"))
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|&us| us > 0.0)
                .map(|us| (host_up - us / 1_000_000.0).max(0.0) as u64);

            ServiceSnap {
                def: def.clone(),
                state,
                sub,
                pid,
                restarts,
                mem_bytes,
                uptime_secs,
            }
        })
        .collect();

    snaps.sort_by_key(|s| {
        let tone_rank = match s.tone() {
            "bad" => 0,
            "warn" => 1,
            "unknown" => 2,
            "ok" => 3,
            _ => 4,
        };
        let soft_rank = if s.def.is_soft() { 1 } else { 0 };
        (tone_rank * 10 + soft_rank, s.def.label.to_ascii_lowercase())
    });
    snaps
}

pub fn service_control(svc: &ServiceDef, op: &str, self_unit: &str) -> CmdOutput {
    if op == "restart" && svc.manager == "systemd-user" && svc.unit == self_unit {
        return run_argv(
            "systemd-run",
            &svec(&[
                "--user",
                "--collect",
                "--on-active=2s",
                "/usr/bin/systemctl",
                "--user",
                "restart",
                &svc.unit,
            ]),
            None,
            8,
        );
    }
    match svc.manager.as_str() {
        "docker" => {
            let mut args = vec![op.to_string()];
            args.extend(svc.docker_containers());
            run_argv("docker", &args, None, 45)
        }
        "systemd-system" => {
            run_argv("sudo", &svec(&["-n", "systemctl", op, &svc.unit]), None, 30)
        }
        _ => run_argv("systemctl", &svec(&["--user", op, &svc.unit]), None, 30),
    }
}

pub fn service_logs(svc: &ServiceDef, lines: usize) -> CmdOutput {
    let n = lines.clamp(10, 120).to_string();
    match svc.manager.as_str() {
        "docker" => {
            let first = svc
                .docker_containers()
                .into_iter()
                .next()
                .unwrap_or_else(|| svc.unit.clone());
            run_argv("docker", &svec(&["logs", "--tail", &n, "--timestamps", &first]), None, 12)
        }
        "systemd-system" => run_argv(
            "sudo",
            &svec(&["-n", "journalctl", "-u", &svc.unit, "-n", &n, "--no-pager"]),
            None,
            12,
        ),
        _ => run_argv(
            "journalctl",
            &svec(&["--user", "-u", &svc.unit, "-n", &n, "--no-pager"]),
            None,
            12,
        ),
    }
}

// ── backup engine (pure Rust + tar) ─────────────────────────────────────────
pub fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

pub fn is_safe_archive_name(s: &str) -> bool {
    s.ends_with(".tar.gz")
        && !s.contains('/')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub id: String,
    pub target: String,
    pub created_at: String,
    pub bytes: u64,
    pub sha256: String,
    pub paths: Vec<String>,
}

pub fn create_backup(
    backup_root: &Path,
    target: &BackupTargetDef,
    suffix: &str,
) -> Result<BackupManifest, String> {
    if !is_safe_id(&target.id) {
        return Err("Invalid backup target ID".to_string());
    }
    if target.paths.is_empty() {
        return Err("Backup target has no paths".to_string());
    }
    for p in &target.paths {
        if !Path::new(p).exists() {
            return Err(format!("Source path missing: {p}"));
        }
    }
    let dir = backup_root.join(&target.id);
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create backup dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let stamp = utc_compact_stamp();
    let name = format!("{stamp}-{suffix}.tar.gz");
    let final_path = dir.join(&name);
    let tmp_path = dir.join(format!("{name}.tmp"));

    let mut args = vec![
        "-czf".to_string(),
        tmp_path.display().to_string(),
        "--absolute-names".to_string(),
        "--exclude".to_string(),
        backup_root.display().to_string(),
        "--".to_string(),
    ];
    args.extend(target.paths.iter().cloned());
    let tar_res = run_argv("/usr/bin/tar", &args, None, 180);
    if tar_res.exit_code != 0 {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("tar failed: {}", truncate_str(&tar_res.output, 220)));
    }
    fs::rename(&tmp_path, &final_path).map_err(|e| format!("Rename failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&final_path, fs::Permissions::from_mode(0o600));
    }
    let bytes = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
    let sha = sha256_file(&final_path)?;
    let manifest = BackupManifest {
        id: name.clone(),
        target: target.id.clone(),
        created_at: utc_iso_now(),
        bytes,
        sha256: sha,
        paths: target.paths.clone(),
    };
    save_json_atomic(&dir.join(format!("{name}.json")), &manifest);

    // Retention cleanup
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut archives: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.ends_with(".tar.gz").then_some(n)
            })
            .collect();
        archives.sort();
        archives.reverse();
        for old in archives.into_iter().skip(target.retention.max(1)) {
            let _ = fs::remove_file(dir.join(&old));
            let _ = fs::remove_file(dir.join(format!("{old}.json")));
        }
    }
    Ok(manifest)
}

#[derive(Debug, Clone)]
pub struct BackupListEntry {
    pub target: String,
    pub label: String,
    pub file: Option<String>,
    pub bytes: u64,
    pub created_at: String,
}

pub fn list_latest_backups(backup_root: &Path, targets: &[BackupTargetDef]) -> Vec<BackupListEntry> {
    let mut out = Vec::new();
    for t in targets {
        let dir = backup_root.join(&t.id);
        let mut archives: Vec<String> = fs::read_dir(&dir)
            .ok()
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.ends_with(".tar.gz").then_some(n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        archives.sort();
        archives.reverse();
        if let Some(latest) = archives.into_iter().next() {
            let man_path = dir.join(format!("{latest}.json"));
            let (bytes, created) = if let Ok(raw) = fs::read_to_string(&man_path) {
                if let Ok(m) = serde_json::from_str::<BackupManifest>(&raw) {
                    (m.bytes, m.created_at)
                } else {
                    (
                        fs::metadata(dir.join(&latest)).map(|m| m.len()).unwrap_or(0),
                        latest.clone(),
                    )
                }
            } else {
                (
                    fs::metadata(dir.join(&latest)).map(|m| m.len()).unwrap_or(0),
                    latest.clone(),
                )
            };
            out.push(BackupListEntry {
                target: t.id.clone(),
                label: t.label.clone(),
                file: Some(latest),
                bytes,
                created_at: created,
            });
        } else {
            out.push(BackupListEntry {
                target: t.id.clone(),
                label: t.label.clone(),
                file: None,
                bytes: 0,
                created_at: "—".to_string(),
            });
        }
    }
    out
}

pub fn verify_backup(backup_root: &Path, target_id: &str, file_name: &str) -> Result<BackupManifest, String> {
    if !is_safe_id(target_id) || !is_safe_archive_name(file_name) {
        return Err("Invalid backup identifier".to_string());
    }
    let file = backup_root.join(target_id).join(file_name);
    let man_raw = fs::read_to_string(backup_root.join(target_id).join(format!("{file_name}.json")))
        .map_err(|e| format!("Manifest read failed: {e}"))?;
    let man: BackupManifest =
        serde_json::from_str(&man_raw).map_err(|e| format!("Manifest invalid: {e}"))?;
    let actual = sha256_file(&file)?;
    if actual != man.sha256 {
        return Err("SHA-256 checksum mismatch!".to_string());
    }
    let test = run_argv("/usr/bin/tar", &svec(&["-tzf", &file.display().to_string()]), None, 60);
    if test.exit_code != 0 {
        return Err(format!("tar integrity test failed: {}", truncate_str(&test.output, 160)));
    }
    Ok(man)
}

pub fn restore_backup(
    backup_root: &Path,
    target: &BackupTargetDef,
    file_name: &str,
) -> Result<(BackupManifest, BackupManifest), String> {
    let verified = verify_backup(backup_root, &target.id, file_name)?;
    let checkpoint = create_backup(backup_root, target, "pre-restore")?;
    let file = backup_root.join(&target.id).join(file_name);
    let res = run_argv(
        "sudo",
        &svec(&["-n", "tar", "-xzf", &file.display().to_string(), "--absolute-names", "-C", "/"]),
        None,
        180,
    );
    if res.exit_code != 0 {
        return Err(format!("Restore extract failed: {}", truncate_str(&res.output, 200)));
    }
    Ok((verified, checkpoint))
}

// ── 9Router HTTP client ─────────────────────────────────────────────────────
pub fn router_cli_token(data_dir: &Path) -> Result<String, String> {
    let mid = fs::read_to_string(data_dir.join("machine-id"))
        .map_err(|e| format!("machine-id read failed: {e}"))?;
    let sec = fs::read_to_string(data_dir.join("auth").join("cli-secret"))
        .map_err(|e| format!("cli-secret read failed: {e}"))?;
    let combined = format!("{}9r-cli-auth{}", mid.trim(), sec.trim());
    Ok(sha256_hex(combined.as_bytes())[..16].to_string())
}

pub fn router_get(
    base_url: &str,
    api_key: &str,
    data_dir: Option<&Path>,
    endpoint: &str,
    timeout_secs: u64,
) -> Result<Value, String> {
    let url = format!("{}{endpoint}", base_url.trim_end_matches('/'));
    let agent = build_api_agent(timeout_secs);
    let mut req = agent
        .get(&url)
        .set("Content-Type", "application/json")
        .set("Connection", "close");
    if let Some(dd) = data_dir {
        if let Ok(tok) = router_cli_token(dd) {
            req = req.set("x-9r-cli-token", &tok);
        }
    }
    if endpoint.starts_with("/v1/") && !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {api_key}"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!("HTTP {code}: {}", truncate_str(&body, 140)));
        }
        Err(e) => return Err(format!("{e}")),
    };
    let text = resp.into_string().map_err(|e| format!("{e}"))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| format!("JSON decode failed: {e}"))
}

pub fn extract_router_list(body: &Value) -> Vec<Value> {
    if let Some(arr) = body.as_array() {
        return arr.clone();
    }
    for key in ["data", "items", "connections", "nodes", "models", "keys", "combos"] {
        if let Some(arr) = body.get(key).and_then(Value::as_array) {
            return arr.clone();
        }
    }
    Vec::new()
}

pub fn router_refresh_catalog(
    base_url: &str,
    api_key: &str,
    data_dir: Option<&Path>,
    profiles_root: &Path,
    profiles: &[String],
) -> Result<(usize, usize, usize, usize), String> {
    let conns = extract_router_list(&router_get(base_url, api_key, data_dir, "/api/providers", 12)?);
    let nodes = extract_router_list(&router_get(base_url, api_key, data_dir, "/api/provider-nodes", 12)?);
    let models = extract_router_list(&router_get(base_url, api_key, data_dir, "/v1/models", 12)?);

    let mut grouped: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for m in &models {
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
        let prov = m
            .get("provider")
            .or_else(|| m.get("owned_by"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| id.split('/').next().unwrap_or("unknown"));
        grouped
            .entry(prov.to_string())
            .or_default()
            .push(json!({
                "id": id,
                "ownedBy": m.get("owned_by").and_then(Value::as_str).unwrap_or(prov),
            }));
    }
    let providers_json: Vec<Value> = grouped
        .iter()
        .map(|(id, items)| json!({"id": id, "models": items}))
        .collect();
    let upstream: Vec<&String> = grouped.keys().collect();
    let catalog = json!({
        "generatedAt": utc_iso_now(),
        "source": base_url,
        "router": {"id": "9router", "label": "9Router", "upstreamProviders": upstream},
        "providers": providers_json,
        "connectionCount": conns.len(),
        "nodeCount": nodes.len(),
    });
    for prof in profiles {
        let dir = profiles_root.join(prof).join("model-routing");
        let _ = fs::create_dir_all(&dir);
        save_json_atomic(&dir.join("9router-catalog.json"), &catalog);
    }
    Ok((grouped.len(), models.len(), conns.len(), nodes.len()))
}

// ── Hermes sessions manager (env-configured HERMES_PROFILE_DIR) ─────────────
#[derive(Debug, Clone)]
pub struct SessionItem {
    pub key: String,
    pub label: String,
    pub mode: String,
    pub model: String,
    pub compress_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SessionLabelsConfig {
    #[serde(default)]
    primary_group_id: String,
    #[serde(default)]
    legacy_marker: String,
    #[serde(default)]
    owner_dm_label: String,
    #[serde(default)]
    forum_labels: HashMap<String, String>,
    #[serde(default)]
    group_topic_labels: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    group_prefix: HashMap<String, String>,
}

fn load_session_labels(config_dir: &Path) -> SessionLabelsConfig {
    let p = config_dir.join("session-labels.json");
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn load_agent_sessions(profile_dir: &Path, config_dir: &Path) -> Vec<SessionItem> {
    let sessions_val: Value = load_json(&profile_dir.join("sessions").join("sessions.json"));
    let modes_val: Value = load_json(&profile_dir.join("state").join("session_modes.json"));
    let comp_val: Value = load_json(&profile_dir.join("state").join("session_compress.json"));
    let labels = load_session_labels(config_dir);

    let fp: HashSet<String> = modes_val
        .get("fullpower")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(ToString::to_string)).collect())
        .unwrap_or_default();
    let he: HashSet<String> = modes_val
        .get("hemat")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(ToString::to_string)).collect())
        .unwrap_or_default();

    let mut keys: Vec<String> = Vec::new();
    let is_valid_key = |k: &str| {
        k.starts_with("agent:main:telegram:")
            && (labels.legacy_marker.is_empty() || !k.contains(&labels.legacy_marker))
    };
    if let Some(map) = sessions_val.as_object() {
        for k in map.keys() {
            if is_valid_key(k) {
                keys.push(k.clone());
            }
        }
    }
    for k in fp.iter().chain(he.iter()) {
        if is_valid_key(k) && !keys.contains(k) {
            keys.push(k.clone());
        }
    }

    let mut items: Vec<SessionItem> = keys
        .into_iter()
        .map(|k| {
            let mode = if fp.contains(&k) {
                "fullpower".to_string()
            } else if he.contains(&k) {
                "hemat".to_string()
            } else {
                "santai".to_string()
            };
            let entry = sessions_val.get(&k);
            let model = entry
                .and_then(|e| e.pointer("/model_override/model").and_then(Value::as_str))
                .unwrap_or("default")
                .to_string();
            let compress_pct = comp_val
                .pointer(&format!("/overrides/{}", k.replace('/', "~1")))
                .and_then(Value::as_f64);
            let chat_name = entry.and_then(|e| e.pointer("/origin/chat_name").and_then(Value::as_str));
            let label = resolve_session_label(&k, chat_name, &labels);
            SessionItem {
                key: k,
                label,
                mode,
                model,
                compress_pct,
            }
        })
        .collect();
    items.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    items
}

fn resolve_session_label(key: &str, chat_name: Option<&str>, cfg: &SessionLabelsConfig) -> String {
    let trimmed = key.trim_start_matches("agent:main:telegram:");
    if trimmed.starts_with("dm:") {
        if !cfg.owner_dm_label.is_empty() {
            return cfg.owner_dm_label.clone();
        }
        return chat_name.map(|s| format!("DM · {s}")).unwrap_or_else(|| "DM Owner".to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("group:") {
        let parts: Vec<&str> = rest.split(':').collect();
        let chat_id = parts.first().copied().unwrap_or("");
        let thread_id = if parts.len() >= 3 && parts[1] == "thread" {
            Some(parts[2])
        } else if parts.len() >= 2 {
            Some(parts[1])
        } else {
            None
        };
        if let Some(thr) = thread_id {
            if !cfg.primary_group_id.is_empty() && chat_id == cfg.primary_group_id {
                if let Some(lbl) = cfg.forum_labels.get(thr) {
                    return lbl.clone();
                }
            }
            if let Some(map) = cfg.group_topic_labels.get(chat_id) {
                if let Some(lbl) = map.get(thr) {
                    return lbl.clone();
                }
            }
            if let Some(pref) = cfg.group_prefix.get(chat_id) {
                return format!("{pref} · #{thr}");
            }
            if let Some(cn) = chat_name {
                return format!("{cn} · #{thr}");
            }
            return format!("Topic #{thr}");
        }
        if let Some(pref) = cfg.group_prefix.get(chat_id) {
            return pref.clone();
        }
        if let Some(cn) = chat_name {
            return cn.to_string();
        }
    }
    truncate_str(trimmed, 24)
}

pub fn trigger_compress_now(profile_dir: &Path, keys: &[String]) {
    let path = profile_dir.join("state").join("compress-now.json");
    save_json_atomic(
        &path,
        &json!({
            "sessions": keys,
            "ts": now_epoch() * 1000,
        }),
    );
}

pub fn set_agent_session_mode(profile_dir: &Path, key: &str, mode: &str) -> Result<(), String> {
    if !matches!(mode, "santai" | "hemat" | "fullpower") {
        return Err(format!("Unsupported mode: {mode}"));
    }
    let path = profile_dir.join("state").join("session_modes.json");
    let mut doc: Value = load_json(&path);
    let mut fp: HashSet<String> = doc
        .get("fullpower")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(ToString::to_string)).collect())
        .unwrap_or_default();
    let mut he: HashSet<String> = doc
        .get("hemat")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(ToString::to_string)).collect())
        .unwrap_or_default();
    fp.remove(key);
    he.remove(key);
    if mode == "fullpower" {
        fp.insert(key.to_string());
    } else if mode == "hemat" {
        he.insert(key.to_string());
    }
    let mut fp_v: Vec<String> = fp.into_iter().collect();
    let mut he_v: Vec<String> = he.into_iter().collect();
    fp_v.sort();
    he_v.sort();
    let note = doc
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("fullpower=compress 50%; hemat=compress 10%")
        .to_string();
    doc = json!({"fullpower": fp_v, "hemat": he_v, "note": note});
    save_json_atomic(&path, &doc);
    Ok(())
}

pub fn set_agent_session_compress(profile_dir: &Path, key: &str, pct: Option<f64>) {
    let path = profile_dir.join("state").join("session_compress.json");
    let mut doc: Value = load_json(&path);
    if !doc.is_object() {
        doc = json!({"overrides": {}});
    }
    if doc.get("overrides").and_then(Value::as_object).is_none() {
        doc["overrides"] = json!({});
    }
    if let Some(ov) = doc.get_mut("overrides").and_then(Value::as_object_mut) {
        match pct {
            Some(v) => {
                ov.insert(key.to_string(), json!(v.clamp(0.10, 0.90)));
            }
            None => {
                ov.remove(key);
            }
        }
    }
    save_json_atomic(&path, &doc);
}

pub fn set_agent_session_model(
    profile_dir: &Path,
    key: &str,
    model: Option<&str>,
    router_base: &str,
) {
    let path = profile_dir.join("sessions").join("sessions.json");
    let mut doc: Value = load_json(&path);
    if !doc.is_object() {
        doc = json!({});
    }
    if let Some(root) = doc.as_object_mut() {
        match model {
            Some(m) if !m.trim().is_empty() => {
                let entry = root
                    .entry(key.to_string())
                    .or_insert_with(|| json!({"session_key": key}));
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert(
                        "model_override".into(),
                        json!({
                            "model": m.trim(),
                            "provider": "custom:9Router",
                            "base_url": format!("{}/v1", router_base.trim_end_matches('/')),
                        }),
                    );
                }
            }
            _ => {
                if let Some(obj) = root.get_mut(key).and_then(Value::as_object_mut) {
                    obj.remove("model_override");
                }
            }
        }
    }
    save_json_atomic(&path, &doc);
    let flag = profile_dir.join("state").join("model_override_reload.flag");
    if let Some(p) = flag.parent() {
        let _ = fs::create_dir_all(p);
    }
    let _ = fs::write(flag, format!("{}\n", now_epoch()));
}

// ── background job manager & audit log ──────────────────────────────────────
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
        let mut dirty = false;
        for j in stored.values_mut() {
            if j.status == "running" {
                j.status = "interrupted".to_string();
                dirty = true;
            }
        }
        if dirty {
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

    fn persist(&self, map: &HashMap<String, JobRecord>) {
        save_json_atomic(&self.index_file, map);
    }

    pub fn list(&self) -> Vec<JobRecord> {
        let mut rows: Vec<JobRecord> = self.jobs.lock().unwrap().values().cloned().collect();
        rows.sort_by(|a, b| b.started_epoch.cmp(&a.started_epoch).then_with(|| b.id.cmp(&a.id)));
        rows
    }

    pub fn get(&self, id: &str) -> Option<JobRecord> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn running_count(&self) -> usize {
        self.live_pids.lock().unwrap().len()
    }

    pub fn read_log(&self, job: &JobRecord) -> String {
        fs::read_to_string(&job.output_file).unwrap_or_else(|_| "(no log yet)".to_string())
    }

    pub fn stop(&self, id: &str) -> bool {
        let pid = self.live_pids.lock().unwrap().get(id).copied();
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
        let row = json!({
            "at": utc_iso_now(),
            "action": action,
            "target": target,
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "summary": summary,
        });
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{row}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
            }
        }
    }

    pub fn recent_audit(&self, limit: usize) -> Vec<Value> {
        let path = self.jobs_dir.join("audit.jsonl");
        let Ok(raw) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut out: Vec<Value> = raw
            .lines()
            .rev()
            .take(limit)
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect();
        out.truncate(limit);
        out
    }

    pub fn start_shell<Fd>(
        self: &Arc<Self>,
        command: String,
        label: String,
        cwd: Option<PathBuf>,
        on_done: Fd,
    ) -> Result<JobRecord, String>
    where
        Fd: Fn(JobRecord, String) + Send + Sync + 'static,
    {
        let now = now_epoch();
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:08x}", ((now & 0xFFFF_FF) << 8) ^ (n & 0xFF));
        let log_path = self.jobs_dir.join(format!("{id}.log"));
        let _ = fs::write(&log_path, b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600));
        }

        let mut cmd = Command::new("/bin/bash");
        cmd.args(["-lc", &command])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn job: {e}"))?;
        let pid = child.id();

        let job = JobRecord {
            id: id.clone(),
            label,
            command: truncate_str(&command, 420),
            status: "running".to_string(),
            pid: Some(pid),
            exit_code: None,
            started_at: utc_iso_now(),
            started_epoch: now,
            ended_at: None,
            output_file: log_path.display().to_string(),
        };
        {
            let mut map = self.jobs.lock().unwrap();
            map.insert(id.clone(), job.clone());
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

        let writer = Arc::new(Mutex::new((log_path.clone(), 0usize)));
        if let Some(out) = child.stdout.take() {
            let w = Arc::clone(&writer);
            thread::spawn(move || pump_log(out, w));
        }
        if let Some(err) = child.stderr.take() {
            let w = Arc::clone(&writer);
            thread::spawn(move || pump_log(err, w));
        }

        let mgr = Arc::clone(self);
        let jid = id.clone();
        let lp = log_path.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3600);
            let mut timed_out = false;
            let (code, signaled) = loop {
                match child.try_wait() {
                    Ok(Some(st)) => break (st.code(), st.code().is_none()),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            timed_out = true;
                            let _ = child.kill();
                            let _ = child.wait();
                            break (Some(124), true);
                        }
                        thread::sleep(Duration::from_millis(250));
                    }
                    Err(_) => break (Some(1), false),
                }
            };
            mgr.live_pids.lock().unwrap().remove(&jid);
            thread::sleep(Duration::from_millis(120));
            let st_str = if signaled || timed_out {
                "killed"
            } else if code == Some(0) {
                "success"
            } else {
                "failed"
            };
            let final_job = {
                let mut map = mgr.jobs.lock().unwrap();
                let rec = map.get_mut(&jid).map(|j| {
                    j.status = st_str.to_string();
                    j.exit_code = code;
                    j.ended_at = Some(utc_iso_now());
                    j.clone()
                });
                mgr.persist(&map);
                rec
            };
            if let Some(fj) = final_job {
                let log = fs::read_to_string(&lp).unwrap_or_default();
                on_done(fj, tail_str(&log, 1800));
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
                let mut g = sink.lock().unwrap();
                if g.1 < MAX_JOB_LOG {
                    let take = n.min(MAX_JOB_LOG - g.1);
                    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&g.0) {
                        let _ = f.write_all(&buf[..take]);
                    }
                    g.1 += take;
                }
            }
            Err(_) => break,
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────
pub fn svec(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn utc_compact_stamp() -> String {
    let (y, m, d, hh, mm, ss) = utc_ymd_hms();
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

pub fn utc_iso_now() -> String {
    let (y, m, d, hh, mm, ss) = utc_ymd_hms();
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn utc_ymd_hms() -> (i64, u64, u64, i64, i64, i64) {
    let secs = now_epoch() as i64;
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

pub fn fmt_mem_opt(bytes: Option<u64>) -> String {
    match bytes {
        Some(b) if b > 0 => human_bytes(b),
        _ => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_safe_ids() {
        assert!(is_safe_id("px-ops"));
        assert!(!is_safe_id("../etc"));
        assert!(is_safe_archive_name("20261121T030000Z-backup.tar.gz"));
        assert!(!is_safe_archive_name("../x.tar.gz"));
        assert!(!is_safe_archive_name("bad.zip"));
    }

    #[test]
    fn test_transitional_states() {
        assert!(is_transitional("deactivating"));
        assert!(is_transitional("activating"));
        assert!(!is_transitional("active"));
        assert!(!is_transitional("failed"));
    }
}
