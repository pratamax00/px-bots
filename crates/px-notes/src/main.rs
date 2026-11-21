mod vault;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tg_core::bot::TgClient;
use tg_core::state::{load_json, save_json_atomic};
use tg_core::ui::{
    button, details_block, field_grid, format_epoch_wib, hl, human_bytes, md_text, stamp_wib,
    table_markdown, tg_button_row, tg_cb_button_styled, tg_copy_button, tg_url_button, truncate_str,
};
use tg_core::{log_error, log_info, log_warn};
use vault::{link_category_label, NotesVault, LINK_CATEGORIES};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LIST_PAGE_SIZE: usize = 8;
const FILE_PREVIEW_CHARS: usize = 2800;

// ── configuration (100% env-only) ───────────────────────────────────────────
#[derive(Clone)]
struct Config {
    tg_token: String,
    owner_id: i64,
    notes_root: PathBuf,
    state_file: PathBuf,
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
        let notes_root = PathBuf::from(
            env::var("NOTES_ROOT").unwrap_or_else(|_| "./notes".to_string()),
        );
        let state_file = PathBuf::from(
            env::var("STATE_FILE").unwrap_or_else(|_| "./state/px-notes.json".to_string()),
        );
        Ok(Self {
            tg_token,
            owner_id,
            notes_root,
            state_file,
        })
    }
}

