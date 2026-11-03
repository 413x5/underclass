//! Vercel AI Gateway: a managed gateway in front of many upstream providers.
//!
//! Vercel serves models over the OpenAI Chat Completions and Responses APIs at
//! `https://ai-gateway.vercel.sh/v1`, authenticating with an AI Gateway API key. The
//! model id is `provider/model` upstream (e.g. `openai/gpt-6-luna`); underclass pools
//! it under the `vercel/<provider>/<model>` namespace so the provider stays part of the
//! routable id, mirroring the Cline backend's `cline/<provider>/<model>` shape.
//!
//! Vercel is credit-based: a single API key spends credits across every provider, so
//! there is one account-wide quota (no per-billing-surface cooling) and no per-model
//! cooldown beyond the upstream's own `429`.

use crate::models::{Account, AccountStatus, BackendId, ModelInfo, ModelStatus, now_ms};
use crate::provider::Backend;
use crate::store::Store;
use http::HeaderMap;
use reqwest::Client;
use serde_json::Value;
use std::sync::Mutex;

pub const DEFAULT_UPSTREAM: &str = "https://ai-gateway.vercel.sh";
pub const MODEL_PREFIX: &str = "vercel/";

pub fn upstream_base() -> String {
    std::env::var("UNDERCLASS_VERCEL_UPSTREAM")
        .map(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("authorization failed")]
    Failed,
    #[error("re-key target is missing or belongs to another backend")]
    BadReplaceTarget,
}

/// Strips the pool-facing `vercel/` namespace, yielding the id Vercel expects upstream.
pub fn upstream_model_id(model: &str) -> &str {
    model.strip_prefix(MODEL_PREFIX).unwrap_or(model)
}

pub fn pool_model_id(upstream: &str) -> String {
    format!("{MODEL_PREFIX}{upstream}")
}

async fn verify_key(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Option<String>, AuthError> {
    // Verified empirically: /v1/models is readable without a key, but a request that
    // carries a bad bearer is rejected with 401, so sending the key here validates it.
    // A 429 means the key is valid but rate-limited, so it is treated as success.
    let resp = client
        .get(format!("{}/v1/models", upstream_base()))
        .bearer_auth(api_key)
        .header(
            http::header::USER_AGENT,
            crate::user_agent::resolve(None, user_agent),
        )
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        return Err(AuthError::Failed);
    }
    if status == 429 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(AuthError::Status(status));
    }
    Ok(None)
}

pub async fn fetch_identity(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Option<String> {
    // Vercel keys carry no profile identity; the label falls back to "vercel".
    let _ = (client, api_key, user_agent);
    None
}

pub async fn add_account(
    client: &Client,
    store: &Store,
    api_key: &str,
    label: Option<String>,
    replace_account: Option<String>,
    user_agent: Option<&str>,
) -> Result<Account, AuthError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(AuthError::Failed);
    }
    verify_key(client, api_key, user_agent).await?;
    let label = label.filter(|l| !l.trim().is_empty());
    let ts = now_ms();
    let account = match replace_account {
        Some(existing_id) => {
            let Some(mut account) = store.get_account(&existing_id) else {
                return Err(AuthError::BadReplaceTarget);
            };
            if account.backend != BackendId::Vercel {
                return Err(AuthError::BadReplaceTarget);
            }
            account.refresh_token = Some(api_key.to_string());
            account.access_token = None;
            account.expires_at = 0;
            account.label = resolve_relabel(&account.label, label, None);
            account.status = AccountStatus::Healthy;
            account.reset_at = 0;
            account.updated_at = ts;
            account
        }
        None => {
            let mut account = crate::flows::new_account(
                BackendId::Vercel,
                label.unwrap_or_else(|| "vercel".into()),
            );
            account.refresh_token = Some(api_key.to_string());
            account
        }
    };
    store.upsert_account(&account);
    Ok(account)
}

fn resolve_relabel(existing: &str, label: Option<String>, identity: Option<String>) -> String {
    match label {
        Some(label) => label,
        None if existing.trim().is_empty() => identity.unwrap_or_else(|| "vercel".into()),
        None => existing.to_string(),
    }
}

pub fn default_catalog() -> Vec<ModelInfo> {
    // Vercel's catalog is dynamic (hundreds of models); seed empty and refresh from upstream.
    Vec::new()
}

pub async fn fetch_listing(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Vec<ModelInfo>, AuthError> {
    let resp = client
        .get(format!("{}/v1/models", upstream_base()))
        .bearer_auth(api_key)
        .header(
            http::header::USER_AGENT,
            crate::user_agent::resolve(None, user_agent),
        )
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AuthError::Status(resp.status().as_u16()));
    }
    let body: Value = resp.json().await?;
    Ok(parse_models_response(&body))
}

