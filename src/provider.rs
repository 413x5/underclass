use crate::models::{Account, BackendId, Outcome};
use http::HeaderMap;
use serde_json::Value;

pub trait Backend: Send + Sync {
    fn id(&self) -> BackendId;

    fn default_cooldown_ms(&self) -> i64;

    fn rewrite_url(&self, path: &str, account: &Account) -> String;

    fn inject_headers(
        &self,
        account: &Account,
        token: &str,
        sticky: Option<&str>,
        body: &Value,
        headers: &mut HeaderMap,
    );

    fn prepare_body(&self, _body: &mut Value) {}

    /// @cc [owner:ghuntley,label:proxy] responses-translation-before-upstream
    /// When this returns true, the proxy MUST translate an inbound `/v1/responses` body to Chat
    /// Completions before header injection and `prepare_body`, and MUST translate the upstream body
    /// back to the Responses format (streamed or not) before it reaches the client. Backends that
    /// return false MUST pass request and response bodies through unchanged.
    fn responses_via_chat(&self) -> bool {
        false
    }

    /// @cc [owner:ghuntley,label:proxy] response-envelope-unwrapped
    /// When this returns a key, a non-streaming 2xx upstream body MUST be a JSON object carrying
    /// that key, and the proxy MUST unwrap `body[key]` before usage accounting or delivery to the
    /// client. An unwrappable body MUST pass through unchanged rather than be dropped.
    fn response_envelope(&self) -> Option<&'static str> {
        None
    }

    fn auto_stream_usage(&self, _path: &str, _body: &mut Value) -> bool {
        false
    }

    fn usage_is_chat(&self, path: &str) -> bool {
        path.ends_with("/chat/completions")
    }

    fn classify(&self, status: u16, _body: &str, headers: &HeaderMap, now_ms: i64) -> Outcome {
        crate::health::classify(status, headers, self.default_cooldown_ms(), now_ms)
    }
}

pub type BackendMap = std::collections::HashMap<BackendId, std::sync::Arc<dyn Backend>>;
