use crate::models::{now_ms, Account, AccountStatus, BackendId, ModelInfo};
use crate::provider::Backend;
use crate::store::Store;
use http::HeaderMap;
use reqwest::Client;
use serde_json::Value;
use std::sync::Mutex;

pub const DEFAULT_API_BASE: &str = "https://api.cline.bot";
pub const MODEL_PREFIX: &str = "cline/";
pub const USER_AGENT: &str = concat!("opencode/", env!("CARGO_PKG_VERSION"), " (underclass-pool-proxy)");

pub fn api_base() -> String {
    std::env::var("UNDERCLASS_CLINE_UPSTREAM")
        .map(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
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

/// Strips the pool-facing `cline/` namespace, yielding the ID Cline expects upstream.
pub fn upstream_model_id(model: &str) -> &str {
    model.strip_prefix(MODEL_PREFIX).unwrap_or(model)
}

pub fn pool_model_id(upstream: &str) -> String {
    format!("{MODEL_PREFIX}{upstream}")
}

/// @cc [owner:ghuntley,label:pool] cline-catalog-namespaced
/// `parse_models_response` MUST expose every upstream model ID under the `cline/` prefix, MUST skip
/// entries with an empty id or a `:batch` suffix (batch-only pricing), and MUST preserve any
/// `cline-pass/` subscription models the account can use. Malformed entries MUST be skipped, never
/// panic.
pub fn parse_models_response(body: &Value) -> Vec<ModelInfo> {
    let Some(items) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || id.ends_with(":batch") {
            continue;
        }
        out.push(ModelInfo {
            id: pool_model_id(id),
            name: id.to_string(),
            context: 0,
            input: 0,
            output: 0,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// @cc [owner:ghuntley,label:pool] cline-catalog-keeps-clinepass
/// A refreshed Cline catalog MUST retain the documented ClinePass subscription models even when the
/// upstream `/models` listing omits them, so a ClinePass subscription stays routable through the
/// pool.
pub fn merge_catalog(fetched: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let mut merged = fetched;
    for subscription in default_catalog() {
        if !merged.iter().any(|model| model.id == subscription.id) {
            merged.push(subscription);
        }
    }
    merged.sort_by(|a, b| a.id.cmp(&b.id));
    merged
}

/// Model IDs documented for the ClinePass subscription, used until an account refreshes the
/// catalog and kept afterwards so the subscription remains visible.
pub fn default_catalog() -> Vec<ModelInfo> {
    let entries: &[(&str, &str)] = &[
        ("cline-pass/glm-5.3", "GLM-5.3"),
        ("cline-pass/glm-5.3-flash", "GLM-5.3 Flash"),
        ("cline-pass/kimi-k3", "Kimi K3"),
        ("cline-pass/deepseek-v4-pro", "DeepSeek V4 Pro"),
        ("cline-pass/deepseek-v4.1-flash", "DeepSeek V4.1 Flash"),
        ("cline-pass/mimo-v2.5", "MiMo-V2.5"),
        ("cline-pass/mimo-v2.5-pro", "MiMo-V2.5 Pro"),
        ("cline-pass/minimax-m3", "MiniMax M3"),
        ("cline-pass/muse-spark-1.3-contributor", "Muse Spark 1.3 Contributor"),
        ("cline-pass/qwen3.8-max", "Qwen3.8 Max"),
        ("cline-pass/qwen3.7-max", "Qwen3.7 Max"),
        ("cline-pass/qwen3.7-plus", "Qwen3.7 Plus"),
    ];
    entries
        .iter()
        .map(|(id, name)| ModelInfo {
            id: pool_model_id(id),
            name: name.to_string(),
            context: 0,
            input: 0,
            output: 0,
        })
        .collect()
}

/// @cc [owner:ghuntley,label:auth] cline-identity-label
/// An account created from an API key MUST be labeled with the profile `email` (falling back to
/// `name`) reported by `/api/v1/users/me`; an explicit operator label takes precedence and
/// `cline` is the last resort. A blank `email` MUST be treated as absent so the `name` fallback
/// still applies.
pub fn parse_user(body: &Value) -> Option<String> {
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string)
    };
    field("email").or_else(|| field("name"))
}

async fn fetch_user(client: &Client, api_key: &str) -> Result<Value, AuthError> {
    let resp = client
        .get(format!("{}/api/v1/users/me", api_base()))
        .bearer_auth(api_key)
        .header(http::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AuthError::Status(resp.status().as_u16()));
    }
    Ok(resp.json().await?)
}

pub async fn fetch_identity(client: &Client, api_key: &str) -> Option<String> {
    fetch_user(client, api_key)
        .await
        .ok()
        .and_then(|body| parse_user(&body))
}

async fn fetch_listing(client: &Client, api_key: &str) -> Result<Vec<ModelInfo>, AuthError> {
    let resp = client
        .get(format!("{}/api/v1/models", api_base()))
        .bearer_auth(api_key)
        .header(http::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(AuthError::Status(resp.status().as_u16()));
    }
    let body: Value = resp.json().await?;
    Ok(parse_models_response(&body))
}

/// @cc [owner:ghuntley,label:pool] cline-refresh-preserves-operator-limits
/// `carry_forward_limits` MUST overwrite a zero `context`/`input`/`output` on a fetched model with
/// the stored non-zero value for the same id, and MUST leave a fetched non-zero value alone. A
/// fetched model absent from the stored catalog MUST keep its own (zero) limits rather than
/// inheriting another model's.
fn carry_forward_limits(fetched: &mut [ModelInfo], previous: &[ModelInfo]) {
    for model in fetched.iter_mut() {
        let Some(old) = previous.iter().find(|old| old.id == model.id) else {
            continue;
        };
        // Upstream never reports limits, so a zero means "unknown", not "zero".
        if model.context == 0 {
            model.context = old.context;
        }
        if model.input == 0 {
            model.input = old.input;
        }
        if model.output == 0 {
            model.output = old.output;
        }
    }
}

/// @cc [owner:ghuntley,label:pool] cline-refresh-never-shrinks-to-defaults
/// `refresh_catalog` MUST leave the stored catalog and the routable model set untouched when the
/// upstream `/models` listing yields zero usable entries, and MUST return `Ok` with an empty catalog
/// so callers can distinguish a silent no-op from a fetch error. It MUST NOT persist the
/// ClinePass defaults alone, because that would drop every credit-based `cline/<provider>/*` model
/// from the pool until the next successful refresh. Limits previously stored for a still-listed
/// model MUST be carried forward by `carry_forward_limits` so an operator's catalog edit survives
/// the refresh.
pub async fn refresh_catalog(
    client: &Client,
    store: &Store,
    pool: &Mutex<crate::pool::PoolCore>,
    api_key: &str,
) -> Result<Vec<ModelInfo>, AuthError> {
    let listing = fetch_listing(client, api_key).await?;
    if listing.is_empty() {
        return Ok(Vec::new());
    }
    let mut models = merge_catalog(listing);
    carry_forward_limits(&mut models, &store.catalog(BackendId::Cline));
    store.set_catalog(BackendId::Cline, &models);
    let ids: Vec<String> = models.iter().map(|model| model.id.clone()).collect();
    pool.lock().unwrap().set_catalog(BackendId::Cline, ids);
    Ok(models)
}

/// @cc [owner:ghuntley,label:auth] cline-key-onboarding
/// Adding a Cline account MUST verify the operator-supplied API key against `/api/v1/users/me`
/// and MUST NOT persist an account when that verification fails, MUST store the key only in the
/// account's `refresh_token` column, and MUST reject an empty key. A `401`/`403` verification
/// response MUST surface as `AuthError::Failed` (the key was rejected); any other upstream status
/// MUST keep its own error so a transient outage is not reported as a bad key. Replacing an existing
/// account MUST keep its id, clear any cooldown, and
/// return the account to `Healthy`; a replace id that is unknown or belongs to another backend
/// MUST be rejected with `AuthError::BadReplaceTarget` - distinct from a rejected key, so a stale
/// dialog never tells the operator their working key was refused - and a Cline key can never
/// rebind a non-Cline account. A blank or whitespace-only
/// label MUST be treated as absent. An absent label MUST keep an existing non-blank label on a
/// replace, and otherwise fall back to the profile identity and then to `cline`.
pub async fn add_account(
    client: &Client,
    store: &Store,
    api_key: &str,
    label: Option<String>,
    replace_account: Option<String>,
) -> Result<Account, AuthError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(AuthError::Failed);
    }
    let identity = fetch_user(client, api_key).await.map_err(|error| match error {
        // A key the upstream rejected must never reach the store. A transient 429/5xx says nothing
        // about the key, so it must not be reported as a rejection that prompts a key rotation.
        AuthError::Status(401 | 403) => AuthError::Failed,
        other => other,
    })?;
    let identity = parse_user(&identity);
    // An explicit `""` from the admin API is not a label; treat it like an absent one so a blank
    // identity never reaches the store, the account row, or a log line.
    let label = label.filter(|label| !label.trim().is_empty());
    let ts = now_ms();
    let account = match replace_account {
        Some(existing_id) => {
            let Some(mut account) = store.get_account(&existing_id) else {
                return Err(AuthError::BadReplaceTarget);
            };
            if account.backend != BackendId::Cline {
                return Err(AuthError::BadReplaceTarget);
            }
            account.refresh_token = Some(api_key.to_string());
            account.access_token = None;
            account.expires_at = 0;
            // Re-keying must not silently rename an account the operator labeled: the Re-key dialog
            // submits no label, so an absent label keeps the stored one instead of adopting the
            // profile identity. Only the identity refresh (which targets generic labels) may override.
            account.label = resolve_relabel(&account.label, label, identity);
            account.status = AccountStatus::Healthy;
            account.reset_at = 0;
            account.updated_at = ts;
            account
        }
        None => {
            let mut account = crate::flows::new_account(
                BackendId::Cline,
                label.or(identity).unwrap_or_else(|| "cline".into()),
            );
            account.refresh_token = Some(api_key.to_string());
            account
        }
    };
    store.upsert_account(&account);
    Ok(account)
}