/// @cc [owner:ghuntley,label:pool] vercel-catalog-from-listing
/// `parse_models_response` MUST expose every listed language model under the `vercel/` prefix,
/// preserving the upstream `provider/model` id, and MUST skip non-language modalities (image,
/// video, embedding, speech, ...) because the pool only routes chat/text models. Malformed
/// entries MUST be skipped, never panic.
pub fn parse_models_response(body: &Value) -> Vec<ModelInfo> {
    let Some(items) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        // Only language models are routable through the OpenAI chat/responses surfaces.
        let ty = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("language");
        if ty != "language" {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(id);
        let context = item
            .get("context_window")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let output = item.get("max_tokens").and_then(Value::as_i64).unwrap_or(0);
        out.push(ModelInfo {
            id: pool_model_id(id),
            name: name.to_string(),
            context,
            input: 0,
            output,
            scope: String::new(),
            metadata: Default::default(),
        });
        let last = out.last_mut().unwrap();
        last.input_from_context();
        if last.context > 0 && last.input > last.context {
            last.input = last.context;
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

pub async fn refresh_catalog(
    client: &Client,
    store: &Store,
    pool: &Mutex<crate::pool::PoolCore>,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Vec<ModelInfo>, AuthError> {
    let listing = fetch_listing(client, api_key, user_agent).await?;
    if listing.is_empty() {
        return Ok(Vec::new());
    }
    let previous = store.catalog(BackendId::Vercel);
    // Carry forward stored limits/status for ids the listing still names.
    let mut models = listing.clone();
    for m in models.iter_mut() {
        if let Some(old) = previous.iter().find(|o| o.id == m.id) {
            if m.context == 0 {
                m.context = old.context;
            }
            if m.input == 0 {
                m.input = old.input;
            }
            if m.output == 0 {
                m.output = old.output;
            }
            m.input_from_context();
            if m.context > 0 {
                m.input = m.input.min(m.context);
            }
            m.metadata.fill_from(&old.metadata);
            if old.is_deprecated() {
                m.metadata.status = ModelStatus::Deprecated;
            }
        }
    }
    // Keep stored rows the listing no longer names (never-shrink).
    for old in &previous {
        if !models.iter().any(|m| m.id == old.id) {
            models.push(old.clone());
        }
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    store.set_catalog(BackendId::Vercel, &models);
    pool.lock().unwrap().set_catalog(BackendId::Vercel, &models);
    pool.lock().unwrap().rebuild_aliases(store);
    Ok(models)
}

pub struct VercelBackend {
    pub cooldown_ms: i64,
    /// The operator's configured outbound user agent, if any.
    pub user_agent: Option<String>,
}

impl Backend for VercelBackend {
    fn id(&self) -> BackendId {
        BackendId::Vercel
    }

    fn surfaces(&self) -> &'static [crate::models::Surface] {
        &[crate::models::Surface::OpenAI]
    }

    fn default_cooldown_ms(&self) -> i64 {
        self.cooldown_ms
    }

    fn rewrite_url(&self, path: &str, _model: &str, _account: &Account) -> String {
        let base = upstream_base();
        if path.ends_with("/responses") || path.ends_with("/chat/completions") {
            // Vercel serves both under /v1; keep the client's path.
            if path.ends_with("/responses") {
                format!("{base}/v1/responses")
            } else {
                format!("{base}/v1/chat/completions")
            }
        } else {
            format!("{base}{path}")
        }
    }

    fn inject_headers(
        &self,
        _account: &Account,
        token: &str,
        _sticky: Option<&str>,
        _body: &Value,
        client: &HeaderMap,
        headers: &mut HeaderMap,
    ) {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {}", token)).expect("bearer"),
        );
        headers.insert(
            http::header::USER_AGENT,
            crate::user_agent::resolve(
                crate::user_agent::client_value(client),
                self.user_agent.as_deref(),
            ),
        );
    }

    fn prepare_body(&self, body: &mut Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            let upstream = upstream_model_id(model).to_string();
            object.insert("model".into(), Value::String(upstream));
        }
    }

    fn usage_is_chat(&self, path: &str) -> bool {
        path.ends_with("/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_models_response_keeps_language_only() {
        let body = json!({"data": [
            {"id": "openai/gpt-6-luna", "name": "GPT-6 Luna", "type": "language", "context_window": 1050000, "max_tokens": 128000},
            {"id": "bfl/flux-3-image", "name": "FLUX 3", "type": "image", "context_window": 0, "max_tokens": 0},
            {"id": "", "type": "language"},
            {"no_id": true}
        ]});
        let ids: Vec<String> = parse_models_response(&body)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["vercel/openai/gpt-6-luna"]);
    }

    #[test]
    fn upstream_model_id_strips_prefix() {
        assert_eq!(
            upstream_model_id("vercel/openai/gpt-6-luna"),
            "openai/gpt-6-luna"
        );
        assert_eq!(upstream_model_id("openai/gpt-6-luna"), "openai/gpt-6-luna");
    }
}
