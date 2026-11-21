use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LINK_CATEGORIES: [(&str, &str, &str); 5] = [
    ("api-sellers", "api-sellers.md", "API Sellers"),
    ("otp-sms", "otp-sms.md", "OTP / SMS"),
    ("proxies", "proxies.md", "Proxies"),
    ("tools", "tools.md", "Tools"),
    ("misc", "misc.md", "Misc"),
];

pub fn link_category_file(cat: &str) -> Option<&'static str> {
    LINK_CATEGORIES
        .iter()
        .find(|(k, _, _)| *k == cat)
        .map(|(_, f, _)| *f)
}

pub fn link_category_label(cat: &str) -> &'static str {
    LINK_CATEGORIES
        .iter()
        .find(|(k, _, _)| *k == cat)
        .map(|(_, _, l)| *l)
        .unwrap_or("Misc")
}

#[derive(Debug, Clone)]
pub struct NoteEntry {
    pub path: String,
    pub size: u64,
    pub mtime_secs: i64,
}

#[derive(Debug, Clone)]
pub struct NoteFile {
    pub path: String,
    pub content: String,
    pub size: u64,
    pub mtime_secs: i64,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct LinkRecord {
    pub name: String,
    pub url: String,
    pub purpose: String,
    pub note: String,
    pub status: String,
    pub updated: String,
    pub category: String,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct VaultStats {
    pub files: usize,
    pub bytes: u64,
    pub links: usize,
}

#[derive(Clone)]
pub struct NotesVault {
    root: PathBuf,
}

impl NotesVault {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let canonical = fs::canonicalize(&root)
            .map_err(|e| format!("NOTES_ROOT not accessible ({}): {e}", root.display()))?;
        Ok(Self { root: canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Strict path jail: rejects `..`, absolute paths, NUL bytes, and escapes outside `root`.
    pub fn safe_join(&self, rel: &str) -> Result<PathBuf, String> {
        let cleaned = rel.replace('\\', "/");
        let cleaned = cleaned.trim_start_matches('/');
        if cleaned.contains('\0') {
            return Err("Illegal NUL byte in path".to_string());
        }
        let mut out = self.root.clone();
        for comp in Path::new(cleaned).components() {
            match comp {
                Component::Normal(seg) => out.push(seg),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err("Path traversal is not allowed".to_string());
                }
            }
        }
        if !out.starts_with(&self.root) {
            return Err("Path escapes notes vault".to_string());
        }
        Ok(out)
    }

    /// Validates and normalizes a user-supplied relative `.md` path.
    pub fn normalize_md_rel(&self, raw: &str) -> Result<String, String> {
        let s = raw.trim();
        if s.is_empty() {
            return Err("Path cannot be empty".to_string());
        }
        if s.starts_with('/') {
            return Err("Path must be relative (no leading '/')".to_string());
        }
        if s.contains('\\') {
            return Err("Use '/' instead of '\\'".to_string());
        }
        let mut cleaned = s.to_string();
        if !cleaned.ends_with(".md") {
            cleaned.push_str(".md");
        }
        if !cleaned
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        {
            return Err("Path may only contain a-z, 0-9, '.', '_', '-', '/'".to_string());
        }
        for seg in cleaned.split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err("Invalid path segment".to_string());
            }
        }
        Ok(cleaned)
    }

    pub fn list_files(&self) -> Vec<NoteEntry> {
        let mut out = Vec::new();
        self.walk_md(&self.root, "", &mut out);
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn walk_md(&self, dir: &Path, base_rel: &str, out: &mut Vec<NoteEntry>) {
        let Ok(rd) = fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for ent in entries {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel = if base_rel.is_empty() {
                name.clone()
            } else {
                format!("{base_rel}/{name}")
            };
            let path = ent.path();
            let Ok(meta) = ent.metadata() else {
                continue;
            };
            if meta.is_dir() {
                self.walk_md(&path, &rel, out);
            } else if meta.is_file() && name.ends_with(".md") {
                let mtime_secs = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                out.push(NoteEntry {
                    path: rel,
                    size: meta.len(),
                    mtime_secs,
                });
            }
        }
    }

    pub fn read_file(&self, rel: &str) -> Result<NoteFile, String> {
        let abs = self.safe_join(rel)?;
        if abs.extension().and_then(|e| e.to_str()) != Some("md") {
            return Err("Only .md files can be opened".to_string());
        }
        let content = fs::read_to_string(&abs).map_err(|e| format!("Cannot read file: {e}"))?;
        let meta = fs::metadata(&abs).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(content.len() as u64);
        let mtime_secs = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let clean_rel = abs
            .strip_prefix(&self.root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        Ok(NoteFile {
            path: clean_rel,
            content,
            size,
            mtime_secs,
        })
    }

    pub fn create_note(&self, rel: &str, content: Option<&str>) -> Result<NoteFile, String> {
        let path_rel = self.normalize_md_rel(rel)?;
        let abs = self.safe_join(&path_rel)?;
        if abs.exists() {
            return Err(format!("Note already exists: `{path_rel}`"));
        }
        let title = Path::new(&path_rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled");
        let body = match content {
            Some(c) if !c.trim().is_empty() => format!("{c}\n"),
            _ => format!("# {title}\n\n"),
        };
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
        }
        fs::write(&abs, &body).map_err(|e| format!("Failed to write file: {e}"))?;
        self.read_file(&path_rel)
    }

    pub fn append_note(&self, rel: &str, addition: &str) -> Result<NoteFile, String> {
        let abs = self.safe_join(rel)?;
        if !abs.exists() {
            return Err(format!("Note not found: `{rel}`"));
        }
        let existing = fs::read_to_string(&abs).map_err(|e| format!("Cannot read note: {e}"))?;
        let trimmed = existing.trim_end();
        let next = format!("{trimmed}\n\n{}\n", addition.trim());
        fs::write(&abs, next).map_err(|e| format!("Failed to update note: {e}"))?;
        self.read_file(rel)
    }

    pub fn append_link(&self, category: &str, name: &str, url: &str) -> Result<String, String> {
        let cat = category.trim().to_ascii_lowercase();
        let Some(fname) = link_category_file(&cat) else {
            return Err("Unknown link category".to_string());
        };
        let link_name = name.trim();
        let link_url = url.trim();
        if link_name.is_empty() {
            return Err("Link name is required".to_string());
        }
        let lower_url = link_url.to_ascii_lowercase();
        if !lower_url.starts_with("http://") && !lower_url.starts_with("https://") {
            return Err("URL must start with http:// or https://".to_string());
        }

        let today = today_iso_date();
        let block = format!(
            "\n## {link_name}\n- URL: {link_url}\n- Status: aktif\n- Updated: {today}\n"
        );
        let rel = format!("links/{fname}");
        let abs = self.safe_join(&rel)?;
        let existing = fs::read_to_string(&abs).unwrap_or_else(|_| format!("# {cat}\n\n"));
        let next = format!("{}\n{block}", existing.trim_end());
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create links dir: {e}"))?;
        }
        fs::write(&abs, next).map_err(|e| format!("Failed to save link: {e}"))?;
        Ok(rel)
    }

    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for ent in self.list_files() {
            let Ok(abs) = self.safe_join(&ent.path) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&abs) else {
                continue;
            };
            let path_match = ent.path.to_ascii_lowercase().contains(&q);
            let lower_content = content.to_ascii_lowercase();
            let body_pos = lower_content.find(&q);
            if path_match || body_pos.is_some() {
                let snippet = if let Some(byte_idx) = body_pos {
                    // Safely build a UTF-8 char-aligned snippet around the match
                    let mut start = byte_idx.saturating_sub(36);
                    while start > 0 && !content.is_char_boundary(start) {
                        start -= 1;
                    }
                    let mut end = (byte_idx + q.len() + 64).min(content.len());
                    while end < content.len() && !content.is_char_boundary(end) {
                        end += 1;
                    }
                    content[start..end]
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    ent.path.clone()
                };
                hits.push(SearchHit {
                    path: ent.path,
                    snippet,
                });
            }
        }
        hits
    }

