//! OpenCode Go: the Console subscription served from `https://opencode.ai/inference/go`.
//!
//! Go is one gateway in front of several upstream wire APIs. This backend serves its **OpenAI**
//! surface only: the `/anthropic` and `/google` paths expect a Claude Code or Gemini client, and
//! underclass's Anthropic surface admits only Claude Code fingerprints, so a OpenCode Go account cannot be
//! pooled through it. Models Go serves exclusively over those two paths are therefore not
//! advertised in the catalog at all - see `wire_for`.
//!
//! Two things about the gateway are not obvious and are load-bearing:
//!
//! - Every request must carry a session id in `x-opencode-session`; without it Go answers
//!   `400 MissingSessionID` before it looks at the credential. The proxy's sticky key is that
//!   session id, and a client that identifies no session still gets one.
//! - The upstream path must match the model's API family (`/openai/v1/chat/completions` for the
//!   open-weight models, `/openai/v1/responses` for the GPT/Grok/Muse family). `wire_for` is the
//!   table that decides, and a `/v1/responses` request for a chat-family model is translated.

use crate::models::{Account, AccountStatus, BackendId, ModelInfo, Outcome, Surface, now_ms};
use crate::provider::Backend;
use crate::store::Store;
use http::HeaderMap;
use reqwest::Client;
use serde_json::Value;
use std::sync::Mutex;

/// The Go gateway base. Overridable so tests can point at a mock upstream, and so an operator with
/// a pay-as-you-go Console key can target `https://opencode.ai/inference` instead.
pub const DEFAULT_UPSTREAM: &str = "https://opencode.ai/inference/go";
/// The Console API, used to verify a key and learn who it belongs to.
pub const DEFAULT_CONSOLE: &str = "https://opencode.ai/console";
pub const MODEL_PREFIX: &str = "opencode-go/";
/// Go asks a client to identify itself with its own user agent rather than a generic SDK name, so
/// the client's own value travels upstream when it sent one. See `crate::user_agent`.
pub const USER_AGENT: &str = crate::user_agent::DEFAULT;

pub fn upstream_base() -> String {
    std::env::var("UNDERCLASS_GO_UPSTREAM")
        .map(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string())
}

pub fn console_base() -> String {
    std::env::var("UNDERCLASS_GO_CONSOLE")
        .map(|base| base.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| DEFAULT_CONSOLE.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected status {0}")]
    Status(u16),
    #[error("{0}")]
    Rejected(String),
    #[error("re-key target is missing or belongs to another backend")]
    BadReplaceTarget,
}

/// The upstream wire API a model speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// `/openai/v1/chat/completions`.
    Chat,
    /// `/openai/v1/responses`.
    Responses,
}

/// Strips the pool-facing `opencode-go/` namespace, yielding the id Go expects upstream.
pub const LEGACY_MODEL_PREFIX: &str = "opencode_go/";

pub fn upstream_model_id(model: &str) -> &str {
    model
        .strip_prefix(MODEL_PREFIX)
        .or_else(|| model.strip_prefix(LEGACY_MODEL_PREFIX))
        .unwrap_or(model)
}

pub fn pool_model_id(upstream: &str) -> String {
    format!("{MODEL_PREFIX}{upstream}")
}

/// Normalize a client-facing or persisted model id to the current `MODEL_PREFIX` namespace,
/// accepting the legacy `opencode_go/` prefix. Unprefixed ids pass through.
pub fn normalize_model_id(model: &str) -> String {
    if let Some(rest) = model.strip_prefix(LEGACY_MODEL_PREFIX) {
        return format!("{MODEL_PREFIX}{rest}");
    }
    model.to_string()
}

/// @cc [owner:ghuntley,label:pool] opencode-go-wire-family
/// `wire_for` MUST report the wire API `model` is served over on Go's OpenAI surface, and MUST
/// report `None` for an id Go serves only over its Anthropic or Gemini paths and for an id this
/// backend has not classified. A model whose family is unknown MUST NOT be advertised or routed:
/// guessing sends the request to the wrong upstream path, which Go answers with an error the
/// client cannot act on.
pub fn wire_for(model: &str) -> Option<Wire> {
    WIRE_FAMILY
        .iter()
        .find(|(id, _)| *id == upstream_model_id(model))
        .and_then(|(_, wire)| *wire)
}

/// @cc [owner:ghuntley,label:proxy] opencode-go-upstream-wire-agrees-with-translation
/// `upstream_wire` MUST report `Responses` only for a model this build classifies as
/// Responses-family and `Chat` for every other id, including one it cannot classify. The upstream
/// path and `responses_via_chat` MUST both be derived from it, so a request can never be sent to one
/// API while its body is shaped for the other.
fn upstream_wire(model: &str) -> Wire {
    match wire_for(model) {
        Some(Wire::Responses) => Wire::Responses,
        _ => Wire::Chat,
    }
}