/// @cc [owner:ghuntley,label:auth] cline-rekey-keeps-operator-label
/// `resolve_relabel` MUST keep the existing label when no explicit label is supplied, so re-keying an
/// account the operator named does not rename it to the profile identity. An explicit label wins, and
/// an existing blank label MAY be replaced by the identity and then by `cline`.
fn resolve_relabel(existing: &str, label: Option<String>, identity: Option<String>) -> String {
    match label {
        Some(label) => label,
        None if existing.trim().is_empty() => identity.unwrap_or_else(|| "cline".into()),
        None => existing.to_string(),
    }
}

pub struct ClineBackend {
    pub cooldown_ms: i64,
}

impl Backend for ClineBackend {
    fn id(&self) -> BackendId {
        BackendId::Cline
    }

    fn default_cooldown_ms(&self) -> i64 {
        self.cooldown_ms
    }

    /// @cc [owner:ghuntley,label:proxy] cline-responses-via-chat
    /// Cline exposes only Chat Completions, so `/responses` requests routed to this backend MUST be
    /// converted to `POST <base>/api/v1/chat/completions` and their bodies translated back to the
    /// Responses API before reaching the client.
    fn responses_via_chat(&self) -> bool {
        true
    }

    fn rewrite_url(&self, path: &str, _account: &Account) -> String {
        let base = api_base();
        if path.ends_with("/responses") || path.ends_with("/chat/completions") {
            format!("{base}/api/v1/chat/completions")
        } else {
            format!("{base}{path}")
        }
    }