    pub fn list_links(&self, category_filter: Option<&str>) -> Vec<LinkRecord> {
        let mut out = Vec::new();
        for (cat, fname, _) in LINK_CATEGORIES {
            if let Some(f) = category_filter {
                if f != cat {
                    continue;
                }
            }
            let rel = format!("links/{fname}");
            let Ok(abs) = self.safe_join(&rel) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&abs) else {
                continue;
            };
            out.extend(parse_link_blocks(&content, cat, &rel));
        }
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        out
    }

    pub fn stats(&self) -> VaultStats {
        let files = self.list_files();
        let bytes = files.iter().map(|f| f.size).sum();
        let links = self.list_links(None).len();
        VaultStats {
            files: files.len(),
            bytes,
            links,
        }
    }
}

pub fn parse_link_blocks(content: &str, category: &str, path_rel: &str) -> Vec<LinkRecord> {
    let text = content.replace("\r\n", "\n");
    let mut out = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut url = String::new();
    let mut purpose = String::new();
    let mut note = String::new();
    let mut status = "aktif".to_string();
    let mut updated = String::new();

    let flush = |out: &mut Vec<LinkRecord>,
                 name: &mut Option<String>,
                 url: &mut String,
                 purpose: &mut String,
                 note: &mut String,
                 status: &mut String,
                 updated: &mut String| {
        if let Some(n) = name.take() {
            if !url.is_empty() {
                out.push(LinkRecord {
                    name: n,
                    url: std::mem::take(url),
                    purpose: std::mem::take(purpose),
                    note: std::mem::take(note),
                    status: std::mem::replace(status, "aktif".to_string()),
                    updated: std::mem::take(updated),
                    category: category.to_string(),
                    path: path_rel.to_string(),
                });
                return;
            }
        }
        url.clear();
        purpose.clear();
        note.clear();
        *status = "aktif".to_string();
        updated.clear();
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            flush(
                &mut out,
                &mut cur_name,
                &mut url,
                &mut purpose,
                &mut note,
                &mut status,
                &mut updated,
            );
            let n = rest.trim();
            if !n.is_empty() {
                cur_name = Some(n.to_string());
            }
            continue;
        }
        if cur_name.is_some() {
            if let Some(rest) = line.trim_start().strip_prefix("- ") {
                if let Some((k, v)) = rest.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim();
                    match key.as_str() {
                        "url" => url = val.to_string(),
                        "buat" | "purpose" => purpose = val.to_string(),
                        "note" => note = val.to_string(),
                        "status" => status = val.to_ascii_lowercase(),
                        "updated" => updated = val.to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    flush(
        &mut out,
        &mut cur_name,
        &mut url,
        &mut purpose,
        &mut note,
        &mut status,
        &mut updated,
    );
    out
}

fn today_iso_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 7 * 3600;
    let days = secs.div_euclid(86400);
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
    format!("{year:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_vault_crud_search_and_jail() {
        let dir = env::temp_dir().join("px_notes_vault_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let vault = NotesVault::new(dir.clone()).unwrap();

        assert!(vault.normalize_md_rel("/etc/passwd").is_err());
        assert!(vault.normalize_md_rel("../escape").is_err());
        assert_eq!(vault.normalize_md_rel("ideas/demo").unwrap(), "ideas/demo.md");

        let created = vault.create_note("ideas/demo", Some("# Demo\nhello rust")).unwrap();
        assert_eq!(created.path, "ideas/demo.md");
        assert!(vault.create_note("ideas/demo", None).is_err());

        let updated = vault.append_note("ideas/demo.md", "second line").unwrap();
        assert!(updated.content.contains("second line"));

        let hits = vault.search("rust");
        assert_eq!(hits.len(), 1);

        let link_path = vault
            .append_link("tools", "Example Tool", "https://example.com")
            .unwrap();
        assert_eq!(link_path, "links/tools.md");
        let links = vault.list_links(Some("tools"));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].name, "Example Tool");

        let _ = fs::remove_dir_all(&dir);
    }
}