/// The family of every Go model routable over the OpenAI surface, plus the ids Go serves only over
/// its Anthropic or Gemini paths (`None`). Kept in source because neither the public `/v1/models`
/// listing nor Go's error responses carry it; `docs/adr/fork/opencode-go-backend.md` records where
/// each row came from.
const WIRE_FAMILY: &[(&str, Option<Wire>)] = &[
    // Responses family: the AI SDK's OpenAI package against `/openai/v1/responses`.
    ("grok-4.6", Some(Wire::Responses)),
    ("grok-4.7", Some(Wire::Responses)),
    ("gpt-5.6-luna", Some(Wire::Responses)),
    ("gpt-6-luna", Some(Wire::Responses)),
    ("muse-spark-1.2-contributor", Some(Wire::Responses)),
    ("muse-spark-1.3-contributor", Some(Wire::Responses)),
    // Chat Completions family: the openai-compatible package against `/openai/v1/chat/completions`.
    ("beam-free", Some(Wire::Chat)),
    ("cat-jam-free", Some(Wire::Chat)),
    ("deepseek-v4-pro", Some(Wire::Chat)),
    ("deepseek-v4.1-flash", Some(Wire::Chat)),
    ("glm-5.1", Some(Wire::Chat)),
    ("glm-5.2", Some(Wire::Chat)),
    ("glm-5.3", Some(Wire::Chat)),
    ("glm-5.3-flash", Some(Wire::Chat)),
    ("hy3", Some(Wire::Chat)),
    ("hy4-preview", Some(Wire::Chat)),
    ("kimi-k2.6", Some(Wire::Chat)),
    ("kimi-k2.7-code", Some(Wire::Chat)),
    ("kimi-k3", Some(Wire::Chat)),
    ("longcat-2.0", Some(Wire::Chat)),
    ("mimo-v2.5", Some(Wire::Chat)),
    ("mimo-v2.6-flash", Some(Wire::Chat)),
    ("mimo-v2.6-pro", Some(Wire::Chat)),
    ("step-5-preview", Some(Wire::Chat)),
    // Anthropic Messages or Gemini only, so no OpenAI path serves them. They are listed rather
    // than omitted so a reader can tell "Go does not offer this over OpenAI" from "this build has
    // never heard of it" - the catalog and the refresh treat both as unroutable.
    ("claude-haiku-5-5", None),
    ("qwen3.7-plus", None),
    ("qwen3.8-flash", None),
    ("qwen3.8-max", None),
    ("minimax-m2.7", None),
    ("minimax-m3", None),
    ("gemini-4-argon-contributor", None),
];

/// Model metadata for the Go lineup, from the Console/Go endpoint tables and models.dev (which is
/// where the OpenCode client itself reads them). Go's own `/v1/models` listing carries no limits.
fn catalog_entries() -> &'static [(&'static str, &'static str, i64, i64, i64)] {
    &[
        ("beam-free", "Beam Free", 1_048_576, 1_048_576, 131_072),
        (
            "cat-jam-free",
            "Cat Jam Free",
            1_048_576,
            1_048_576,
            131_072,
        ),
        (
            "deepseek-v4-pro",
            "DeepSeek V4 Pro",
            1_000_000,
            1_000_000,
            384_000,
        ),
        (
            "deepseek-v4.1-flash",
            "DeepSeek V4.1 Flash",
            1_000_000,
            1_000_000,
            384_000,
        ),
        ("glm-5.1", "GLM-5.1", 204_800, 204_800, 131_072),
        ("glm-5.2", "GLM-5.2", 1_000_000, 1_000_000, 131_072),
        ("glm-5.3", "GLM-5.3", 1_000_000, 1_000_000, 131_072),
        (
            "glm-5.3-flash",
            "GLM-5.3-Flash",
            1_000_000,
            1_000_000,
            131_072,
        ),
        ("gpt-5.6-luna", "GPT-5.6 Luna", 1_050_000, 922_000, 128_000),
        ("gpt-6-luna", "GPT-6 Luna", 1_050_000, 922_000, 128_000),
        ("grok-4.6", "Grok 4.6", 500_000, 500_000, 500_000),
        ("grok-4.7", "Grok 4.7", 500_000, 500_000, 500_000),
        ("hy3", "Hy3", 256_000, 192_000, 128_000),
        ("hy4-preview", "Hy4 preview", 1_024_000, 1_024_000, 64_000),
        ("kimi-k2.6", "Kimi K2.6", 262_144, 262_144, 65_536),
        (
            "kimi-k2.7-code",
            "Kimi K2.7 Code",
            262_144,
            262_144,
            262_144,
        ),
        ("kimi-k3", "Kimi K3", 1_048_576, 1_048_576, 131_072),
        ("longcat-2.0", "LongCat-2.0", 1_000_000, 1_000_000, 131_072),
        ("mimo-v2.5", "MiMo-V2.5", 1_000_000, 1_000_000, 128_000),
        (
            "mimo-v2.6-flash",
            "MiMo-V2.6-Flash",
            1_048_576,
            1_048_576,
            131_072,
        ),
        (
            "mimo-v2.6-pro",
            "MiMo-V2.6-Pro",
            1_048_576,
            1_048_576,
            131_072,
        ),
        (
            "muse-spark-1.2-contributor",
            "Muse Spark 1.2 Contributor",
            1_048_576,
            1_048_576,
            131_072,
        ),
        (
            "muse-spark-1.3-contributor",
            "Muse Spark 1.3 Contributor",
            1_048_576,
            1_048_576,
            131_072,
        ),
        (
            "step-5-preview",
            "Step 5 Preview",
            1_000_000,
            1_000_000,
            65_536,
        ),
    ]
}

