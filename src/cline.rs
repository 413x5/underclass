use crate::models::{Account, AccountStatus, BackendId, ModelInfo, now_ms};
use crate::provider::Backend;
use crate::store::Store;
use http::HeaderMap;
use reqwest::Client;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

pub const DEFAULT_API_BASE: &str = "https://api.cline.bot";
pub const MODEL_PREFIX: &str = "cline/";

/// The public catalog Cline's own clients read their featured models from. Its `clinePass` bucket
/// is the only published list of ClinePass subscription models - `GET /api/v1/models` omits them.
pub const RECOMMENDED_MODELS_PATH: &str = "/api/v1/ai/cline/recommended-models";

/// The namespace Cline gives its subscription models, before `MODEL_PREFIX` is applied.
pub const CLINE_PASS_NAMESPACE: &str = "cline-pass/";

/// The recommended-models buckets that carry models the public `/api/v1/models` listing omits.
/// `recommended` is deliberately absent: it repeats the listing, so reading it would add nothing.
pub const FEED_MODEL_BUCKETS: &[&str] = &["clinePass", "free"];

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

/// The billing surface an upstream Cline model id draws from. ClinePass subscription models bill
/// against the subscription; every other id spends credits, which are account-level.
pub const SCOPE_CLINE_PASS: &str = "cline-pass";
pub const SCOPE_CREDITS: &str = "credits";