    /// @cc [owner:ghuntley,label:pool] cline-402-quota
    /// Cline reports exhausted credits as `402 Payment Required`, which MUST cool the account for
    /// the configured cooldown like a `429`. `403` MUST NOT mark the account auth-failed: Cline uses
    /// it for per-resource access (for example a model outside the account's plan), and a valid key
    /// must stay in rotation for the models it can serve.
    fn classify(&self, status: u16, _body: &str, headers: &HeaderMap, now_ms: i64) -> crate::models::Outcome {
        if status == 402 {
            return crate::models::Outcome::QuotaExhausted {
                until_ms: now_ms.saturating_add(self.cooldown_ms),
            };
        }
        if status == 403 {
            return crate::models::Outcome::Ok;
        }
        crate::health::classify(status, headers, self.cooldown_ms, now_ms)
    }

    /// Cline wraps non-streaming completions as `{"data": <completion>, "success": true}`.
    fn response_envelope(&self) -> Option<&'static str> {
        Some("data")
    }

    fn prepare_body(&self, body: &mut Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            let upstream = upstream_model_id(model).to_string();
            object.insert("model".into(), Value::String(upstream));
        }
        // Cline's routers reject or ignore OpenAI-only request controls.
        object.remove("prompt_cache_key");
        object.remove("promptCacheKey");
        object.remove("store");
    }

    /// @cc [owner:ghuntley,label:accounting] cline-stream-usage
    /// Streamed Cline requests MUST ask for the final usage chunk unless the client already chose
    /// `stream_options.include_usage`; the proxy's unsupported-option fallback protects accounts
    /// whose upstream rejects the parameter.
    fn auto_stream_usage(&self, path: &str, body: &mut Value) -> bool {
        if !(path.ends_with("/chat/completions") || path.ends_with("/responses")) {
            return false;
        }
        if body.get("stream").and_then(Value::as_bool) != Some(true) {
            return false;
        }
        let Some(object) = body.as_object_mut() else {
            return false;
        };
        if let Some(options) = object.get_mut("stream_options") {
            let Some(options) = options.as_object_mut() else {
                return false;
            };
            if options.contains_key("include_usage") {
                return false;
            }
            options.insert("include_usage".into(), Value::Bool(true));
        } else {
            object.insert(
                "stream_options".into(),
                serde_json::json!({"include_usage": true}),
            );
        }
        true
    }

    /// Cline is Chat-Completions-only, including for translated Responses requests.
    fn usage_is_chat(&self, _path: &str) -> bool {
        true
    }

    fn inject_headers(
        &self,
        _account: &Account,
        token: &str,
        _sticky: Option<&str>,
        _body: &Value,
        headers: &mut HeaderMap,
    ) {
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer"),
        );
        if let Ok(value) = http::HeaderValue::from_str(USER_AGENT) {
            headers.insert(http::header::USER_AGENT, value);
        }
        headers.insert("X-Title", http::HeaderValue::from_static("underclass"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_prefixes_ids_and_skips_batch_entries() {
        let body = json!({"data": [
            {"id": "anthropic/claude-sonnet-4-6"},
            {"id": "openai/gpt-5.4:batch"},
            {"id": ""},
            {"no_id": true},
            {"id": "cline-pass/deepseek-v4.1-flash"}
        ]});
        let models = parse_models_response(&body);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "cline/anthropic/claude-sonnet-4-6",
                "cline/cline-pass/deepseek-v4.1-flash"
            ]
        );
    }

    #[test]
    fn operator_limits_survive_a_refresh() {
        // Cline's listing carries no limits, so a boot refresh would otherwise wipe the only
        // remedy the fork ADR documents for zero-limit Cline models: editing the catalog.
        let stored = vec![ModelInfo {
            id: "cline/openai/gpt-5.4".into(),
            name: "GPT-5.4".into(),
            context: 128_000,
            input: 120_000,
            output: 8_000,
        }];
        let mut fetched = parse_models_response(&json!({"data": [{"id": "openai/gpt-5.4"}]}));
        assert_eq!(fetched[0].context, 0, "the listing reports no limits");
        carry_forward_limits(&mut fetched, &stored);
        assert_eq!(fetched[0].context, 128_000);
        assert_eq!(fetched[0].input, 120_000);
        assert_eq!(fetched[0].output, 8_000);
    }

    #[test]
    fn upstream_limits_win_and_new_models_keep_their_own() {
        let stored = vec![
            ModelInfo {
                id: "cline/openai/gpt-5.4".into(),
                name: String::new(),
                context: 1,
                input: 1,
                output: 1,
            },
            ModelInfo {
                id: "cline/gone/model".into(),
                name: String::new(),
                context: 999,
                input: 999,
                output: 999,
            },
        ];
        let mut fetched = vec![
            ModelInfo {
                id: "cline/openai/gpt-5.4".into(),
                name: String::new(),
                context: 400_000,
                input: 0,
                output: 0,
            },
            ModelInfo {
                id: "cline/brand/new".into(),
                name: String::new(),
                context: 0,
                input: 0,
                output: 0,
            },
        ];
        carry_forward_limits(&mut fetched, &stored);
        assert_eq!(fetched[0].context, 400_000, "a real upstream limit wins");
        assert_eq!(fetched[0].input, 1, "the zero fields still inherit");
        assert_eq!(fetched[1].context, 0, "a new model inherits nothing");
    }

    #[test]
    fn merge_keeps_subscription_models_absent_upstream() {
        let fetched = parse_models_response(&json!({"data": [{"id": "openai/gpt-5.4"}]}));
        let merged = merge_catalog(fetched);
        assert!(merged.iter().any(|model| model.id == "cline/openai/gpt-5.4"));
        assert!(merged.iter().any(|model| model.id == "cline/cline-pass/kimi-k3"));
    }

    #[test]
    fn unusable_listing_yields_no_models_to_persist() {
        // A 200 with a missing or null `data` array, or entries that all fail validation, is a
        // failed listing, not an empty catalog. `refresh_catalog` keys off this to avoid
        // replacing a good catalog with the defaults alone.
        for body in [
            json!({}),
            json!({"data": null}),
            json!({"data": []}),
            json!({"data": [{"no_id": true}, {"id": ""}, {"id": "x:batch"}]}),
        ] {
            assert!(
                parse_models_response(&body).is_empty(),
                "expected an empty listing for {body}"
            );
        }
    }

    #[test]
    fn model_ids_round_trip_through_the_namespace() {
        assert_eq!(upstream_model_id("cline/anthropic/claude-sonnet-4-6"), "anthropic/claude-sonnet-4-6");
        assert_eq!(upstream_model_id("gpt-5.5"), "gpt-5.5");
        assert_eq!(
            pool_model_id("cline-pass/kimi-k3"),
            "cline/cline-pass/kimi-k3"
        );
    }

    #[test]
    fn rewrites_completion_paths_and_strips_namespace() {
        let backend = ClineBackend { cooldown_ms: 1000 };
        let account = crate::flows::new_account(BackendId::Cline, "x".into());
        let url = backend.rewrite_url("/v1/responses", &account);
        assert!(url.ends_with("/api/v1/chat/completions"));
        assert!(backend.rewrite_url("/v1/chat/completions", &account) == url);
        assert!(backend.responses_via_chat());
        let mut body = json!({"model": "cline/anthropic/claude-sonnet-4-6", "prompt_cache_key": "k", "store": false});
        backend.prepare_body(&mut body);
        assert_eq!(body["model"], "anthropic/claude-sonnet-4-6");
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("store").is_none());
    }

    #[test]
    fn payment_required_cools_and_forbidden_stays_healthy() {
        let backend = ClineBackend { cooldown_ms: 30_000 };
        let headers = HeaderMap::new();
        assert!(matches!(
            backend.classify(402, "", &headers, 1_000),
            crate::models::Outcome::QuotaExhausted { until_ms: 31_000 }
        ));
        assert_eq!(backend.classify(403, "", &headers, 1_000), crate::models::Outcome::Ok);
        assert_eq!(backend.classify(401, "", &headers, 1_000), crate::models::Outcome::AuthFailed);
    }

    #[test]
    fn stream_usage_is_requested_once() {
        let backend = ClineBackend { cooldown_ms: 1000 };
        let mut body = json!({"stream": true});
        assert!(backend.auto_stream_usage("/v1/responses", &mut body));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert!(!backend.auto_stream_usage("/v1/chat/completions", &mut body));
        let mut explicit = json!({"stream": true, "stream_options": {"include_usage": false}});
        assert!(!backend.auto_stream_usage("/v1/chat/completions", &mut explicit));
        assert_eq!(explicit["stream_options"], json!({"include_usage": false}));
    }

    #[test]
    fn identity_prefers_email_then_name() {
        assert_eq!(
            parse_user(&json!({"email": "a@example.test", "name": "A"})),
            Some("a@example.test".into())
        );
        assert_eq!(
            parse_user(&json!({"name": "A"})),
            Some("A".into())
        );
        assert_eq!(parse_user(&json!({})), None);
    }

    #[test]
    fn blank_email_falls_back_to_name() {
        assert_eq!(
            parse_user(&json!({"email": "", "name": "A"})),
            Some("A".into())
        );
        assert_eq!(
            parse_user(&json!({"email": "   ", "name": "A"})),
            Some("A".into())
        );
        assert_eq!(parse_user(&json!({"email": "", "name": ""})), None);
    }

    #[test]
    fn default_catalog_covers_clinepass() {
        let catalog = default_catalog();
        assert!(catalog.iter().any(|model| model.id == "cline/cline-pass/glm-5.3"));
        assert!(catalog.iter().all(|model| model.id.starts_with(MODEL_PREFIX)));
    }

    #[test]
    fn invalid_replace_target_is_not_reported_as_a_rejected_key() {
        // A stale Re-key dialog must not blame a working key: the two failures render differently.
        let rejected = crate::cline::AuthError::Failed.to_string();
        let stale = crate::cline::AuthError::BadReplaceTarget.to_string();
        assert_ne!(rejected, stale);
        assert!(
            stale.to_lowercase().contains("re-key target"),
            "message must name the stale target, got: {stale}"
        );
    }

    #[test]
    fn rekey_keeps_an_operator_set_label() {
        // The Re-key dialog submits no label, so the stored one must survive.
        assert_eq!(
            resolve_relabel("work", None, Some("me@example.com".into())),
            "work",
            "re-keying must not rename an operator-labeled account to the profile identity"
        );
        // An explicit label still wins.
        assert_eq!(
            resolve_relabel("work", Some("renamed".into()), Some("me@example.com".into())),
            "renamed"
        );
        // A blank stored label is not an operator label; identity, then `cline`, fill it.
        assert_eq!(
            resolve_relabel("", None, Some("me@example.com".into())),
            "me@example.com"
        );
        assert_eq!(resolve_relabel("   ", None, None), "cline");
    }
}
