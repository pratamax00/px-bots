use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;
use tg_core::ui::truncate_str;

pub const RETENTION_24H: &str = "24h";
pub const RETENTION_PERMANENT: &str = "permanent";

pub fn normalize_retention(value: &str) -> &'static str {
    if value == RETENTION_PERMANENT {
        RETENTION_PERMANENT
    } else {
        RETENTION_24H
    }
}

pub fn retention_label(value: &str) -> &'static str {
    if normalize_retention(value) == RETENTION_PERMANENT {
        "♾ Permanent"
    } else {
        "⏱ 24h"
    }
}

/// FNV-1a 64-bit hash matching the Cloudflare Worker push notification callback format.
pub fn mailbox_ref(address: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for ch in address.trim().to_ascii_lowercase().chars() {
        hash ^= ch as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn is_mailbox_ref(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn normalize_local_part(input: &str) -> Result<String, String> {
    let raw = input.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return Err("Local-part cannot be empty".to_string());
    }
    let local = raw.split('@').next().unwrap_or("");
    let cleaned: String = local
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-'))
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        return Err("Invalid local-part (use a-z, 0-9, . _ + -)".to_string());
    }
    if cleaned.len() > 64 {
        return Err("Local-part too long (max 64 chars)".to_string());
    }
    Ok(cleaned)
}

pub fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Converts raw email body (HTML or plain text) into clean, readable Markdown text.
pub fn clean_email_body(raw: &str, max_chars: usize) -> String {
    static RE_DROP: OnceLock<Regex> = OnceLock::new();
    static RE_COMMENT: OnceLock<Regex> = OnceLock::new();
    static RE_BR: OnceLock<Regex> = OnceLock::new();
    static RE_BLOCK: OnceLock<Regex> = OnceLock::new();
    static RE_TAG: OnceLock<Regex> = OnceLock::new();
    static RE_SPACES: OnceLock<Regex> = OnceLock::new();
    static RE_BLANKS: OnceLock<Regex> = OnceLock::new();

    let re_drop = RE_DROP
        .get_or_init(|| Regex::new(r"(?is)<(script|style|head)\b[^>]*>.*?</(script|style|head)\s*>").unwrap());
    let re_comment = RE_COMMENT.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
    let re_br = RE_BR.get_or_init(|| Regex::new(r"(?i)<br\s*/?>").unwrap());
    let re_block = RE_BLOCK.get_or_init(|| Regex::new(r"(?i)</(p|div|tr|li|h[1-6])\s*>").unwrap());
    let re_tag = RE_TAG.get_or_init(|| Regex::new(r"</?[a-zA-Z][a-zA-Z0-9:-]*(?:\s[^<>]*?)?/?>").unwrap());
    let re_spaces = RE_SPACES.get_or_init(|| Regex::new(r"[ \t]{2,}").unwrap());
    let re_blanks = RE_BLANKS.get_or_init(|| Regex::new(r"\n{3,}").unwrap());

    let s = raw.replace("\r\n", "\n");
    let s = re_drop.replace_all(&s, " ");
    let s = re_comment.replace_all(&s, " ");
    let s = re_br.replace_all(&s, "\n");
    let s = re_block.replace_all(&s, "\n");
    let s = re_tag.replace_all(&s, " ");
    let s = decode_entities(&s);
    let s = re_spaces.replace_all(&s, " ");
    let cleaned_lines: Vec<&str> = s.lines().map(str::trim).collect();
    let joined = cleaned_lines.join("\n");
    let compact = re_blanks.replace_all(&joined, "\n\n").trim().to_string();
    if compact.chars().count() > max_chars {
        format!("{}\n\n_…truncated_", truncate_str(&compact, max_chars))
    } else {
        compact
    }
}

/// Extracts unique 4–8 digit OTP/verification codes while skipping year numbers (19xx / 20xx).
pub fn extract_codes(raw: &str) -> Vec<String> {
    static RE_CODE: OnceLock<Regex> = OnceLock::new();
    static RE_URL: OnceLock<Regex> = OnceLock::new();
    let re_url = RE_URL.get_or_init(|| Regex::new(r"https?://\S+").unwrap());
    let re_code = RE_CODE.get_or_init(|| Regex::new(r"\b(\d{4,8})\b").unwrap());

    let text = clean_email_body(raw, 12_000);
    let no_urls = re_url.replace_all(&text, " ");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cap in re_code.captures_iter(&no_urls) {
        let code = &cap[1];
        if code.len() == 4 && (code.starts_with("19") || code.starts_with("20")) {
            continue;
        }
        if seen.insert(code.to_string()) {
            out.push(code.to_string());
            if out.len() >= 4 {
                break;
            }
        }
    }
    out
}

/// Extracts unique safe http(s) action links (for one-tap URL pills inside message bubbles).
pub fn extract_links(raw: &str, max: usize) -> Vec<(String, String)> {
    static RE_HREF: OnceLock<Regex> = OnceLock::new();
    static RE_BARE: OnceLock<Regex> = OnceLock::new();

    let re_href = RE_HREF.get_or_init(|| {
        Regex::new(r#"(?is)<a\b[^>]*?href\s*=\s*"(https?://[^"\s<>]{4,512})"[^>]*>(.*?)</a>"#)
            .unwrap()
    });
    let re_bare = RE_BARE.get_or_init(|| Regex::new(r#"https?://[^\s<>"'`\])]{6,512}"#).unwrap());

    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let trim_tail = |u: &str| -> String {
        decode_entities(u)
            .trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'', ')', ']', '}'])
            .to_string()
    };

    for cap in re_href.captures_iter(raw) {
        let url = trim_tail(&cap[1]);
        let key = url.to_ascii_lowercase();
        if url.len() > 512 || !seen.insert(key) {
            continue;
        }
        let raw_lbl = clean_email_body(&cap[2], 32);
        let label = if raw_lbl.is_empty() || raw_lbl.starts_with("http") {
            short_host(&url)
        } else {
            truncate_str(&raw_lbl, 22)
        };
        out.push((label, url));
        if out.len() >= max {
            return out;
        }
    }

    for m in re_bare.find_iter(raw) {
        let url = trim_tail(m.as_str());
        let key = url.to_ascii_lowercase();
        if url.len() > 512 || !seen.insert(key) {
            continue;
        }
        let label = short_host(&url);
        out.push((label, url));
        if out.len() >= max {
            break;
        }
    }
    out
}

fn short_host(url: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = stripped.split('/').next().unwrap_or("Open Link");
    truncate_str(host, 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_mailbox_ref_parity() {
        // Must stay 16 lowercase hex chars and case-insensitive
        let a = mailbox_ref("Test@Example.com");
        let b = mailbox_ref("test@example.com");
        assert_eq!(a, b);
        assert!(is_mailbox_ref(&a));
    }

    #[test]
    fn test_extract_codes_skips_years() {
        let body = "Copyright 2026. Your verification code is 482910. Visit https://a.com/999999";
        assert_eq!(extract_codes(body), vec!["482910".to_string()]);
    }

    #[test]
    fn test_extract_links_and_html_clean() {
        let html = "<p>Hello!</p><a href=\"https://example.com/verify?t=1\">Verify Email</a>";
        let links = extract_links(html, 4);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, "Verify Email");
        assert_eq!(links[0].1, "https://example.com/verify?t=1");
        assert!(clean_email_body(html, 200).contains("Hello!"));
    }

    #[test]
    fn test_normalize_local_part() {
        assert_eq!(normalize_local_part("  Dev.User@foo.com ").unwrap(), "dev.user");
        assert!(normalize_local_part("!!!").is_err());
    }
}
