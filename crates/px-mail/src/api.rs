use serde_json::{json, Value};
use tg_core::http::build_agent;
use tg_core::ui::truncate_str;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub message: String,
    pub status: Option<u16>,
    pub transient: bool,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct MailApi {
    base_url: String,
    admin_token: String,
    agent: ureq::Agent,
}

impl MailApi {
    pub fn new(base_url: String, admin_token: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            admin_token,
            agent: build_agent(10, 30),
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        admin: bool,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        let url = format!("{}{path}", self.base_url);
        let mut req = self
            .agent
            .request(method, &url)
            .set("User-Agent", "PX-Mail/0.3");
        if admin {
            req = req.set("x-admin-auth", &self.admin_token);
        }
        if let Some(jwt) = bearer {
            req = req.set("Authorization", &format!("Bearer {jwt}"));
        }
        let resp = match body {
            Some(payload) => req
                .set("Content-Type", "application/json")
                .send_json(payload),
            None => req.call(),
        };
        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let raw = r.into_string().unwrap_or_default();
                let parsed_msg = serde_json::from_str::<Value>(&raw)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(ToString::to_string))
                    .unwrap_or_else(|| format!("HTTP {code}: {}", truncate_str(&raw, 140)));
                return Err(ApiError {
                    message: parsed_msg,
                    status: Some(code),
                    transient: code >= 500 || code == 429 || code == 408,
                });
            }
            Err(e) => {
                return Err(ApiError {
                    message: format!("API unreachable: {e}"),
                    status: None,
                    transient: true,
                });
            }
        };
        let text = resp.into_string().map_err(|e| ApiError {
            message: format!("Read failed: {e}"),
            status: None,
            transient: true,
        })?;
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        serde_json::from_str(&text).map_err(|e| ApiError {
            message: format!("JSON decode failed: {e}"),
            status: None,
            transient: false,
        })
    }

    pub fn create_address(
        &self,
        domain: Option<&str>,
        local_part: Option<&str>,
        retention: &str,
    ) -> Result<Value, ApiError> {
        let mut body = serde_json::Map::new();
        if let Some(d) = domain.filter(|s| !s.is_empty()) {
            body.insert("domain".into(), json!(d));
        }
        if let Some(lp) = local_part.filter(|s| !s.is_empty()) {
            body.insert("localPart".into(), json!(lp));
        }
        body.insert("retention".into(), json!(retention));
        self.request("POST", "/admin/new_address", true, None, Some(Value::Object(body)))
    }

    pub fn delete_inbox(&self, address: &str, jwt: Option<&str>) -> Result<Value, ApiError> {
        let mut body = serde_json::Map::new();
        body.insert("address".into(), json!(address));
        if let Some(j) = jwt.filter(|s| !s.is_empty()) {
            body.insert("jwt".into(), json!(j));
        }
        self.request("DELETE", "/admin/inbox", true, None, Some(Value::Object(body)))
    }

    pub fn probe_mailboxes(&self, items: &[(String, String)]) -> Result<Vec<Value>, ApiError> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let payload_items: Vec<Value> = items
            .iter()
            .map(|(addr, jwt)| {
                if jwt.is_empty() {
                    json!({"address": addr.to_ascii_lowercase()})
                } else {
                    json!({"address": addr.to_ascii_lowercase(), "jwt": jwt})
                }
            })
            .collect();
        let data = self.request(
            "POST",
            "/admin/mailboxes/probe",
            true,
            None,
            Some(json!({"items": payload_items})),
        )?;
        Ok(data
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn claim_address(&self, address: &str, jwt: Option<&str>) -> Result<Value, ApiError> {
        let mut body = serde_json::Map::new();
        body.insert("address".into(), json!(address));
        if let Some(j) = jwt.filter(|s| !s.is_empty()) {
            body.insert("jwt".into(), json!(j));
        }
        self.request("POST", "/admin/claim", true, None, Some(Value::Object(body)))
    }

    pub fn list_messages(&self, jwt: &str) -> Result<Vec<Value>, ApiError> {
        let data = self.request("GET", "/messages", false, Some(jwt), None)?;
        if let Some(arr) = data.as_array() {
            return Ok(arr.clone());
        }
        Ok(data
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    pub fn get_message(&self, jwt: &str, id: &str) -> Result<Value, ApiError> {
        let encoded = pct_encode(id);
        self.request("GET", &format!("/messages/{encoded}"), false, Some(jwt), None)
    }
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
