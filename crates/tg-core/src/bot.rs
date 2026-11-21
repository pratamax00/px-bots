use crate::http::build_agent;
use crate::ui::markdown_to_html;
use crate::{log_error, log_warn};
use serde_json::{json, Value};
use std::sync::Arc;
use std::thread;

/// Canonical Telegram Bot API 10.3+ client (rich-first, in-place edit, non-blocking ack).
pub struct TgClient {
    token: String,
    owner_id: i64,
    agent: ureq::Agent,
}

impl TgClient {
    pub fn new(token: String, owner_id: i64) -> Arc<Self> {
        Arc::new(Self {
            token,
            owner_id,
            agent: build_agent(10, 45),
        })
    }

    pub fn owner_id(&self) -> i64 {
        self.owner_id
    }

    pub fn is_owner(&self, chat_id: i64) -> bool {
        chat_id == self.owner_id
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// Raw Bot API POST request.
    pub fn call(&self, method: &str, payload: Value) -> Result<Value, String> {
        let url = format!("https://api.telegram.org/bot{}/{method}", self.token);
        let resp = match self
            .agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(payload)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(format!("HTTP {code}: {body}"));
            }
            Err(e) => return Err(format!("{e}")),
        };
        let text = resp.into_string().map_err(|e| format!("{e}"))?;
        if text.is_empty() {
            return Ok(json!({}));
        }
        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| format!("JSON decode failed: {e}"))?;
        if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
            let desc = parsed
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("Telegram API error");
            return Err(desc.to_string());
        }
        Ok(parsed)
    }

    /// Sends a new rich message (with automatic HTML fallback) to the owner chat.
    pub fn send(&self, markdown: &str, keyboard: Option<Value>) -> Option<i64> {
        self.send_to(self.owner_id, None, markdown, keyboard)
    }

    /// Sends a new rich message to any chat/topic (with automatic HTML fallback).
    pub fn send_to(
        &self,
        chat_id: i64,
        thread_id: Option<i64>,
        markdown: &str,
        keyboard: Option<Value>,
    ) -> Option<i64> {
        let mut payload = json!({
            "chat_id": chat_id,
            "rich_message": {"markdown": markdown},
            "disable_web_page_preview": true,
        });
        if let Some(tid) = thread_id.filter(|&t| t > 0) {
            payload["message_thread_id"] = json!(tid);
        }
        if let Some(kb) = &keyboard {
            payload["reply_markup"] = kb.clone();
        }
        match self.call("sendRichMessage", payload) {
            Ok(res) => res.pointer("/result/message_id").and_then(Value::as_i64),
            Err(e) => {
                log_warn(&format!("sendRichMessage failed ({e}), falling back to HTML"));
                let html = markdown_to_html(markdown);
                let mut fb = json!({
                    "chat_id": chat_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                });
                if let Some(tid) = thread_id.filter(|&t| t > 0) {
                    fb["message_thread_id"] = json!(tid);
                }
                if let Some(kb) = keyboard {
                    fb["reply_markup"] = kb;
                }
                match self.call("sendMessage", fb) {
                    Ok(res) => res.pointer("/result/message_id").and_then(Value::as_i64),
                    Err(e2) => {
                        log_error(&format!("sendMessage fallback failed: {e2}"));
                        None
                    }
                }
            }
        }
    }

    /// Edits an existing message in-place (`editMessageText` + `rich_message`, HTML fallback).
    pub fn edit(&self, message_id: i64, markdown: &str, keyboard: Option<Value>) -> Option<i64> {
        let mut payload = json!({
            "chat_id": self.owner_id,
            "message_id": message_id,
            "rich_message": {"markdown": markdown},
            "disable_web_page_preview": true,
        });
        if let Some(kb) = &keyboard {
            payload["reply_markup"] = kb.clone();
        }
        match self.call("editMessageText", payload) {
            Ok(_) => Some(message_id),
            Err(e) => {
                if e.to_ascii_lowercase().contains("message is not modified") {
                    return Some(message_id);
                }
                log_warn(&format!("editRich failed ({e}), falling back to HTML edit"));
                let html = markdown_to_html(markdown);
                let mut fb = json!({
                    "chat_id": self.owner_id,
                    "message_id": message_id,
                    "text": html,
                    "parse_mode": "HTML",
                    "disable_web_page_preview": true,
                });
                if let Some(kb) = &keyboard {
                    fb["reply_markup"] = kb.clone();
                }
                match self.call("editMessageText", fb) {
                    Ok(_) => Some(message_id),
                    Err(e2) => {
                        if e2.to_ascii_lowercase().contains("message is not modified") {
                            return Some(message_id);
                        }
                        log_warn(&format!("HTML edit failed ({e2}), sending new message"));
                        self.send(markdown, keyboard)
                    }
                }
            }
        }
    }

    /// Unified presentation helper: edits in-place when `message_id` is present, else sends new.
    pub fn present(
        &self,
        message_id: Option<i64>,
        markdown: &str,
        keyboard: Option<Value>,
    ) -> Option<i64> {
        match message_id {
            Some(mid) => self.edit(mid, markdown, keyboard),
            None => self.send(markdown, keyboard),
        }
    }

    /// Non-blocking callback acknowledgement to keep UI response at 1x RTT.
    pub fn ack(self: &Arc<Self>, cb_id: String, text: &str) {
        let client = Arc::clone(self);
        let msg = text.to_string();
        thread::spawn(move || {
            let mut payload = json!({"callback_query_id": cb_id});
            if !msg.is_empty() {
                payload["text"] = json!(msg);
            }
            let _ = client.call("answerCallbackQuery", payload);
        });
    }

    /// Publishes the bot slash command menu.
    pub fn set_my_commands(&self, commands: &[(&str, &str)]) {
        let list: Vec<Value> = commands
            .iter()
            .map(|(c, d)| json!({"command": c, "description": d}))
            .collect();
        if let Err(e) = self.call("setMyCommands", json!({"commands": list})) {
            log_warn(&format!("setMyCommands failed: {e}"));
        }
    }

    /// Long-polling `getUpdates` (default 25s timeout stays below NAT idle drop).
    pub fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, String> {
        let data = self.call(
            "getUpdates",
            json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }),
        )?;
        Ok(data
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}