// ── persistent state & interactive draft wizard ─────────────────────────────
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Draft {
    Search,
    NotePath,
    NoteBody { path: String },
    NoteAppend { path: String },
    LinkName { category: String },
    LinkUrl { category: String, name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct BotState {
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    draft: Option<Draft>,
}

// ── app context ─────────────────────────────────────────────────────────────
struct App {
    cfg: Config,
    tg: Arc<TgClient>,
    vault: NotesVault,
    state: Mutex<BotState>,
    refs: Mutex<HashMap<String, String>>,
    ref_seq: AtomicU64,
}

impl App {
    fn new(cfg: Config) -> Result<Arc<Self>, String> {
        let vault = NotesVault::new(cfg.notes_root.clone())?;
        let tg = TgClient::new(cfg.tg_token.clone(), cfg.owner_id);
        let initial_state: BotState = load_json(&cfg.state_file);
        Ok(Arc::new(Self {
            cfg,
            tg,
            vault,
            state: Mutex::new(initial_state),
            refs: Mutex::new(HashMap::new()),
            ref_seq: AtomicU64::new(1),
        }))
    }

    fn persist_state(&self, s: &BotState) {
        save_json_atomic(&self.cfg.state_file, s);
    }

    fn set_draft(&self, draft: Option<Draft>) {
        let mut st = self.state.lock().unwrap();
        st.draft = draft;
        self.persist_state(&st);
    }

    fn issue_ref(&self, path: &str) -> String {
        let id = self.ref_seq.fetch_add(1, Ordering::Relaxed);
        let key = format!("r{id}");
        let mut map = self.refs.lock().unwrap();
        if map.len() > 300 {
            map.clear();
        }
        map.insert(key.clone(), path.to_string());
        key
    }

    fn take_ref(&self, key: &str) -> Option<String> {
        self.refs.lock().unwrap().get(key).cloned()
    }

    // ── renderers (Bot API 10.3+) ───────────────────────────────────────────
    fn home_text(&self) -> String {
        let stats = self.vault.stats();
        let recent = {
            let mut files = self.vault.list_files();
            files.sort_by(|a, b| b.mtime_secs.cmp(&a.mtime_secs));
            files.truncate(3);
            files
        };

        let grid = field_grid(&[
            ("Notes", hl(&format!("{} files", stats.files))),
            ("Vault Size", hl(&human_bytes(stats.bytes))),
            ("Bookmarks", hl(&format!("{} links", stats.links))),
            ("Mode", hl("🟢 Telegram-Only")),
        ]);

        let pills = tg_button_row(&[
            tg_cb_button_styled("➕ New Note", "note:start", "success"),
            tg_cb_button_styled("➕ Add Link", "link:start", "success"),
            tg_cb_button_styled("🔎 Search", "search:start", "primary"),
            tg_cb_button_styled("🔄 Refresh", "menu:home", "primary"),
        ]);

        let recent_table = if recent.is_empty() {
            "_No notes yet._".to_string()
        } else {
            let r: Vec<Vec<String>> = recent
                .iter()
                .map(|f| vec![format!("`{}`", truncate_str(&f.path, 30)), human_bytes(f.size)])
                .collect();
            table_markdown(&["Recently Modified", "Size"], &r)
        };

        let cmds = table_markdown(
            &["Command", "Description"],
            &[
                vec!["`/start`".into(), "Vault dashboard".into()],
                vec!["`/notes`".into(), "Browse Markdown notes".into()],
                vec!["`/links`".into(), "Browse saved links".into()],
                vec!["`/search <q>`".into(), "Full-text & path search".into()],
                vec!["`/new [path]`".into(), "Create a new note".into()],
                vec!["`/link`".into(), "Save a new bookmark".into()],
                vec!["`/open <path>`".into(), "Open note directly".into()],
            ],
        );

        [
            format!("**📝 PX-Notes** · {}", hl(&format!("v{VERSION}"))),
            String::new(),
            grid,
            String::new(),
            format!("_{}_", stamp_wib()),
            String::new(),
            pills,
            String::new(),
            "---".to_string(),
            String::new(),
            details_block("Recent Notes & Commands", &format!("{recent_table}\n\n{cmds}")),
        ]
        .join("\n")
    }

    fn show_home(&self, msg_id: Option<i64>) {
        self.set_draft(None);
        self.tg.present(msg_id, &self.home_text(), Some(kb_main()));
    }

    fn show_notes_page(&self, page: usize, msg_id: Option<i64>) {
        self.set_draft(None);
        let files = self.vault.list_files();
        let total = files.len();
        let pages = total.div_ceil(LIST_PAGE_SIZE).max(1);
        let p = page.min(pages - 1);
        let start = p * LIST_PAGE_SIZE;
        let end = (start + LIST_PAGE_SIZE).min(total);
        let slice = &files[start..end];

        let table_rows: Vec<Vec<String>> = if slice.is_empty() {
            vec![vec!["_Empty vault_".into(), "—".into()]]
        } else {
            slice
                .iter()
                .map(|f| {
                    let r = self.issue_ref(&f.path);
                    vec![
                        tg_cb_button_styled(&truncate_str(&f.path, 34), &format!("file:open:{r}"), "link"),
                        human_bytes(f.size),
                    ]
                })
                .collect()
        };

        let pills = tg_button_row(&[
            tg_cb_button_styled("➕ New Note", "note:start", "success"),
            tg_cb_button_styled("🔎 Search", "search:start", "primary"),
        ]);

        let card = [
            format!(
                "**📁 PX-Notes · Vault** · {} · {}",
                hl(&format!("{total} notes")),
                hl(&format!("{}/{pages}", p + 1))
            ),
            String::new(),
            table_markdown(&["Path", "Size"], &table_rows),
            String::new(),
            pills,
            String::new(),
            format!("_{}_", stamp_wib()),
        ]
        .join("\n");

        let mut kb_rows: Vec<Value> = Vec::new();
        let mut nav = Vec::new();
        if p > 0 {
            nav.push(button(&format!("← Page {p}"), &format!("notes:page:{}", p - 1), "primary"));
        }
        if p + 1 < pages {
            nav.push(button(
                &format!("Page {} →", p + 2),
                &format!("notes:page:{}", p + 1),
                "primary",
            ));
        }
        if !nav.is_empty() {
            kb_rows.push(Value::Array(nav));
        }
        kb_rows.push(json!([button("🏠 Dashboard", "menu:home", "primary")]));

        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn show_links(&self, category: Option<&str>, msg_id: Option<i64>) {
        self.set_draft(None);
        let links = self.vault.list_links(category);
        let cat_title = category.map(link_category_label).unwrap_or("All");

        let rows_data: Vec<Vec<String>> = if links.is_empty() {
            vec![vec!["_No links saved_".into(), "—".into()]]
        } else {
            links
                .iter()
                .take(12)
                .map(|l| {
                    let name_cell = if is_valid_tg_url(&l.url) {
                        tg_url_button(&truncate_str(&l.name, 22), l.url.trim())
                    } else {
                        format!("**{}**", truncate_str(&md_text(&l.name), 22))
                    };
                    vec![name_cell, format!("`{}`", l.category)]
                })
                .collect()
        };

        let mut cat_pills: Vec<String> = LINK_CATEGORIES
            .iter()
            .map(|(k, _, label)| {
                let st = if category == Some(*k) { "success" } else { "primary" };
                tg_cb_button_styled(label, &format!("links:cat:{k}"), st)
            })
            .collect();
        cat_pills.push(tg_cb_button_styled(
            "All",
            "links:all",
            if category.is_none() { "success" } else { "primary" },
        ));

        let blocks = vec![
            format!(
                "**🔗 PX-Notes · Bookmarks** · {} · {}",
                hl(cat_title),
                hl(&format!("{} links", links.len()))
            ),
            String::new(),
            table_markdown(&["Link", "Category"], &rows_data),
            String::new(),
            tg_button_row(&cat_pills),
            String::new(),
            format!("_{}_", stamp_wib()),
        ];

        let kb_rows = vec![json!([
            button("➕ Add Link", "link:start", "success"),
            button("🏠 Dashboard", "menu:home", "primary"),
        ])];

        self.tg.present(
            msg_id,
            &blocks.join("\n"),
            Some(json!({"inline_keyboard": kb_rows})),
        );
    }

    fn show_file(&self, path_rel: &str, msg_id: Option<i64>) {
        self.set_draft(None);
        match self.vault.read_file(path_rel) {
            Ok(file) => {
                let r = self.issue_ref(&file.path);
                let line_count = file.content.lines().count();
                let updated = if file.mtime_secs > 0 {
                    format_epoch_wib(file.mtime_secs + 7 * 3600)
                } else {
                    "—".to_string()
                };
                let truncated = file.content.chars().count() > FILE_PREVIEW_CHARS;
                let preview = if truncated {
                    format!(
                        "{}\n\n_…truncated ({} total chars)_",
                        truncate_str(&file.content, FILE_PREVIEW_CHARS),
                        file.content.chars().count()
                    )
                } else {
                    file.content.trim().to_string()
                };

                let meta = field_grid(&[
                    ("Size", hl(&human_bytes(file.size))),
                    ("Lines", hl(&line_count.to_string())),
                    ("Updated", format!("`{updated}`")),
                ]);

                let pills = tg_button_row(&[
                    tg_copy_button("📋 Copy Path", &file.path),
                    tg_cb_button_styled("✏️ Append", &format!("file:append:{r}"), "success"),
                    tg_cb_button_styled("🔄 Reload", &format!("file:open:{r}"), "primary"),
                ]);

                let card = [
                    format!("**📄 {}**", md_text(&file.path)),
                    String::new(),
                    meta,
                    String::new(),
                    pills,
                    String::new(),
                    "---".to_string(),
                    String::new(),
                    preview,
                ]
                .join("\n");

                let kb = json!({
                    "inline_keyboard": [[
                        button("📁 Notes", "notes:page:0", "primary"),
                        button("🏠 Dashboard", "menu:home", "primary"),
                    ]]
                });
                self.tg.present(msg_id, &card, Some(kb));
            }
            Err(e) => {
                self.tg
                    .present(msg_id, &error_card("Open Failed", &e), Some(kb_back_notes()));
            }
        }
    }

    fn show_search_prompt(&self, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::Search));
        let card = [
            format!("**🔎 PX-Notes · Search** · {}", hl("Waiting for query")),
            String::new(),
            field_grid(&[
                ("Scope", "File paths & Markdown contents".into()),
                ("Example", "`infra` or `links/`".into()),
            ]),
            String::new(),
            "Send your search keyword as a message.".to_string(),
        ]
        .join("\n");
        self.tg.present(msg_id, &card, Some(kb_cancel()));
    }

    fn do_search(&self, query: &str, msg_id: Option<i64>) {
        self.set_draft(None);
        let hits = self.vault.search(query);
        let top: Vec<_> = hits.iter().take(10).collect();

        let table_rows: Vec<Vec<String>> = if top.is_empty() {
            vec![vec!["_No matches found_".into(), "—".into()]]
        } else {
            top.iter()
                .map(|h| {
                    vec![
                        format!("`{}`", truncate_str(&h.path, 24)),
                        truncate_str(&md_text(&h.snippet), 34),
                    ]
                })
                .collect()
        };

        let card = [
            format!(
                "**🔎 PX-Notes · Results** · {} · {}",
                hl(&truncate_str(query, 20)),
                hl(&format!("{} hits", hits.len()))
            ),
            String::new(),
            table_markdown(&["File", "Match"], &table_rows),
            String::new(),
            format!("_{}_", stamp_wib()),
        ]
        .join("\n");

        let mut kb_rows: Vec<Value> = top
            .iter()
            .map(|h| {
                let r = self.issue_ref(&h.path);
                json!([button(&format!("📄 {}", truncate_str(&h.path, 36)), &format!("file:open:{r}"), "primary")])
            })
            .collect();
        kb_rows.push(json!([
            button("🔎 Search Again", "search:start", "primary"),
            button("📁 Notes", "notes:page:0", "primary"),
        ]));
        kb_rows.push(json!([button("🏠 Dashboard", "menu:home", "primary")]));

        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn show_note_path_prompt(&self, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::NotePath));
        let card = [
            format!("**➕ PX-Notes · New Note** · {}", hl("Step 1/2")),
            String::new(),
            field_grid(&[
                ("Input", "Relative path (`.md` auto-added)".into()),
                ("Example", "`ideas/roadmap` or `scratch/todo.md`".into()),
                ("Safety", "Auto-creates folders · jailed inside vault".into()),
            ]),
            String::new(),
            "Send the relative path for your new note.".to_string(),
        ]
        .join("\n");
        self.tg.present(msg_id, &card, Some(kb_cancel()));
    }

    fn show_note_body_prompt(&self, path_rel: String, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::NoteBody {
            path: path_rel.clone(),
        }));
        let card = [
            format!("**➕ PX-Notes · Note Body** · {}", hl("Step 2/2")),
            String::new(),
            field_grid(&[
                ("Path", format!("`{path_rel}`")),
                ("Skip", "Creates note with `# Title` template".into()),
            ]),
            String::new(),
            "Send the initial Markdown content, or tap **Skip**.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [button("⏭ Skip · Use Template", "note:skip", "success")],
                [button("❌ Cancel", "menu:home", "danger")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_append_prompt(&self, path_rel: String, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::NoteAppend {
            path: path_rel.clone(),
        }));
        let card = [
            format!("**✏️ PX-Notes · Append** · {}", hl("Waiting for text")),
            String::new(),
            field_grid(&[("Target", format!("`{path_rel}`"))]),
            String::new(),
            "Send the Markdown text to append to the end of this note.".to_string(),
        ]
        .join("\n");
        self.tg.present(msg_id, &card, Some(kb_cancel()));
    }

    fn do_create_note(&self, path_rel: &str, body: Option<&str>, msg_id: Option<i64>) {
        match self.vault.create_note(path_rel, body) {
            Ok(created) => {
                self.set_draft(None);
                let r = self.issue_ref(&created.path);
                let card = [
                    format!("**✅ PX-Notes · Created** · {}", hl("Saved")),
                    String::new(),
                    field_grid(&[
                        ("Path", format!("`{}`", created.path)),
                        ("Size", hl(&human_bytes(created.size))),
                    ]),
                    String::new(),
                    format!("_{}_", stamp_wib()),
                ]
                .join("\n");
                let kb = json!({
                    "inline_keyboard": [
                        [button("📄 Open Note", &format!("file:open:{r}"), "primary")],
                        [
                            button("➕ New Note", "note:start", "success"),
                            button("📁 Notes", "notes:page:0", "primary"),
                        ],
                        [button("🏠 Dashboard", "menu:home", "primary")],
                    ]
                });
                self.tg.present(msg_id, &card, Some(kb));
            }
            Err(e) => {
                self.tg
                    .present(msg_id, &error_card("Create Failed", &e), Some(kb_retry_note()));
            }
        }
    }

    fn show_link_cat_prompt(&self, msg_id: Option<i64>) {
        self.set_draft(None);
        let card = [
            format!("**🔗 PX-Notes · New Bookmark** · {}", hl("Step 1/3")),
            String::new(),
            field_grid(&[
                ("Destination", "`links/<category>.md`".into()),
                ("Reminder", "Never store raw passwords in notes".into()),
            ]),
            String::new(),
            "Pick a bookmark category below.".to_string(),
        ]
        .join("\n");

        let mut kb_rows: Vec<Value> = LINK_CATEGORIES
            .iter()
            .map(|(k, _, label)| json!([button(label, &format!("link:cat:{k}"), "primary")]))
            .collect();
        kb_rows.push(json!([button("❌ Cancel", "menu:home", "danger")]));
        self.tg
            .present(msg_id, &card, Some(json!({"inline_keyboard": kb_rows})));
    }

    fn show_link_name_prompt(&self, category: String, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::LinkName {
            category: category.clone(),
        }));
        let card = [
            format!("**🔗 PX-Notes · Bookmark Name** · {}", hl("Step 2/3")),
            String::new(),
            field_grid(&[("Category", hl(link_category_label(&category)))]),
            String::new(),
            "Send the service or site name.".to_string(),
        ]
        .join("\n");
        let kb = json!({
            "inline_keyboard": [
                [button("← Categories", "link:start", "primary")],
                [button("❌ Cancel", "menu:home", "danger")],
            ]
        });
        self.tg.present(msg_id, &card, Some(kb));
    }

    fn show_link_url_prompt(&self, category: String, name: String, msg_id: Option<i64>) {
        self.set_draft(Some(Draft::LinkUrl {
            category: category.clone(),
            name: name.clone(),
        }));
        let card = [
            format!("**🔗 PX-Notes · Bookmark URL** · {}", hl("Step 3/3")),
            String::new(),
            field_grid(&[
                ("Category", hl(link_category_label(&category))),
                ("Name", format!("`{name}`")),
            ]),
            String::new(),
            "Send the full URL (`https://…`).".to_string(),
        ]
        .join("\n");
        self.tg.present(msg_id, &card, Some(kb_cancel()));
    }

    fn do_append_link(&self, category: &str, name: &str, url: &str, msg_id: Option<i64>) {
        match self.vault.append_link(category, name, url) {
            Ok(path_rel) => {
                self.set_draft(None);
                let r = self.issue_ref(&path_rel);
                let card = [
                    format!("**✅ PX-Notes · Bookmark Saved** · {}", hl("Appended")),
                    String::new(),
                    field_grid(&[
                        ("File", format!("`{path_rel}`")),
                        ("Name", format!("`{name}`")),
                        ("URL", url.to_string()),
                    ]),
                    String::new(),
                    format!("_{}_", stamp_wib()),
                ]
                .join("\n");
                let kb = json!({
                    "inline_keyboard": [
                        [button("📄 Open File", &format!("file:open:{r}"), "primary")],
                        [
                            button("➕ Another Link", "link:start", "success"),
                            button("🔗 Bookmarks", "links:all", "primary"),
                        ],
                        [button("🏠 Dashboard", "menu:home", "primary")],
                    ]
                });
                self.tg.present(msg_id, &card, Some(kb));
            }
            Err(e) => {
                self.tg
                    .present(msg_id, &error_card("Save Link Failed", &e), Some(kb_retry_link()));
            }
        }
    }
}