/// @cc [owner:ghuntley,label:pool] cline-billing-scope
/// `quota_scope` MUST return `cline-pass` for an upstream id in the `cline-pass/` namespace and
/// `credits` for every other id, so a `402` cools only the billing surface that reported it.
pub fn quota_scope(upstream_id: &str) -> &'static str {
    if upstream_id.starts_with("cline-pass/") {
        SCOPE_CLINE_PASS
    } else {
        SCOPE_CREDITS
    }
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
            scope: quota_scope(id).to_string(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// @cc [owner:ghuntley,label:pool] cline-catalog-scope-backfill
/// A catalog row stored before scopes existed carries an empty scope. `backfill_scopes` MUST set the
/// scope every current Cline model derives from its id, so an in-place upgrade routes per billing
/// surface without waiting for the next `/models` refresh. A non-empty stored scope MUST be kept.
pub fn backfill_scopes(models: &mut [ModelInfo]) -> bool {
    let mut changed = false;
    for model in models.iter_mut() {
        if !model.scope.is_empty() {
            continue;
        }
        // Every id in this catalog is namespaced by `parse_models_response`/`default_catalog`.
        model.scope = quota_scope(upstream_model_id(&model.id)).to_string();
        changed = true;
    }
    changed
}

/// @cc [owner:ghuntley,label:pool] cline-recommended-catalog-from-feed
/// `parse_recommended_models` MUST expose every entry of the response's `clinePass` and `free`
/// buckets under the `cline/` prefix, because `GET /api/v1/models` lists neither: without them a
/// published subscription or free model is unroutable until a table change. It MUST assign the
/// quota scope the entry's id derives, MUST NOT read the `recommended` bucket (that bucket repeats
/// the listing, which already describes those models), MUST skip an entry whose id is empty,
/// MUST use the entry's `name` when present and non-blank, otherwise its id, and MUST return an
/// empty list - never panic - when a bucket is absent or mistyped.
pub fn parse_recommended_models(body: &Value) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = Vec::new();
    for bucket in FEED_MODEL_BUCKETS {
        let Some(items) = body.get(bucket).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id);
            out.push(ModelInfo {
                id: pool_model_id(id),
                name: name.to_string(),
                context: 0,
                input: 0,
                output: 0,
                scope: quota_scope(id).to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// @cc [owner:ghuntley,label:pool] cline-catalog-keeps-clinepass
/// A refreshed Cline catalog MUST retain the ClinePass subscription models even when the upstream
/// `/models` listing omits them, so a ClinePass subscription stays routable through the pool.
/// `merge_catalog` MUST merge the documented set; a refresh that read Cline's recommended-models
/// feed MUST merge that feed's set instead, because the feed is the authoritative list.
pub fn merge_catalog(fetched: Vec<ModelInfo>) -> Vec<ModelInfo> {
    merge_discovered_models(fetched, &default_catalog())
}

/// @cc [owner:ghuntley,label:pool] cline-discovered-merge-keeps-existing-row
/// `merge_discovered_models` MUST keep every model already present, MUST add each discovered model
/// whose id is absent, and on a same-id collision MUST combine the two rows with `merge_row` rather
/// than replacing one, so a source that carries limits is never discarded for a source that does
/// not. It MUST return a list sorted by id, and NO returned row may carry an empty scope: every row
/// MUST name the billing surface its id derives, because an empty scope silently means account-wide
/// quota and would let a credits `402` take the ClinePass subscription out of rotation.
pub fn merge_discovered_models(
    mut fetched: Vec<ModelInfo>,
    discovered: &[ModelInfo],
) -> Vec<ModelInfo> {
    for model in discovered {
        match fetched.iter_mut().find(|existing| existing.id == model.id) {
            Some(existing) => *existing = merge_row(existing, model),
            None => fetched.push(model.clone()),
        }
    }
    for model in fetched.iter_mut() {
        if model.scope.is_empty() {
            model.scope = quota_scope(upstream_model_id(&model.id)).to_string();
        }
    }
    fetched.sort_by(|a, b| a.id.cmp(&b.id));
    fetched.dedup_by(|a, b| a.id == b.id);
    fetched
}

/// @cc [owner:ghuntley,label:pool] cline-row-merge-keeps-the-better-field
/// `merge_row` MUST keep `existing`'s non-zero `context`/`input`/`output` and take `incoming`'s only
/// where `existing` has zero (zero means "this source did not report it"), MUST keep `existing`'s
/// non-empty scope, and MUST NOT change the id. Names are ordered by quality, not by arrival: a name
/// that is not the model's slug MUST beat one that is, so a documented human name wins over a feed
/// slug regardless of merge order, while a blank name never beats a real one.
fn merge_row(existing: &ModelInfo, incoming: &ModelInfo) -> ModelInfo {
    let pick = |current: i64, other: i64| if current != 0 { current } else { other };
    // A slug is what a source with nothing better reports: the id, its upstream form, or the last
    // segment of either (`kimi-k3` for `cline-pass/kimi-k3`).
    let upstream = upstream_model_id(&existing.id);
    let slug = upstream.rsplit('/').next().unwrap_or(upstream);
    let is_slug = |name: &str| {
        let name = name.trim();
        name.is_empty() || name == existing.id || name == upstream || name == slug
    };
    let name = if is_slug(&existing.name) && !is_slug(&incoming.name) {
        incoming.name.clone()
    } else {
        existing.name.clone()
    };
    ModelInfo {
        id: existing.id.clone(),
        name,
        context: pick(existing.context, incoming.context),
        input: pick(existing.input, incoming.input),
        output: pick(existing.output, incoming.output),
        scope: if existing.scope.is_empty() {
            incoming.scope.clone()
        } else {
            existing.scope.clone()
        },
    }
}

/// @cc [owner:ghuntley,label:pool] cline-feed-and-documented-compose
/// `subscription_catalog` MUST return the documented ClinePass models with the feed's models added
/// for ids the table does not document yet, so a newly published subscription model is routable
/// while an already documented one keeps its human name. A feed model whose id the table already
/// lists MUST NOT replace or rename the documented row.
pub fn subscription_catalog(feed: Vec<ModelInfo>) -> Vec<ModelInfo> {
    merge_discovered_models(default_catalog(), &feed)
}

/// Model IDs documented for the ClinePass subscription, seeded at boot and used when Cline's
/// recommended-models feed cannot be read, so the subscription stays visible either way.
pub fn default_catalog() -> Vec<ModelInfo> {
    let entries: &[(&str, &str)] = &[
        ("cline-pass/glm-5.3", "GLM-5.3"),
        ("cline-pass/glm-5.3-flash", "GLM-5.3 Flash"),
        ("cline-pass/kimi-k3", "Kimi K3"),
        ("cline-pass/kimi-k3.1-pro", "Kimi K3.1 Pro"),
        ("cline-pass/deepseek-v4-pro", "DeepSeek V4 Pro"),
        ("cline-pass/deepseek-v4.1-flash", "DeepSeek V4.1 Flash"),
        (
            "cline-pass/deepseek-v4.1-flash-1017",
            "DeepSeek V4.1 Flash 1017",
        ),
        ("cline-pass/mimo-v2.5", "MiMo-V2.5"),
        ("cline-pass/mimo-v2.5-pro", "MiMo-V2.5 Pro"),
        ("cline-pass/mimo-v2.6-flash", "MiMo-V2.6 Flash"),
        ("cline-pass/mimo-v2.6-pro", "MiMo-V2.6 Pro"),
        ("cline-pass/minimax-m3", "MiniMax M3"),
        ("cline-pass/minimax-m3.1-flash", "MiniMax M3.1 Flash"),
        (
            "cline-pass/muse-spark-1.3-contributor",
            "Muse Spark 1.3 Contributor",
        ),
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
            scope: quota_scope(id).to_string(),
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

async fn fetch_user(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Value, AuthError> {
    let resp = client
        .get(format!("{}/api/v1/users/me", api_base()))
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
    Ok(resp.json().await?)
}

pub async fn fetch_identity(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Option<String> {
    fetch_user(client, api_key, user_agent)
        .await
        .ok()
        .and_then(|body| parse_user(&body))
}

async fn fetch_listing(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Vec<ModelInfo>, AuthError> {
    let resp = client
        .get(format!("{}/api/v1/models", api_base()))
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

/// @cc [owner:ghuntley,label:pool] cline-recommended-feed-is-public
/// `fetch_recommended_models` MUST read Cline's recommended-models feed without an API key, so the
/// subscription catalog can be refreshed by a build that holds no Cline account, and MUST treat a
/// non-2xx answer as an error so a failed read is never confused with an empty feed.
async fn fetch_recommended_models(
    client: &Client,
    user_agent: Option<&str>,
) -> Result<Vec<ModelInfo>, AuthError> {
    let resp = client
        .get(format!("{}{}", api_base(), RECOMMENDED_MODELS_PATH))
        .timeout(Duration::from_secs(10))
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
    Ok(parse_recommended_models(&body))
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

/// @cc [owner:ghuntley,label:pool] cline-refresh-discovers-feed-models
/// `build_catalog` MUST publish the union of the `/models` listing, the models the
/// recommended-models feed reports, and the documented `default_catalog` rows, so a model Cline
/// publishes after this build becomes routable and an absent, failed or empty feed still leaves the
/// documented subscription models published. For one id it MUST combine the sources rather than let
/// whichever arrived first win: a non-zero limit from any source MUST survive, a documented name
/// MUST beat a feed slug, and the row MUST name the scope its id derives.
pub fn build_catalog(listing: Vec<ModelInfo>, feed: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let subscriptions = subscription_catalog(
        feed.iter()
            .filter(|model| model.scope == SCOPE_CLINE_PASS)
            .cloned()
            .collect(),
    );
    // Order matters for which name wins, so documented rows go last; limits and scope are combined
    // by `merge_row` instead of being taken from the winning row.
    let mut models = merge_discovered_models(listing, &feed);
    models = merge_discovered_models(models, &subscriptions);
    models
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
    user_agent: Option<&str>,
) -> Result<Vec<ModelInfo>, AuthError> {
    let listing = fetch_listing(client, api_key, user_agent).await?;
    if listing.is_empty() {
        return Ok(Vec::new());
    }
    let previous = store.catalog(BackendId::Cline);
    let feed = match fetch_recommended_models(client, user_agent).await {
        Ok(feed) => feed,
        Err(error) => {
            // The documented table keeps the subscription routable, but a feed-only model silently
            // stops being routable, which is exactly the symptom this catalog exists to prevent.
            // Say so rather than letting the next 503 be the first sign.
            tracing::warn!(error = %error, "catalog.cline_feed_unavailable");
            Vec::new()
        }
    };
    let mut models = build_catalog(listing, feed);
    carry_forward_limits(&mut models, &previous);
    store.set_catalog(BackendId::Cline, &models);
    pool.lock().unwrap().set_catalog(BackendId::Cline, &models);
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
    user_agent: Option<&str>,
) -> Result<Account, AuthError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(AuthError::Failed);
    }
    let identity = fetch_user(client, api_key, user_agent)
        .await
        .map_err(|error| match error {
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
    /// The operator's configured outbound user agent, if any. Cline is a fork-local backend, so
    /// the identity its upstream sees is underclass's to choose: this wins over the client's own
    /// `User-Agent`, which wins over the built-in default. See `crate::user_agent`.
    pub user_agent: Option<String>,
}

impl Backend for ClineBackend {
    fn id(&self) -> BackendId {
        BackendId::Cline
    }

    fn surfaces(&self) -> &'static [crate::models::Surface] {
        &[crate::models::Surface::OpenAI]
    }

    fn default_cooldown_ms(&self) -> i64 {
        self.cooldown_ms
    }

    /// @cc [owner:ghuntley,label:proxy] cline-responses-via-chat
    /// Cline exposes only Chat Completions, so `/responses` requests routed to this backend MUST be
    /// converted to `POST <base>/api/v1/chat/completions` and their bodies translated back to the
    /// Responses API before reaching the client.
    fn responses_via_chat(&self, _model: &str) -> bool {
        true
    }

    fn rewrite_url(&self, path: &str, _model: &str, _account: &Account) -> String {
        let base = api_base();
        if path.ends_with("/responses") || path.ends_with("/chat/completions") {
            format!("{base}/api/v1/chat/completions")
        } else {
            format!("{base}{path}")
        }
    }

    /// @cc [owner:ghuntley,label:pool] cline-402-quota
    /// Cline reports exhausted credits as `402 Payment Required`; `classify` MUST map it to
    /// `QuotaExhausted` with the configured cooldown so `PoolCore::report_for` can cool the model's
    /// billing surface rather than silently retrying a surface with no balance.
    /// `403` MUST NOT mark the account auth-failed: Cline uses it for per-resource access (for
    /// example a model outside the account's plan), and a valid key must stay in rotation for the
    /// models it can serve.
    fn classify(
        &self,
        status: u16,
        _body: &str,
        headers: &HeaderMap,
        now_ms: i64,
    ) -> crate::models::Outcome {
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

    /// Outbound Cline requests present the operator's configured user agent when one is set,
    /// otherwise the client's own `User-Agent` when the client sent one, otherwise
    /// `crate::user_agent::DEFAULT`; the rule is `user-agent-precedence`.
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
            http::HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer"),
        );
        headers.insert(
            http::header::USER_AGENT,
            crate::user_agent::resolve(
                crate::user_agent::client_value(client),
                self.user_agent.as_deref(),
            ),
        );
        headers.insert("X-Title", http::HeaderValue::from_static("underclass"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn account() -> Account {
        crate::flows::new_account(BackendId::Cline, "cline-test".into())
    }

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
            scope: String::new(),
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
                scope: String::new(),
            },
            ModelInfo {
                id: "cline/gone/model".into(),
                name: String::new(),
                context: 999,
                input: 999,
                output: 999,
                scope: String::new(),
            },
        ];
        let mut fetched = vec![
            ModelInfo {
                id: "cline/openai/gpt-5.4".into(),
                name: String::new(),
                context: 400_000,
                input: 0,
                output: 0,
                scope: String::new(),
            },
            ModelInfo {
                id: "cline/brand/new".into(),
                name: String::new(),
                context: 0,
                input: 0,
                output: 0,
                scope: String::new(),
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
        assert!(
            merged
                .iter()
                .any(|model| model.id == "cline/openai/gpt-5.4")
        );
        assert!(
            merged
                .iter()
                .any(|model| model.id == "cline/cline-pass/kimi-k3")
        );
    }

    /// The feed is the only place Cline publishes its ClinePass and free models, and it names both
    /// by slug. `deepseek-v4.1-flash-1017` is the case that motivated discovery: absent from
    /// `/api/v1/models`, present in `clinePass`.
    #[test]
    fn recommended_feed_yields_the_models_the_listing_omits() {
        let body = json!({"recommended": [{"id": "anthropic/claude-sonnet-5.5"}],
            "free": [
                {"id": "reflection/beam-free", "name": "Beam"},
                {"id": "cline-free/solar-mini4"}
            ],
            "clinePass": [
                {"id": "cline-pass/deepseek-v4.1-flash-1017", "name": "cline-pass/deepseek-v4.1-flash-1017"},
                {"id": "cline-pass/kimi-k3.1-pro", "name": "Kimi K3.1 Pro"},
            ],
            "clineCloud": [{"id": "cline-cloud/deepseek-v4.1-flash"}]});
        let models = parse_recommended_models(&body);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "cline/cline-free/solar-mini4",
                "cline/cline-pass/deepseek-v4.1-flash-1017",
                "cline/cline-pass/kimi-k3.1-pro",
                "cline/reflection/beam-free",
            ],
            "the free bucket is published only here; `recommended` repeats the listing"
        );
        for model in &models {
            assert_eq!(
                model.scope,
                quota_scope(upstream_model_id(&model.id)),
                "{} must bill against its own surface",
                model.id
            );
        }
        // A free model rides usage billing at $0, so it is not a subscription model.
        let free = models
            .iter()
            .find(|m| m.id == "cline/reflection/beam-free")
            .expect("free model present");
        assert_eq!(free.scope, SCOPE_CREDITS);
        assert_eq!(free.name, "Beam", "the feed's display name is kept");
        // The slug name is preserved when the feed has nothing better.
        assert_eq!(models[1].name, "cline-pass/deepseek-v4.1-flash-1017");
        // And the id round-trips to exactly what upstream expects.
        assert_eq!(
            upstream_model_id("cline/cline-pass/deepseek-v4.1-flash-1017"),
            "cline-pass/deepseek-v4.1-flash-1017"
        );
    }

    #[test]
    fn recommended_feed_skips_unusable_entries() {
        for body in [
            json!({}),
            json!({"clinePass": null}),
            json!({"clinePass": []}),
            json!({"clinePass": [{"no_id": true}, {"id": ""}], "free": []}),
        ] {
            assert!(
                parse_recommended_models(&body).is_empty(),
                "expected an empty feed for {body}"
            );
        }
        let models = parse_recommended_models(&json!({"clinePass": [
            {"id": "cline-pass/kimi-k3", "name": "   "}
        ]}));
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].name, "cline-pass/kimi-k3",
            "a blank name falls back to the id"
        );
    }

    /// A free model is published only by the feed, so it must survive the same merge a subscription
    /// model does - without inheriting the subscription's billing surface.
    #[test]
    fn free_models_from_the_feed_stay_routable_and_bill_credits() {
        let feed = parse_recommended_models(&json!({"free": [
            {"id": "reflection/beam-free", "name": "Beam"}
        ]}));
        let merged = build_catalog(Vec::new(), feed);
        let free = merged
            .iter()
            .find(|m| m.id == "cline/reflection/beam-free")
            .expect("a free model must stay routable");
        assert_eq!(free.scope, SCOPE_CREDITS);
        assert_eq!(free.name, "Beam");
        assert_eq!(
            upstream_model_id(&free.id),
            "reflection/beam-free",
            "a free id reaches upstream unchanged"
        );
    }

    /// The buckets are disjoint today, but the feed is not underclass's to control. An id in both a
    /// source that carries limits and one that does not must keep the limits and the better name,
    /// whichever order the sources are merged in.
    #[test]
    fn an_id_in_both_the_listing_and_the_feed_keeps_its_limits() {
        let listing = vec![ModelInfo {
            id: "cline/cline-pass/kimi-k3".into(),
            name: "kimi-k3".into(),
            context: 262_144,
            input: 200_000,
            output: 32_000,
            scope: String::new(),
        }];
        let feed = vec![ModelInfo {
            id: "cline/cline-pass/kimi-k3".into(),
            name: "cline-pass/kimi-k3".into(),
            context: 0,
            input: 0,
            output: 0,
            scope: SCOPE_CLINE_PASS.into(),
        }];
        // The documented table is the third source; `build_catalog` is what composes all three.
        let merged = build_catalog(listing, feed);
        let row = merged
            .iter()
            .find(|m| m.id == "cline/cline-pass/kimi-k3")
            .expect("the shared id is published once");
        assert_eq!(row.context, 262_144, "the listing's limits must survive");
        assert_eq!(row.input, 200_000);
        assert_eq!(row.output, 32_000);
        assert_eq!(row.name, "Kimi K3", "the documented name must win");
        assert_eq!(row.scope, SCOPE_CLINE_PASS);
        assert_eq!(
            merged
                .iter()
                .filter(|m| m.id == "cline/cline-pass/kimi-k3")
                .count(),
            1,
            "one row per id"
        );
    }

    /// A feed outage must not strand a subscription: the documented table stands in, and the
    /// listing's own credit models keep serving.
    #[test]
    fn a_failed_feed_keeps_the_documented_subscription_routable() {
        let listing = parse_models_response(&json!({"data": [{"id": "openai/gpt-5.4"}]}));
        let merged = build_catalog(listing, Vec::new());
        assert!(
            merged.iter().any(|m| m.id == "cline/openai/gpt-5.4"),
            "the listing must survive a feed outage"
        );
        assert!(
            merged
                .iter()
                .any(|m| m.id == "cline/cline-pass/deepseek-v4.1-flash-1017"),
            "a documented subscription model must stay routable without the feed"
        );
        assert!(
            merged.iter().all(|m| !m.scope.is_empty()),
            "no row may fall back to account-wide quota"
        );
    }

    /// The documented table gives a new subscription model a readable name without hiding it.
    #[test]
    fn feed_and_documented_models_compose() {
        let feed = parse_recommended_models(&json!({"clinePass": [
            {"id": "cline-pass/deepseek-v4.1-flash-1017"},
            {"id": "cline-pass/brand-new-model"}
        ]}));
        // The listing carries no `cline-pass/` id at all, which is what makes the feed necessary.
        let listing = parse_models_response(&json!({"data": [{"id": "openai/gpt-5.4"}]}));
        let merged = build_catalog(listing, feed);

        let documented = merged
            .iter()
            .find(|m| m.id == "cline/cline-pass/deepseek-v4.1-flash-1017")
            .expect("a published model is routable");
        assert_eq!(
            documented.name, "DeepSeek V4.1 Flash 1017",
            "the documented name must win over the feed's slug"
        );
        assert!(
            merged
                .iter()
                .any(|m| m.id == "cline/cline-pass/brand-new-model"),
            "a model the table does not know yet still becomes routable"
        );
        assert!(
            merged.iter().any(|m| m.id == "cline/openai/gpt-5.4"),
            "the listing must survive the merge"
        );
        let pass = merged
            .iter()
            .filter(|m| upstream_model_id(&m.id).starts_with(CLINE_PASS_NAMESPACE));
        assert!(
            pass.into_iter().all(|m| m.scope == SCOPE_CLINE_PASS),
            "every subscription row bills against the subscription"
        );
    }

    #[test]
    fn legacy_rows_gain_scopes_without_losing_existing_ones() {
        // An in-place upgrade leaves pre-scope rows with `scope: ""`, which would keep a credits
        // 402 cooling the whole account until the next `/models` refresh.
        let mut stored = vec![
            ModelInfo {
                id: "cline/anthropic/claude-sonnet-4-6".into(),
                name: "Claude".into(),
                context: 128_000,
                input: 120_000,
                output: 8_000,
                scope: String::new(),
            },
            ModelInfo {
                id: "cline/cline-pass/kimi-k3".into(),
                name: "Kimi K3".into(),
                context: 0,
                input: 0,
                output: 0,
                scope: String::new(),
            },
        ];
        assert!(backfill_scopes(&mut stored));
        assert_eq!(stored[0].scope, SCOPE_CREDITS);
        assert_eq!(stored[1].scope, SCOPE_CLINE_PASS);
        assert_eq!(
            stored[0].context, 128_000,
            "operator limits survive the backfill"
        );
        assert!(
            !backfill_scopes(&mut stored),
            "an already-scoped catalog is a no-op"
        );

        stored[0].scope = "operator-surface".into();
        assert!(
            !backfill_scopes(&mut stored),
            "an explicit scope is never overwritten"
        );
        assert_eq!(stored[0].scope, "operator-surface");
    }

    #[test]
    fn every_catalog_entry_carries_a_billing_scope() {
        assert_eq!(quota_scope("cline-pass/kimi-k3"), SCOPE_CLINE_PASS);
        assert_eq!(quota_scope("anthropic/claude-sonnet-4-6"), SCOPE_CREDITS);
        // A pass-looking id must be classified by its namespace, not by a substring.
        assert_eq!(quota_scope("openai/cline-pass/not-a-pass"), SCOPE_CREDITS);

        let fetched = parse_models_response(&json!({"data": [
            {"id": "anthropic/claude-sonnet-4-6"},
            {"id": "cline-pass/kimi-k3"}
        ]}));
        let merged = merge_catalog(fetched);
        for model in &merged {
            let upstream = upstream_model_id(&model.id);
            assert_eq!(
                model.scope,
                quota_scope(upstream),
                "{} must bill against its own surface",
                model.id
            );
        }
        let pass = merged
            .iter()
            .find(|m| m.id == "cline/cline-pass/kimi-k3")
            .expect("pass model present");
        assert_eq!(pass.scope, SCOPE_CLINE_PASS);
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
        assert_eq!(
            upstream_model_id("cline/anthropic/claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(upstream_model_id("gpt-5.5"), "gpt-5.5");
        assert_eq!(
            pool_model_id("cline-pass/kimi-k3"),
            "cline/cline-pass/kimi-k3"
        );
    }

    #[test]
    fn rewrites_completion_paths_and_strips_namespace() {
        let backend = ClineBackend {
            cooldown_ms: 1000,
            user_agent: None,
        };
        let account = crate::flows::new_account(BackendId::Cline, "x".into());
        let url = backend.rewrite_url(
            "/v1/responses",
            "cline/anthropic/claude-sonnet-4-6",
            &account,
        );
        assert!(url.ends_with("/api/v1/chat/completions"));
        assert!(
            backend.rewrite_url(
                "/v1/chat/completions",
                "cline/anthropic/claude-sonnet-4-6",
                &account
            ) == url
        );
        assert!(backend.responses_via_chat("cline/anthropic/claude-sonnet-4-6"));
        let mut body = json!({"model": "cline/anthropic/claude-sonnet-4-6", "prompt_cache_key": "k", "store": false});
        backend.prepare_body(&mut body);
        assert_eq!(body["model"], "anthropic/claude-sonnet-4-6");
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("store").is_none());
    }

    #[test]
    fn payment_required_cools_and_forbidden_stays_healthy() {
        let backend = ClineBackend {
            cooldown_ms: 30_000,
            user_agent: None,
        };
        let headers = HeaderMap::new();
        assert!(matches!(
            backend.classify(402, "", &headers, 1_000),
            crate::models::Outcome::QuotaExhausted { until_ms: 31_000 }
        ));
        assert_eq!(
            backend.classify(403, "", &headers, 1_000),
            crate::models::Outcome::Ok
        );
        assert_eq!(
            backend.classify(401, "", &headers, 1_000),
            crate::models::Outcome::AuthFailed
        );
    }

    #[test]
    fn stream_usage_is_requested_once() {
        let backend = ClineBackend {
            cooldown_ms: 1000,
            user_agent: None,
        };
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
        assert_eq!(parse_user(&json!({"name": "A"})), Some("A".into()));
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
        assert!(
            catalog
                .iter()
                .any(|model| model.id == "cline/cline-pass/glm-5.3")
        );
        assert!(
            catalog
                .iter()
                .all(|model| model.id.starts_with(MODEL_PREFIX))
        );
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
            resolve_relabel(
                "work",
                Some("renamed".into()),
                Some("me@example.com".into())
            ),
            "renamed"
        );
        // A blank stored label is not an operator label; identity, then `cline`, fill it.
        assert_eq!(
            resolve_relabel("", None, Some("me@example.com".into())),
            "me@example.com"
        );
        assert_eq!(resolve_relabel("   ", None, None), "cline");
    }

    /// Cline is a fork-local backend, so it presents the client's own user agent, falls back to
    /// the built-in default when the client sent none, and lets the operator override both.
    #[test]
    fn the_client_user_agent_is_forwarded_and_the_override_wins() {
        let backend = ClineBackend {
            cooldown_ms: 1_000,
            user_agent: None,
        };
        let client = headers(&[("user-agent", "opencode/1.17.0")]);

        let mut map = HeaderMap::new();
        backend.inject_headers(&account(), "sk-cline", None, &json!({}), &client, &mut map);
        assert_eq!(
            map.get(http::header::USER_AGENT).unwrap(),
            "opencode/1.17.0"
        );

        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "sk-cline",
            None,
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        assert_eq!(
            map.get(http::header::USER_AGENT).unwrap(),
            crate::user_agent::DEFAULT
        );

        let configured = ClineBackend {
            cooldown_ms: 1_000,
            user_agent: Some("underclass-fleet/2".into()),
        };
        let mut map = HeaderMap::new();
        configured.inject_headers(&account(), "sk-cline", None, &json!({}), &client, &mut map);
        assert_eq!(
            map.get(http::header::USER_AGENT).unwrap(),
            "underclass-fleet/2"
        );
    }
}
