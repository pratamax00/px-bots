use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Escapes GFM table cells so pipes and newlines never break table layout.
pub fn md_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

/// Escapes dynamic external strings so Markdown control characters render literally.
pub fn md_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\r' | '\n' => out.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '~' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.trim().to_string()
}

/// Bot API 10.3+ inline highlight (`==text==`).
pub fn hl(text: &str) -> String {
    format!("=={text}==")
}

/// Canonical GFM table with uniform left-aligned separator (`|:---|:---|`).
pub fn table_markdown(headers: &[&str], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let head: Vec<String> = headers.iter().map(|h| md_cell(h)).collect();
    let sep = format!(
        "|{}|",
        headers.iter().map(|_| ":---").collect::<Vec<_>>().join("|")
    );
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("| {} |", head.join(" | ")));
    lines.push(sep);
    for row in rows {
        let cells: Vec<String> = (0..headers.len())
            .map(|i| row.get(i).map(|s| md_cell(s)).unwrap_or_default())
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

/// Canonical 2-column `Field | Value` grid.
pub fn field_grid(pairs: &[(&str, String)]) -> String {
    let rows: Vec<Vec<String>> = pairs
        .iter()
        .map(|(k, v)| vec![(*k).to_string(), v.clone()])
        .collect();
    table_markdown(&["Field", "Value"], &rows)
}

/// Collapsible `<details><summary>...</summary>` block for secondary info.
pub fn details_block(summary: &str, content: &str) -> String {
    format!("<details>\n<summary>{summary}</summary>\n\n{content}\n\n</details>")
}

pub fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn esc_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Native in-bubble URL pill button (`<tg-button type="url">`).
pub fn tg_url_button(label: &str, url: &str) -> String {
    format!(
        "<tg-button type=\"url\" url=\"{}\">{label}</tg-button>",
        esc_attr(url)
    )
}

/// Native in-bubble callback pill button (`<tg-button type="callback_data">`).
pub fn tg_cb_button(label: &str, data: &str) -> String {
    format!(
        "<tg-button type=\"callback_data\" data=\"{}\">{label}</tg-button>",
        esc_attr(data)
    )
}

/// Native in-bubble one-tap copy pill button (`<tg-button type="copy_text">`).
pub fn tg_copy_button(label: &str, text: &str) -> String {
    format!(
        "<tg-button type=\"copy_text\" text=\"{}\">{label}</tg-button>",
        esc_attr(text)
    )
}

/// Native pill button row (`<tg-button-row>`).
pub fn tg_button_row(buttons: &[String]) -> String {
    format!(
        "<tg-button-row align=\"left\">{}</tg-button-row>",
        buttons.join("")
    )
}

/// Inline keyboard button with optional Bot API style (`primary`, `success`, `danger`, or `""`).
pub fn button(text: &str, data: &str, style: &str) -> Value {
    let mut b = json!({"text": text, "callback_data": data});
    if matches!(style, "primary" | "success" | "danger") {
        b["style"] = json!(style);
    }
    b
}

/// Arranges inline buttons into rows of up to 2 buttons each.
pub fn rows(buttons: Vec<Value>) -> Value {
    let mut out = Vec::new();
    let mut chunk = Vec::with_capacity(2);
    for b in buttons {
        chunk.push(b);
        if chunk.len() == 2 {
            out.push(Value::Array(std::mem::take(&mut chunk)));
        }
    }
    if !chunk.is_empty() {
        out.push(Value::Array(chunk));
    }
    Value::Array(out)
}

/// Safe fallback Markdown -> HTML converter when `sendRichMessage` is rejected.
pub fn markdown_to_html(md: &str) -> String {
    static RE_TG_BTN: OnceLock<Regex> = OnceLock::new();
    static RE_DETAILS: OnceLock<Regex> = OnceLock::new();
    static RE_BOLD: OnceLock<Regex> = OnceLock::new();
    static RE_ITALIC: OnceLock<Regex> = OnceLock::new();
    static RE_CODE: OnceLock<Regex> = OnceLock::new();
    static RE_HL: OnceLock<Regex> = OnceLock::new();

    let re_tg = RE_TG_BTN.get_or_init(|| Regex::new(r"</?tg-button[^>]*>").unwrap());
    let re_det =
        RE_DETAILS.get_or_init(|| Regex::new(r"</?(?:details|summary)>").unwrap());
    let re_bold = RE_BOLD.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*").unwrap());
    let re_italic = RE_ITALIC.get_or_init(|| Regex::new(r"(?m)^_(.+)_$").unwrap());
    let re_code = RE_CODE.get_or_init(|| Regex::new(r"`([^`]+)`").unwrap());
    let re_hl = RE_HL.get_or_init(|| Regex::new(r"==(.+?)==").unwrap());

    let stripped = re_tg.replace_all(md, "");
    let stripped = re_det.replace_all(&stripped, "");
    let escaped = esc_html(&stripped);
    let s = re_bold.replace_all(&escaped, "<b>$1</b>");
    let s = re_italic.replace_all(&s, "<i>$1</i>");
    let s = re_code.replace_all(&s, "<code>$1</code>");
    re_hl.replace_all(&s, "<b>[$1]</b>").into_owned()
}

pub fn truncate_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let cut: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        format!("{cut}…")
    }
}

pub fn tail_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        chars[chars.len() - max_chars..].iter().collect()
    }
}

pub fn human_bytes(bytes: u64) -> String {
    let mut n = bytes as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 {
            return format!("{n:.1} {unit}");
        }
        n /= 1024.0;
    }
    format!("{n:.1} PB")
}

pub fn format_int(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if neg {
        out.push('-');
    }
    out.chars().rev().collect()
}

pub fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn progress_bar(ratio: f64, width: usize) -> String {
    let clamped = ratio.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "▓".repeat(filled), "░".repeat(empty))
}

/// WIB (UTC+7) timestamp string: `"21 Nov 2026, 09:15 WIB"`.
pub fn stamp_wib() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 7 * 3600;
    format_epoch_wib(secs)
}

pub fn format_epoch_wib(wib_secs: i64) -> String {
    let days = wib_secs.div_euclid(86400);
    let tod = wib_secs.rem_euclid(86400);
    let hour = tod / 3600;
    let min = (tod % 3600) / 60;

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

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mon = MONTHS[(m as usize).saturating_sub(1).min(11)];
    format!("{d:02} {mon} {year}, {hour:02}:{min:02} WIB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_markdown() {
        let t = table_markdown(&["Field", "Value"], &[vec!["A|B".into(), "1".into()]]);
        assert_eq!(t, "| Field | Value |\n|:---|:---|\n| A\\|B | 1 |");
    }

    #[test]
    fn test_format_int_and_human() {
        assert_eq!(format_int(1234567), "1,234,567");
        assert_eq!(format_int(-4200), "-4,200");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }

    #[test]
    fn test_buttons_and_html_fallback() {
        assert_eq!(
            tg_copy_button("Copy", "/data/media"),
            "<tg-button type=\"copy_text\" text=\"/data/media\">Copy</tg-button>"
        );
        assert_eq!(
            markdown_to_html("**Title** ==OK== `x`"),
            "<b>Title</b> <b>[OK]</b> <code>x</code>"
        );
    }

    #[test]
    fn test_progress_bar_and_wib() {
        assert_eq!(progress_bar(0.5, 10), "▓▓▓▓▓░░░░░");
        let s = format_epoch_wib(1795224600);
        assert!(s.ends_with("WIB"));
        assert!(s.contains("2026"));
    }
}