fn is_valid_tg_url(raw: &str) -> bool {
    let u = raw.trim();
    let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) else {
        return false;
    };
    if rest.is_empty()
        || u.chars().any(|c| c.is_whitespace() || "`\"'<>()".contains(c))
    {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !host.contains("..")
}

// ── keyboards & cards ───────────────────────────────────────────────────────
fn kb_main() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Notes", "notes:page:0", "primary"),
            button("🔗 Bookmarks", "links:all", "primary"),
        ]]
    })
}

fn kb_cancel() -> Value {
    json!({
        "inline_keyboard": [[button("❌ Cancel", "menu:home", "danger")]]
    })
}

fn kb_back_notes() -> Value {
    json!({
        "inline_keyboard": [[
            button("📁 Notes", "notes:page:0", "primary"),
            button("🏠 Dashboard", "menu:home", "primary"),
        ]]
    })
}

fn kb_retry_note() -> Value {
    json!({
        "inline_keyboard": [[
            button("🔄 Try Again", "note:start", "primary"),
            button("🏠 Dashboard", "menu:home", "primary"),
        ]]
    })
}

fn kb_retry_link() -> Value {
    json!({
        "inline_keyboard": [[
            button("🔄 Try Again", "link:start", "primary"),
            button("🏠 Dashboard", "menu:home", "primary"),
        ]]
    })
}

