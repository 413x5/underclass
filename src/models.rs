use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    Codex,
    Copilot,
}

impl BackendId {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::Codex => "codex",
            BackendId::Copilot => "copilot",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "codex" => Some(BackendId::Codex),
            "copilot" => Some(BackendId::Copilot),
            _ => None,
        }
    }

    pub fn all() -> [BackendId; 2] {
        [BackendId::Codex, BackendId::Copilot]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    Healthy,
    Cooling,
    AuthError,
    Disabled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub backend: BackendId,
    pub label: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expires_at: i64,
    /// Wall-clock time of the last successful token refresh, used to schedule proactive
    /// refresh-token rotation. `0` means the token was never rotated by underclass.
    #[serde(default)]
    pub token_refreshed_at: i64,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub residency: Option<String>,
    #[serde(default)]
    pub enterprise_url: Option<String>,
    pub status: AccountStatus,
    #[serde(default)]
    pub reset_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Account {
    /// @cc [owner:ghuntley,label:monitor] account-plan-metadata-only
    /// `chatgpt_plan` MUST return only a nonempty string from the stored Codex access
    /// token's `chatgpt_plan_type` claim (namespaced auth claim preferred), or None.
    /// Other backends and malformed tokens MUST return None; no I/O is permitted.
    pub fn chatgpt_plan(&self) -> Option<String> {
        if self.backend != BackendId::Codex {
            return None;
        }
        let claims = crate::jwt::parse_jwt_claims(self.access_token.as_deref()?)?;
        claims
            .get("https://api.openai.com/auth")
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                claims
                    .get("chatgpt_plan_type")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
            })
            .map(str::to_owned)
    }

    pub fn healthy(&self) -> bool {
        self.status == AccountStatus::Healthy
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context: i64,
    #[serde(default)]
    pub input: i64,
    #[serde(default)]
    pub output: i64,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub cache_key: String,
    pub account_id: String,
    pub backend: BackendId,
    pub bound_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    QuotaExhausted { until_ms: i64 },
    AuthFailed,
    Transient,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestLogEntry {
    pub ts: i64,
    pub request_id: String,
    pub model: String,
    pub backend: Option<String>,
    pub account: Option<String>,
    pub label: Option<String>,
    pub sticky: String,
    pub status: u16,
    pub duration_ms: u64,
    /// How the response body ended; filled in when a streamed body finishes.
    pub stream_outcome: Option<String>,
    /// Sanitized upstream error code for a failed attempt or stream.
    pub error_code: Option<String>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    fn token(claims: serde_json::Value) -> String {
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn plan_is_per_account_and_tracks_current_token() {
        let mut plus = crate::flows::new_account(BackendId::Codex, "plus account".into());
        plus.access_token = Some(token(
            json!({"https://api.openai.com/auth": {"chatgpt_plan_type": "plus"}}),
        ));
        let mut pro = plus.clone();
        pro.access_token = Some(token(json!({"chatgpt_plan_type": "pro"})));
        assert_eq!(plus.chatgpt_plan().as_deref(), Some("plus"));
        assert_eq!(pro.chatgpt_plan().as_deref(), Some("pro"));
        plus.access_token = pro.access_token.clone();
        assert_eq!(plus.chatgpt_plan().as_deref(), Some("pro"));
        pro.backend = BackendId::Copilot;
        assert_eq!(pro.chatgpt_plan(), None);
    }

    #[test]
    fn plan_prefers_namespace_and_handles_missing_or_invalid_metadata() {
        let mut account = crate::flows::new_account(BackendId::Codex, "account".into());
        for claims in [
            json!({}),
            json!({"chatgpt_plan_type": 42}),
            json!({"chatgpt_plan_type": " "}),
        ] {
            account.access_token = Some(token(claims));
            assert_eq!(account.chatgpt_plan(), None);
        }
        account.access_token = Some("invalid".into());
        assert_eq!(account.chatgpt_plan(), None);
        account.access_token = None;
        assert_eq!(account.chatgpt_plan(), None);
        account.access_token = Some(token(
            json!({"chatgpt_plan_type": "free", "https://api.openai.com/auth": {"chatgpt_plan_type": "business"}}),
        ));
        assert_eq!(account.chatgpt_plan().as_deref(), Some("business"));
    }
}