/// @cc [owner:ghuntley,label:pool] opencode-go-catalog-namespaced
/// `default_catalog` MUST expose every routable entry under the `opencode-go/` prefix, MUST take the
/// limits recorded for it here, MUST leave `scope` empty so a `402` cools the account rather than
/// an invented billing surface, and MUST NOT include an id whose wire family is unknown, so
/// nothing the backend cannot route is ever advertised to a client.
pub fn default_catalog() -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = catalog_entries()
        .iter()
        .filter(|(id, ..)| wire_for(id).is_some())
        .map(|(id, name, context, input, output)| ModelInfo {
            id: pool_model_id(id),
            name: (*name).to_string(),
            context: *context,
            input: *input,
            output: *output,
            scope: String::new(),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// @cc [owner:ghuntley,label:pool] opencode-go-catalog-servable-only
/// `parse_models_response` MUST return one namespaced entry per listed id this backend can route,
/// MUST drop every other listed id - including one this build has never classified, which has no
/// known upstream path - and MUST NOT panic on a malformed body or entry. It exists to describe a
/// listing, not to change the catalog: the routable set is this module's table, because an
/// unclassified id has no upstream path to route to.
pub fn parse_models_response(body: &Value) -> Vec<ModelInfo> {
    let Some(items) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let known = default_catalog();
    let mut out: Vec<ModelInfo> = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        if wire_for(id).is_none() {
            continue;
        }
        let id = pool_model_id(id);
        match known.iter().find(|model| model.id == id) {
            Some(model) => out.push(model.clone()),
            None => out.push(ModelInfo {
                id,
                name: String::new(),
                context: 0,
                input: 0,
                output: 0,
                scope: String::new(),
            }),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// @cc [owner:ghuntley,label:pool] opencode-go-refresh-preserves-operator-limits
/// `carry_forward_limits` MUST overwrite the `name`, `context`, `input`, and `output` of a refreshed
/// model with the stored entry for the same id, MUST keep the refreshed name when the stored one is
/// blank, and MUST NOT copy a stored entry onto an id that has no stored entry. The stored catalog
/// is the operator's copy - Go reports no limits at all, so this module's table only seeds it and
/// the catalog editor edits it - and a refresh that overwrote it would silently revert an edit the
/// UI invited.
fn carry_forward_limits(fetched: &mut [ModelInfo], previous: &[ModelInfo]) {
    for model in fetched.iter_mut() {
        let Some(old) = previous.iter().find(|old| old.id == model.id) else {
            continue;
        };
        if !old.name.trim().is_empty() {
            model.name = old.name.clone();
        }
        model.context = old.context;
        model.input = old.input;
        model.output = old.output;
    }
}

/// @cc [owner:ghuntley,label:pool] opencode-go-refresh-never-shrinks
/// `refresh_catalog` MUST publish exactly `default_catalog()` - this build's servable set - with the
/// stored name and limits carried forward, and MUST persist it to the store and the pool. Go's
/// listing MUST NOT add or remove an entry: it is not an authority in either direction, omitting ids
/// OpenCode Go still serves and carrying ids OpenCode Go refuses for this plan. A refresh that reaches upstream
/// therefore leaves the routable set unchanged, which is what makes a transient listing failure
/// harmless.
pub async fn refresh_catalog(
    client: &Client,
    store: &Store,
    pool: &Mutex<crate::pool::PoolCore>,
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
    // The body is read so a malformed answer is an error rather than a silent no-op, but the
    // listing itself decides nothing: see the contract above.
    let _body: Value = resp.json().await?;
    let mut models = default_catalog();
    carry_forward_limits(&mut models, &store.catalog(BackendId::OpencodeGo));
    store.set_catalog(BackendId::OpencodeGo, &models);
    pool.lock()
        .unwrap()
        .set_catalog(BackendId::OpencodeGo, &models);
    pool.lock().unwrap().rebuild_aliases(store);
    Ok(models)
}

/// @cc [owner:ghuntley,label:auth] opencode-go-identity-label
/// `parse_identity` MUST return the workspace id (`x-opencode-org-id`) the Console config reports
/// for the provider it authenticates, so two Go subscriptions on one host are distinguishable, and
/// MUST return `None` when the body carries none. It MUST NOT fall back to a key-derived or
/// provider-name value, which would label every account the same.
pub fn parse_identity(body: &Value) -> Option<String> {
    body.get("providers")?
        .as_object()?
        .values()
        .filter_map(|provider| {
            provider
                .get("headers")
                .and_then(|headers| headers.get("x-opencode-org-id"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .find(|workspace| !workspace.is_empty())
        .map(str::to_string)
}

/// @cc [owner:ghuntley,label:auth] opencode-go-key-verification
/// `verify_key` MUST call the Console config endpoint with the candidate key, MUST report
/// `AuthError::Rejected` for a `401`, and MUST keep every other non-success status as its own
/// error: Console answers `403` for a workspace whose SSO sign-in is missing, and reporting that
/// as a rejected key would send the operator to re-issue a working key.
async fn verify_key(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Result<Option<String>, AuthError> {
    let resp = client
        .get(format!("{}/api/v2/config", console_base()))
        .bearer_auth(api_key)
        .header(
            http::header::USER_AGENT,
            crate::user_agent::resolve(None, user_agent),
        )
        .send()
        .await?;
    if resp.status().as_u16() == 401 {
        return Err(AuthError::Rejected("Console rejected the API key".into()));
    }
    if !resp.status().is_success() {
        return Err(AuthError::Status(resp.status().as_u16()));
    }
    Ok(resp
        .json()
        .await
        .ok()
        .and_then(|body| parse_identity(&body)))
}

pub async fn fetch_identity(
    client: &Client,
    api_key: &str,
    user_agent: Option<&str>,
) -> Option<String> {
    verify_key(client, api_key, user_agent).await.ok().flatten()
}

/// @cc [owner:ghuntley,label:auth] opencode-go-key-onboarding
/// Adding a OpenCode Go account MUST verify the operator-supplied key against the Console config endpoint
/// and MUST NOT persist an account when that verification fails, MUST store the key only
/// in the account's `refresh_token` column, and MUST reject an empty key. A `401` MUST surface as
/// `AuthError::Rejected` (the key was refused); a `403` MUST keep its own status so a workspace
/// that demands an SSO sign-in is not reported as a bad key, and any other upstream status MUST
/// keep its own error so a transient outage is not either. Replacing an existing account MUST keep
/// its id, clear any cooldown, and return it to `Healthy`; a replace id that is unknown or belongs
/// to another backend MUST be rejected with `AuthError::BadReplaceTarget`. A blank or
/// whitespace-only label MUST be treated as absent; an absent label MUST keep an existing
/// non-blank label on a replace, and otherwise fall back to the profile identity and then to
/// `opencode-go`.
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
        return Err(AuthError::Rejected(
            "an OpenCode API key is required".into(),
        ));
    }
    let identity = verify_key(client, api_key, user_agent).await?;
    let label = label.filter(|label| !label.trim().is_empty());
    let ts = now_ms();
    let account = match replace_account {
        Some(existing_id) => {
            let Some(mut account) = store.get_account(&existing_id) else {
                return Err(AuthError::BadReplaceTarget);
            };
            if account.backend != BackendId::OpencodeGo {
                return Err(AuthError::BadReplaceTarget);
            }
            account.refresh_token = Some(api_key.to_string());
            account.access_token = None;
            account.expires_at = 0;
            account.label = resolve_relabel(&account.label, label, identity);
            account.status = AccountStatus::Healthy;
            account.reset_at = 0;
            account.scope_cooling.clear();
            account.updated_at = ts;
            account
        }
        None => {
            let mut account = crate::flows::new_account(
                BackendId::OpencodeGo,
                label.or(identity).unwrap_or_else(|| "opencode-go".into()),
            );
            account.refresh_token = Some(api_key.to_string());
            account
        }
    };
    store.upsert_account(&account);
    Ok(account)
}

/// @cc [owner:ghuntley,label:auth] opencode-go-rekey-keeps-operator-label
/// `resolve_relabel` MUST keep the existing label when no explicit label is supplied, so re-keying
/// an account the operator named does not rename it to the identity. An explicit label wins, and
/// an existing blank label MAY be replaced by the identity and then by `opencode-go`.
fn resolve_relabel(existing: &str, label: Option<String>, identity: Option<String>) -> String {
    match label {
        Some(label) => label,
        None if existing.trim().is_empty() => identity.unwrap_or_else(|| "opencode-go".into()),
        None => existing.to_string(),
    }
}

/// The session id OpenCode Go requires, given the request's sticky key. OpenCode Go routes and caches by this value,
/// so the proxy's own affinity key is the right source; a client that identifies no session still
/// needs one or the request is refused before the credential is looked at, and a fresh id is the
/// only honest answer because there is no conversation to be stable about. The caller is
/// responsible for the value being usable as a header.
pub fn session_id(sticky: Option<&str>) -> String {
    sticky
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(crate::models::new_id)
}

pub struct OpencodeGoBackend {
    pub cooldown_ms: i64,
    /// The operator's configured outbound user agent, if any. OpenCode Go is a fork-local backend,
    /// so the identity its upstream sees is underclass's to choose: this wins over the client's own
    /// `User-Agent`, which wins over the built-in default. See `crate::user_agent`.
    pub user_agent: Option<String>,
}

/// @cc [owner:ghuntley,label:pool] opencode-go-unavailable-endpoint-is-a-model-fault
/// `names_an_unavailable_endpoint` MUST report true only for a body that says the endpoint is
/// unavailable, so a `402` from one model is cooled as that model's fault rather than as the whole
/// workspace's. A body without that phrase MUST report false.
pub fn names_an_unavailable_endpoint(body: &str) -> bool {
    body.to_ascii_lowercase()
        .contains("endpoint is unavailable")
}

impl Backend for OpencodeGoBackend {
    fn id(&self) -> BackendId {
        BackendId::OpencodeGo
    }

    fn surfaces(&self) -> &'static [Surface] {
        &[Surface::OpenAI]
    }

    fn default_cooldown_ms(&self) -> i64 {
        self.cooldown_ms
    }

    /// @cc [owner:ghuntley,label:proxy] opencode-go-upstream-path
    /// `rewrite_url` MUST target `/openai/v1/responses` for a model whose wire family is
    /// `Responses`, and `/openai/v1/chat/completions` for every other model regardless of which
    /// OpenAI-shaped path the client used, because Go serves each model over exactly one of them.
    fn rewrite_url(&self, _path: &str, model: &str, _account: &Account) -> String {
        let base = upstream_base();
        match upstream_wire(model) {
            Wire::Responses => format!("{base}/openai/v1/responses"),
            Wire::Chat => format!("{base}/openai/v1/chat/completions"),
        }
    }

    /// @cc [owner:ghuntley,label:proxy] opencode-go-responses-via-chat
    /// `responses_via_chat` MUST report true exactly when `upstream_wire` reports `Chat`, so a
    /// `/v1/responses` request routed to the chat endpoint is translated and its reply translated
    /// back. A Responses-family model MUST NOT be translated: Go serves it over the Responses API,
    /// which is the protocol the client already speaks.
    fn responses_via_chat(&self, model: &str) -> bool {
        upstream_wire(model) == Wire::Chat
    }

    /// @cc [owner:ghuntley,label:auth] opencode-go-rejected-key-is-final
    /// `refresh_on` MUST report false for every status: a OpenCode Go account holds a static Console API
    /// key, so there is no refresh that could redeem a rejection, and retrying the same account
    /// would replay the identical request with the identical key. A rejected key MUST therefore
    /// reach `classify` and be reported once.
    fn refresh_on(&self, _status: u16) -> bool {
        false
    }

    /// @cc [owner:ghuntley,label:proxy] opencode-go-credential-swap
    /// Outbound Go requests MUST carry the selected account's Console key as a bearer credential,
    /// MUST carry `x-opencode-session` (the sticky key, or a fresh id when the client named no
    /// session), and MUST NOT forward the client's own credential headers. The user agent is
    /// governed by `user-agent-precedence`.
    fn inject_headers(
        &self,
        _account: &Account,
        token: &str,
        sticky: Option<&str>,
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
        let session = http::HeaderValue::from_str(&session_id(sticky)).unwrap_or_else(|_| {
            http::HeaderValue::from_str(&crate::models::new_id()).expect("uuid")
        });
        headers.insert("x-opencode-session", session);
    }

    /// Go's gateway routes by model id, so the pool-facing namespace is stripped before the
    /// upstream call and the client's other request controls travel unchanged.
    fn prepare_body(&self, body: &mut Value) {
        let Some(object) = body.as_object_mut() else {
            return;
        };
        if let Some(model) = object.get("model").and_then(Value::as_str) {
            let upstream = upstream_model_id(model).to_string();
            object.insert("model".into(), Value::String(upstream));
        }
    }

    /// @cc [owner:ghuntley,label:accounting] opencode-go-stream-usage
    /// A streamed request for a Chat-family model MUST ask for the final usage chunk unless the
    /// client already chose `stream_options.include_usage`, and MUST leave a client's own choice
    /// alone. A Responses-family model MUST NOT gain `stream_options`: its upstream is the
    /// Responses API, which does not accept the parameter. The family is read from the body's
    /// model, which still carries the pool namespace at this point, because the client's own path
    /// says nothing about the upstream it will reach.
    fn auto_stream_usage(&self, _path: &str, body: &mut Value) -> bool {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if upstream_wire(model) != Wire::Chat
            || body.get("stream").and_then(Value::as_bool) != Some(true)
        {
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

    /// @cc [owner:ghuntley,label:pool] opencode-go-model-quota-scope
    /// Go meters usage per model, so `classify` MUST report `ModelQuotaExhausted` for a `429` - the
    /// cooldown applies to the requested model and the account keeps serving the rest of the
    /// catalog - and MUST use the upstream `Retry-After` deadline when one is present. A `402`
    /// naming an unavailable endpoint MUST also report `ModelQuotaExhausted`: Go answers a model it
    /// currently cannot serve that way, and cooling the whole account for one model's outage takes
    /// every other model out of rotation with it. Any other `402` MUST stay `QuotaExhausted` (the
    /// workspace balance is empty). `403` MUST stay `Transient`: Go answers it for a per-resource
    /// refusal, including a workspace with no Go subscription, and a key that is merely unsubscribed
    /// must not be ejected as if it were rejected.
    fn classify(&self, status: u16, body: &str, headers: &HeaderMap, now_ms: i64) -> Outcome {
        let until = || {
            crate::health::parse_retry_after_headers(headers, now_ms)
                .unwrap_or_else(|| now_ms.saturating_add(self.cooldown_ms))
        };
        if status == 429 {
            return Outcome::ModelQuotaExhausted { until_ms: until() };
        }
        if status == 402 {
            return if names_an_unavailable_endpoint(body) {
                Outcome::ModelQuotaExhausted { until_ms: until() }
            } else {
                Outcome::QuotaExhausted { until_ms: until() }
            };
        }
        if status == 403 {
            return Outcome::Transient;
        }
        crate::health::classify(status, headers, self.cooldown_ms, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backend() -> OpencodeGoBackend {
        OpencodeGoBackend {
            cooldown_ms: 30_000,
            user_agent: None,
        }
    }

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
        crate::flows::new_account(BackendId::OpencodeGo, "go".into())
    }

    #[test]
    fn namespace_round_trips() {
        assert_eq!(pool_model_id("kimi-k3"), "opencode-go/kimi-k3");
        assert_eq!(upstream_model_id("opencode-go/kimi-k3"), "kimi-k3");
        // An unprefixed id is passed through, so a hand-written catalog entry still routes.
        assert_eq!(upstream_model_id("kimi-k3"), "kimi-k3");
    }

    #[test]
    fn wire_family_is_known_for_every_advertised_model() {
        for model in default_catalog() {
            let id = upstream_model_id(&model.id);
            assert!(wire_for(id).is_some(), "{id} must declare a wire family");
        }
        assert_eq!(wire_for("grok-4.7"), Some(Wire::Responses));
        assert_eq!(wire_for("opencode-go/gpt-6-luna"), Some(Wire::Responses));
        assert_eq!(wire_for("glm-5.3"), Some(Wire::Chat));
        assert_eq!(wire_for("opencode-go/glm-5.3"), Some(Wire::Chat));
        // Anthropic/Gemini-only ids and ids this build has never seen are not routable here.
        assert_eq!(wire_for("claude-haiku-5-5"), None);
        assert_eq!(wire_for("qwen3.8-max"), None);
        assert_eq!(wire_for("gemini-4-argon-contributor"), None);
        assert_eq!(wire_for("omen-alpha"), None);
    }

    #[test]
    fn catalog_omits_models_this_surface_cannot_reach() {
        let catalog = default_catalog();
        let ids: Vec<&str> = catalog.iter().map(|model| model.id.as_str()).collect();
        assert!(ids.contains(&"opencode-go/kimi-k3"));
        assert!(ids.contains(&"opencode-go/gpt-6-luna"));
        assert!(!ids.contains(&"opencode-go/claude-haiku-5-5"));
        assert!(!ids.contains(&"opencode-go/minimax-m3"));
        assert!(!ids.contains(&"opencode-go/gemini-4-argon-contributor"));
        // Ids Go advertises but does not serve for this plan are not offered either: routing a
        // client to one produces a refusal the client can only retry.
        for dead in [
            "opencode-go/mimo-v2.5-pro",
            "opencode-go/space-bunny",
            "opencode-go/step-5-preview-free",
            "opencode-go/longcat-2.5-preview-free",
            "opencode-go/minimax-m3.1-flash-free",
        ] {
            assert!(
                !ids.contains(&dead),
                "{dead} is not servable and must not be offered"
            );
        }
        assert!(ids.iter().all(|id| id.starts_with(MODEL_PREFIX)));
    }

    #[test]
    fn listing_keeps_routable_ids_and_drops_the_rest() {
        let body = json!({"data": [
            {"id": "kimi-k3"},
            {"id": "claude-haiku-5-5"},
            {"id": "omen-alpha"},
            {"id": ""},
            {"no_id": true},
            {"id": "glm-5.3"}
        ]});
        let ids: Vec<String> = parse_models_response(&body)
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(ids, vec!["opencode-go/glm-5.3", "opencode-go/kimi-k3"]);
        assert!(parse_models_response(&json!({})).is_empty());
        assert!(parse_models_response(&json!({"data": "nope"})).is_empty());
    }

    #[test]
    fn listing_carries_the_recorded_limits() {
        let models = parse_models_response(&json!({"data": [{"id": "kimi-k3"}]}));
        assert_eq!(models[0].name, "Kimi K3");
        assert_eq!(models[0].context, 1_048_576);
        assert_eq!(models[0].output, 131_072);
    }

    #[test]
    fn operator_limits_survive_a_refresh() {
        let stored = vec![ModelInfo {
            id: "opencode-go/kimi-k3".into(),
            name: "Kimi K3".into(),
            context: 42,
            input: 41,
            output: 40,
            scope: String::new(),
        }];
        let mut fetched = parse_models_response(&json!({"data": [{"id": "kimi-k3"}]}));
        // The refresh carries this module's table, which is a seed, not the operator's copy.
        assert_eq!(fetched[0].context, 1_048_576);
        carry_forward_limits(&mut fetched, &stored);
        assert_eq!(fetched[0].context, 42);
        assert_eq!(fetched[0].input, 41);
        assert_eq!(fetched[0].output, 40);
        assert_eq!(
            fetched[0].name, "Kimi K3",
            "a stored name is the operator's too"
        );
    }

    #[test]
    fn a_blank_stored_name_keeps_the_recorded_one() {
        let stored = vec![ModelInfo {
            id: "opencode-go/kimi-k3".into(),
            name: "  ".into(),
            context: 0,
            input: 0,
            output: 0,
            scope: String::new(),
        }];
        let mut fetched = parse_models_response(&json!({"data": [{"id": "kimi-k3"}]}));
        carry_forward_limits(&mut fetched, &stored);
        assert_eq!(fetched[0].name, "Kimi K3");
    }

    #[test]
    fn an_unknown_id_never_inherits_another_models_limits() {
        let stored = vec![ModelInfo {
            id: "opencode-go/kimi-k3".into(),
            name: "Kimi K3".into(),
            context: 42,
            input: 41,
            output: 40,
            scope: String::new(),
        }];
        let mut fetched = vec![ModelInfo {
            id: "opencode-go/kimi-k3-preview".into(),
            name: String::new(),
            context: 0,
            input: 0,
            output: 0,
            scope: String::new(),
        }];
        carry_forward_limits(&mut fetched, &stored);
        assert_eq!(fetched[0].context, 0);
        assert_eq!(fetched[0].input, 0);
        assert_eq!(fetched[0].output, 0);
    }

    #[test]
    fn a_rejected_key_is_never_retried_on_the_same_account() {
        let backend = backend();
        // A OpenCode Go account holds a static Console key: no status can be redeemed by refreshing.
        for status in [401, 403, 429, 500] {
            assert!(
                !backend.refresh_on(status),
                "{status} must not trigger a refresh"
            );
        }
    }

    #[test]
    fn upstream_path_follows_the_model_family() {
        let backend = backend();
        let account = account();
        // The client's own path is irrelevant: Go serves each model over one API.
        assert_eq!(
            backend.rewrite_url("/v1/responses", "opencode-go/gpt-6-luna", &account),
            format!("{}/openai/v1/responses", upstream_base())
        );
        assert_eq!(
            backend.rewrite_url("/v1/chat/completions", "opencode-go/grok-4.7", &account),
            format!("{}/openai/v1/responses", upstream_base())
        );
        assert_eq!(
            backend.rewrite_url("/v1/responses", "opencode-go/kimi-k3", &account),
            format!("{}/openai/v1/chat/completions", upstream_base())
        );
        assert_eq!(
            backend.rewrite_url("/v1/chat/completions", "opencode-go/kimi-k3", &account),
            format!("{}/openai/v1/chat/completions", upstream_base())
        );
    }

    #[test]
    fn only_chat_family_models_translate_a_responses_request() {
        let backend = backend();
        assert!(backend.responses_via_chat("opencode-go/kimi-k3"));
        assert!(!backend.responses_via_chat("opencode-go/gpt-6-luna"));
        // An id this build cannot classify reaches the chat endpoint, so it must be translated
        // too: the path and the body shape are two views of one decision.
        assert_eq!(upstream_wire("opencode-go/never-heard-of-it"), Wire::Chat);
        assert!(backend.responses_via_chat("opencode-go/never-heard-of-it"));
        assert_eq!(
            backend.rewrite_url("/v1/responses", "opencode-go/never-heard-of-it", &account()),
            format!("{}/openai/v1/chat/completions", upstream_base())
        );
    }

    #[test]
    fn the_model_namespace_is_stripped_before_the_upstream_call() {
        let backend = backend();
        let mut body = json!({"model": "opencode-go/kimi-k3", "messages": []});
        backend.prepare_body(&mut body);
        assert_eq!(body["model"], "kimi-k3");
    }

    #[test]
    fn every_request_carries_a_session_id() {
        let backend = backend();
        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            Some("sess-1"),
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        assert_eq!(map.get("x-opencode-session").unwrap(), "sess-1");
        assert_eq!(
            map.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer oc_sk_key"
        );
        assert_eq!(map.get(http::header::USER_AGENT).unwrap(), USER_AGENT);

        // No session in the request still needs one: OpenCode Go refuses the request without it.
        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            None,
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        assert!(!map.get("x-opencode-session").unwrap().is_empty());
        // A blank sticky key is not a session id either.
        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            Some("   "),
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        assert!(!map.get("x-opencode-session").unwrap().is_empty());
    }

    #[test]
    fn an_unusable_session_value_never_drops_the_header() {
        let backend = backend();
        // A sticky key can carry anything the client put in `prompt_cache_key`; OpenCode Go refuses a
        // request without the header outright, so an unusable value must be replaced, not dropped.
        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            Some("bad\nsession"),
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        let session = map
            .get("x-opencode-session")
            .expect("the header must always be present");
        assert!(!session.to_str().unwrap().contains('\n'));
    }

    #[test]
    fn a_client_credential_never_reaches_the_upstream() {
        let backend = backend();
        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_selected",
            None,
            &json!({}),
            &headers(&[("authorization", "Bearer client-key")]),
            &mut map,
        );
        assert_eq!(
            map.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer oc_sk_selected"
        );
    }

    #[test]
    fn chat_streams_ask_for_usage_and_responses_do_not() {
        let backend = backend();
        let mut chat = json!({"model": "opencode-go/kimi-k3", "messages": [], "stream": true});
        assert!(backend.auto_stream_usage("/v1/chat/completions", &mut chat));
        assert_eq!(chat["stream_options"], json!({"include_usage": true}));

        // A Responses-family model reaches the Responses API even from the chat path, so it must
        // not gain a chat-only parameter.
        let mut responses =
            json!({"model": "opencode-go/gpt-6-luna", "messages": [], "stream": true});
        assert!(!backend.auto_stream_usage("/v1/chat/completions", &mut responses));
        assert!(responses.get("stream_options").is_none());

        // A client that already chose stays untouched.
        let mut chosen = json!({
            "model": "opencode-go/kimi-k3",
            "messages": [],
            "stream": true,
            "stream_options": {"include_usage": false, "other": 1}
        });
        assert!(!backend.auto_stream_usage("/v1/chat/completions", &mut chosen));
        assert_eq!(
            chosen["stream_options"],
            json!({"include_usage": false, "other": 1})
        );

        // A non-streaming request is left alone.
        let mut plain = json!({"model": "opencode-go/kimi-k3", "messages": []});
        assert!(!backend.auto_stream_usage("/v1/chat/completions", &mut plain));
        assert!(plain.get("stream_options").is_none());
    }

    #[test]
    fn a_rate_limit_cools_the_model_not_the_account() {
        let backend = backend();
        let map = headers(&[("retry-after", "120")]);
        assert!(matches!(
            backend.classify(429, "{}", &map, 1_000),
            Outcome::ModelQuotaExhausted { until_ms: 121_000 }
        ));
        assert!(matches!(
            backend.classify(429, "{}", &HeaderMap::new(), 1_000),
            Outcome::ModelQuotaExhausted { until_ms: 31_000 }
        ));
        assert!(matches!(
            backend.classify(402, "{}", &HeaderMap::new(), 1_000),
            Outcome::QuotaExhausted { until_ms: 31_000 }
        ));
        // Go reports a model it currently cannot serve as 402 "Endpoint is unavailable". That is
        // one model's outage, not the workspace's: cooling the account takes every other model out
        // of rotation with it.
        assert!(matches!(
            backend.classify(
                402,
                r#"{"error":{"type":"server_error","message":"Upstream request failed: Endpoint is unavailable."}}"#,
                &HeaderMap::new(),
                1_000
            ),
            Outcome::ModelQuotaExhausted { until_ms: 31_000 }
        ));
        assert!(matches!(
            backend.classify(
                402,
                r#"{"error":{"type":"server_error","message":"Upstream request failed: Insufficient account funds"}}"#,
                &HeaderMap::new(),
                1_000
            ),
            Outcome::QuotaExhausted { until_ms: 31_000 }
        ));
        // The free tier's client gate, a model outside the plan, and a workspace with no Go
        // subscription all arrive as 403; none of them is a rejected credential.
        for body in [
            r#"{"error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode."}}"#,
            r#"{"error":{"type":"server_error","message":"Upstream request failed: An active OpenCode Go subscription is required to use Go models."}}"#,
        ] {
            assert!(matches!(
                backend.classify(403, body, &HeaderMap::new(), 1_000),
                Outcome::Transient
            ));
        }
        assert!(matches!(
            backend.classify(401, "{}", &HeaderMap::new(), 1_000),
            Outcome::AuthFailed
        ));
        assert!(matches!(
            backend.classify(500, "{}", &HeaderMap::new(), 1_000),
            Outcome::Transient
        ));
    }

    #[test]
    fn identity_is_the_workspace_console_reports() {
        let body = json!({"providers": {"opencode": {
            "name": "Default / OpenCode",
            "headers": {"x-opencode-org-id": "wrk_01K6"},
            "models": {}
        }}});
        assert_eq!(parse_identity(&body).as_deref(), Some("wrk_01K6"));
        // A config without a workspace header names no identity; it must not invent one.
        assert_eq!(
            parse_identity(&json!({"providers": {"opencode": {"models": {}}}})),
            None
        );
        assert_eq!(parse_identity(&json!({})), None);
        assert_eq!(
            parse_identity(&json!({"providers": {"p": {"headers": {"x-opencode-org-id": "  "}}}})),
            None
        );
    }

    #[test]
    fn rekeying_keeps_the_operators_label() {
        assert_eq!(resolve_relabel("mine", None, Some("other".into())), "mine");
        assert_eq!(resolve_relabel("mine", Some("new".into()), None), "new");
        assert_eq!(
            resolve_relabel("  ", None, Some("mail@example.test".into())),
            "mail@example.test"
        );
        assert_eq!(resolve_relabel("  ", None, None), "opencode-go");
    }

    #[test]
    fn session_id_prefers_the_sticky_key() {
        assert_eq!(session_id(Some("sess-9")), "sess-9");
        assert_ne!(session_id(None), session_id(None));
        assert!(!session_id(Some(" ")).trim().is_empty());
    }
    /// The client's own user agent travels upstream, the built-in default covers a client that
    /// sent none, and the operator's configured override beats both.
    #[test]
    fn the_client_user_agent_is_forwarded_and_the_override_wins() {
        let backend = backend();
        let client = headers(&[("user-agent", "opencode/1.17.0")]);

        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            Some("sess-1"),
            &json!({}),
            &client,
            &mut map,
        );
        assert_eq!(
            map.get(http::header::USER_AGENT).unwrap(),
            "opencode/1.17.0"
        );

        let mut map = HeaderMap::new();
        backend.inject_headers(
            &account(),
            "oc_sk_key",
            Some("sess-1"),
            &json!({}),
            &HeaderMap::new(),
            &mut map,
        );
        assert_eq!(map.get(http::header::USER_AGENT).unwrap(), USER_AGENT);

        let configured = OpencodeGoBackend {
            cooldown_ms: 30_000,
            user_agent: Some("underclass-fleet/2".into()),
        };
        let mut map = HeaderMap::new();
        configured.inject_headers(
            &account(),
            "oc_sk_key",
            Some("sess-1"),
            &json!({}),
            &client,
            &mut map,
        );
        assert_eq!(
            map.get(http::header::USER_AGENT).unwrap(),
            "underclass-fleet/2"
        );
    }
}