fn error_card(title: &str, msg: &str) -> String {
    [
        format!("**❌ PX-Notes · {title}** · {}", hl("Error")),
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
        app.show_home(msg_id);
    } else if let Some(page_s) = data.strip_prefix("notes:page:") {
        let p = page_s.parse::<usize>().unwrap_or(0);
        app.tg.ack(cb_id, "");
        app.show_notes_page(p, msg_id);
    } else if data == "links:all" {
        app.tg.ack(cb_id, "");
        app.show_links(None, msg_id);
    } else if let Some(cat) = data.strip_prefix("links:cat:") {
        app.tg.ack(cb_id, "");
        app.show_links(Some(cat), msg_id);
    } else if let Some(r) = data.strip_prefix("file:open:") {
        app.tg.ack(cb_id, "");
        match app.take_ref(r) {
            Some(path) => app.show_file(&path, msg_id),
            None => {
                app.tg.present(
                    msg_id,
                    &error_card("Expired Reference", "Re-open the Notes list."),
                    Some(kb_back_notes()),
                );
            }
        }
    } else if let Some(r) = data.strip_prefix("file:append:") {
        app.tg.ack(cb_id, "");
        match app.take_ref(r) {
            Some(path) => app.show_append_prompt(path, msg_id),
            None => {
                app.tg.present(
                    msg_id,
                    &error_card("Expired Reference", "Re-open the note first."),
                    Some(kb_back_notes()),
                );
            }
        }
    } else if data == "search:start" {
        app.tg.ack(cb_id, "");
        app.show_search_prompt(msg_id);
    } else if data == "note:start" {
        app.tg.ack(cb_id, "");
        app.show_note_path_prompt(msg_id);
    } else if data == "note:skip" {
        app.tg.ack(cb_id, "Creating…");
        let draft = { app.state.lock().unwrap().draft.clone() };
        match draft {
            Some(Draft::NoteBody { path }) => app.do_create_note(&path, None, msg_id),
            _ => {
                app.tg.present(
                    msg_id,
                    &error_card("Draft Missing", "Start creating a note again."),
                    Some(kb_retry_note()),
                );
            }
        }
    } else if data == "link:start" {
        app.tg.ack(cb_id, "");
        app.show_link_cat_prompt(msg_id);
    } else if let Some(cat) = data.strip_prefix("link:cat:") {
        app.tg.ack(cb_id, "");
        app.show_link_name_prompt(cat.to_string(), msg_id);
    } else {
        app.tg.ack(cb_id, "");
        app.show_home(msg_id);
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
        let mut parts = text.splitn(2, char::is_whitespace);
        let raw_cmd = parts.next().unwrap_or("").to_ascii_lowercase();
        let cmd = raw_cmd.split('@').next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();

        match cmd {
            "/start" | "/menu" | "/help" => app.show_home(None),
            "/notes" => app.show_notes_page(0, None),
            "/links" => app.show_links(None, None),
            "/search" => {
                if arg.is_empty() {
                    app.show_search_prompt(None);
                } else {
                    app.do_search(arg, None);
                }
            }
            "/new" => {
                if arg.is_empty() {
                    app.show_note_path_prompt(None);
                } else {
                    match app.vault.normalize_md_rel(arg) {
                        Ok(rel) => app.show_note_body_prompt(rel, None),
                        Err(e) => {
                            app.tg.send(&error_card("Invalid Path", &e), Some(kb_main()));
                        }
                    }
                }
            }
            "/link" => app.show_link_cat_prompt(None),
            "/open" => {
                if arg.is_empty() {
                    app.show_notes_page(0, None);
                } else {
                    app.show_file(arg, None);
                }
            }
            _ => app.show_home(None),
        }
        return;
    }

    // Check active wizard draft
    let draft = { app.state.lock().unwrap().draft.clone() };
    if let Some(d) = draft {
        match d {
            Draft::Search => app.do_search(text, None),
            Draft::NotePath => match app.vault.normalize_md_rel(text) {
                Ok(rel) => app.show_note_body_prompt(rel, None),
                Err(e) => {
                    app.tg.send(
                        &error_card("Invalid Path", &format!("{e}. Send another path.")),
                        Some(kb_cancel()),
                    );
                }
            },
            Draft::NoteBody { path } => app.do_create_note(&path, Some(text), None),
            Draft::NoteAppend { path } => match app.vault.append_note(&path, text) {
                Ok(_) => app.show_file(&path, None),
                Err(e) => {
                    app.set_draft(None);
                    app.tg.send(&error_card("Append Failed", &e), Some(kb_back_notes()));
                }
            },
            Draft::LinkName { category } => {
                app.show_link_url_prompt(category, text.to_string(), None);
            }
            Draft::LinkUrl { category, name } => app.do_append_link(&category, &name, text, None),
        }
        return;
    }

    // Default quick search hint
    app.show_home(None);
}

// ── main ────────────────────────────────────────────────────────────────────
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("px-notes {VERSION}");
        return;
    }
    if args.iter().any(|a| a == "--check") {
        match Config::from_env().and_then(|c| {
            let v = NotesVault::new(c.notes_root.clone())?;
            Ok((c, v.stats()))
        }) {
            Ok((cfg, st)) => {
                println!(
                    "OK: px-notes {VERSION} | root={} | files={} | links={} | state={}",
                    cfg.notes_root.display(),
                    st.files,
                    st.links,
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
    let app = match App::new(cfg) {
        Ok(a) => a,
        Err(e) => {
            log_error(&e);
            std::process::exit(1);
        }
    };

    log_info(&format!(
        "PX-Notes v{VERSION} (Rust) started | root={}",
        app.vault.root().display()
    ));

    app.tg.set_my_commands(&[
        ("start", "Open dashboard"),
        ("notes", "Browse Markdown notes"),
        ("links", "Browse saved bookmarks"),
        ("search", "Search paths and contents"),
        ("new", "Create a new note"),
        ("link", "Save a new bookmark"),
        ("open", "Open a note by path"),
        ("help", "Help & dashboard"),
    ]);
    app.tg.send(&app.home_text(), Some(kb_main()));

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
