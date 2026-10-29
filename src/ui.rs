use crate::models::{AccountStatus, BackendId, RequestLogEntry, now_ms};
use crate::proxy::AppState;
use crate::store::UsageQuery;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Datelike;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub const UI_HTML: &str = include_str!("ui.html");

pub async fn index() -> impl IntoResponse {
    (
        [(http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        UI_HTML,
    )
}

/// @cc [owner:ghuntley,label:security] admin-token-gate
/// Every `/admin/api/*` route MUST be rejected with 401 unless the `Authorization` header carries
/// exactly `Bearer <ui_token>`; the HTML page at `/` MUST stay reachable without a token so the
/// token can be entered in the UI.
pub async fn require_ui_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let provided = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if provided != state.ui_token {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(req).await
}

fn account_view(state: &AppState, account: &crate::models::Account) -> serde_json::Value {
    let pool = state.pool.lock().unwrap();
    let inflight = pool
        .accounts
        .get(&account.id)
        .map(|s| s.inflight)
        .unwrap_or(0);
    let binding_count = pool
        .bindings()
        .iter()
        .filter(|b| b.account_id == account.id)
        .count();
    drop(pool);
    let now = now_ms();
    // Remaining time is computed here, not in the browser: an operator's clock is not the server's,
    // and an absolute deadline rendered client-side can show a live cooldown as expired.
    let mut cooling_scopes: Vec<serde_json::Value> = account
        .scope_cooling
        .iter()
        .map(|(scope, until)| {
            json!({
                "scope": scope,
                "until_ms": until,
                "remaining_ms": (until - now).max(0),
            })
        })
        .collect();
    cooling_scopes.sort_by_key(|entry| entry["remaining_ms"].as_i64().unwrap_or(0));
    json!({
        "id": account.id,
        "backend": account.backend.as_str(),
        "plan": account.plan_label(),
        "label": account.label,
        "status": account.status,
        "reset_at": account.reset_at,
        "cooling_remaining_ms": (account.reset_at - now).max(0),
        "cooling_scopes": cooling_scopes,
        "inflight": inflight,
        "sticky_sessions": binding_count,
        "account_id": account.account_id,
        "enterprise_url": account.enterprise_url,
        "created_at": account.created_at,
        "usage": state.resets.snapshot(&account.id),
        "observation": state.observations.lock().unwrap().get(&account.id).cloned(),
    })
}

pub async fn state(State(state): State<Arc<AppState>>) -> Response {
    let now = now_ms();
    let accounts: Vec<serde_json::Value> = state
        .store
        .list_accounts()
        .iter()
        .map(|a| account_view(&state, a))
        .collect();
    let requests: Vec<RequestLogEntry> = state.logs.lock().unwrap().iter().rev().cloned().collect();
    let catalog: serde_json::Map<String, serde_json::Value> = BackendId::all()
        .iter()
        .map(|b| (b.as_str().to_string(), json!(state.store.catalog(*b))))
        .collect();
    Json(json!({
        "accounts": accounts,
        "requests": requests,
        "catalog": catalog,
        "now_ms": now,
    }))
    .into_response()
}

fn usage_query(mut query: UsageQuery) -> Result<UsageQuery, Response> {
    if query.from_ms.is_none() && query.to_ms.is_none() {
        let now = chrono::Utc::now();
        let start = now
            .date_naive()
            .with_day(1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        query.from_ms = Some(start);
    }
    if query
        .from_ms
        .zip(query.to_ms)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err((StatusCode::BAD_REQUEST, "from_ms must precede to_ms").into_response());
    }
    if query
        .group_by
        .as_deref()
        .unwrap_or("")
        .split(',')
        .any(|part| !part.is_empty() && !matches!(part, "model" | "account_id" | "cache_key"))
    {
        return Err((StatusCode::BAD_REQUEST, "invalid group_by").into_response());
    }
    if query.cache_key.is_some() && query.missing_key == Some(true) {
        return Err((
            StatusCode::BAD_REQUEST,
            "cache_key conflicts with missing_key",
        )
            .into_response());
    }
    Ok(query)
}

/// @cc [owner:ghuntley,label:security] usage-admin-query
/// Usage summaries MUST be available only under the admin token gate and MUST report unknown
/// requests separately from measured input and output totals.
pub async fn usage_summary(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let query = match usage_query(query) {
        Ok(query) => query,
        Err(response) => return response,
    };
    match state.store.usage_summary(&query) {
        Ok(groups) => {
            let total_query = UsageQuery {
                group_by: None,
                ..query.clone()
            };
            match state.store.usage_summary(&total_query) {
                Ok(totals) => Json(json!({"groups": groups, "totals": totals.first(), "from_ms": query.from_ms, "to_ms": query.to_ms})).into_response(),
                Err(error) => { tracing::error!(error = %error, "usage.query_failed"); (StatusCode::INTERNAL_SERVER_ERROR, "usage query failed").into_response() }
            }
        }
        Err(error) => {
            tracing::error!(error = %error, "usage.query_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "usage query failed").into_response()
        }
    }
}

pub async fn usage_requests(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let query = match usage_query(query) {
        Ok(query) => query,
        Err(response) => return response,
    };
    match state.store.usage_records(&query) {
        Ok(requests) => {
            Json(json!({"requests": requests, "from_ms": query.from_ms, "to_ms": query.to_ms}))
                .into_response()
        }
        Err(error) => {
            tracing::error!(error = %error, "usage.query_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "usage query failed").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct StartFlowBody {
    pub backend: String,
    #[serde(default)]
    pub enterprise_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// @cc [owner:ghuntley,label:auth] cline-flow-verifies-key
/// A Cline onboarding request MUST create a flow record, validate the pasted API key, and report
/// either `authorized` with the persisted account id or `failed` with a message; it MUST NOT
/// persist an account when the key is empty or rejected upstream. When `replace_account` names an
/// existing account, a successful flow MUST keep that account's id rather than creating a second
/// account against the same upstream quota.
async fn start_cline_flow(
    state: &Arc<AppState>,
    api_key: Option<String>,
    label: Option<String>,
    replace_account: Option<String>,
) -> (crate::flows::FlowRegistry, String) {
    let flow_id = state.flows.create(
        BackendId::Cline,
        crate::flows::FlowState::Pending {
            user_code: String::new(),
            verify_url: "https://app.cline.bot/dashboard/api-keys".into(),
        },
        replace_account.clone(),
    );
    let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) else {
        state.flows.set_state(
            &flow_id,
            crate::flows::FlowState::Failed {
                message: "an API key from app.cline.bot is required".into(),
            },
        );
        return (state.flows.clone(), flow_id);
    };
    // `add_account` stores the trimmed key, so the catalog refresh must authenticate with that same
    // value; a pasted trailing newline would otherwise fail only the refresh.
    let api_key = api_key.trim().to_string();
    match crate::cline::add_account(
        &state.client,
        &state.store,
        &api_key,
        label,
        replace_account,
        state.cline_user_agent.as_deref(),
    )
    .await
    {
        Ok(account) => {
            state.pool.lock().unwrap().sync_account(account.clone());
            if let Err(error) = crate::cline::refresh_catalog(
                &state.client,
                &state.store,
                &state.pool,
                &api_key,
                state.cline_user_agent.as_deref(),
            )
            .await
            {
                crate::logging::log_catalog_refresh(&account, None, Some(&error.to_string()));
            }
            state.flows.set_state(
                &flow_id,
                crate::flows::FlowState::Authorized {
                    account_id: account.id,
                },
            );
        }
        Err(error) => {
            // Only a `Failed` error means the key itself was rejected; anything else is a transient
            // upstream problem and must not tell the operator to replace a working key.
            let message = match error {
                crate::cline::AuthError::Failed => "Cline rejected the API key".to_string(),
                crate::cline::AuthError::BadReplaceTarget => {
                    "that account no longer exists or is not a Cline account; close this dialog and re-key it from the account list".to_string()
                }
                other => format!("could not verify the Cline API key, try again ({other})"),
            };
            state
                .flows
                .set_state(&flow_id, crate::flows::FlowState::Failed { message });
        }
    }
    (state.flows.clone(), flow_id)
}

/// @cc [owner:ghuntley,label:auth] opencode-go-flow-verifies-key
/// An OpenCode Go onboarding request MUST create a flow record, validate the pasted API key, and
/// report either `authorized` with the persisted account id or `failed` with a message; it MUST NOT
/// persist an account when the key is empty or rejected upstream. When `replace_account` names an
/// existing account, a successful flow MUST keep that account's id rather than creating a second
/// account against the same subscription.
async fn start_go_flow(
    state: &Arc<AppState>,
    api_key: Option<String>,
    label: Option<String>,
    replace_account: Option<String>,
) -> (crate::flows::FlowRegistry, String) {
    let flow_id = state.flows.create(
        BackendId::OpencodeGo,
        crate::flows::FlowState::Pending {
            user_code: String::new(),
            verify_url: "https://opencode.ai/console/go".into(),
        },
        replace_account.clone(),
    );
    let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) else {
        state.flows.set_state(
            &flow_id,
            crate::flows::FlowState::Failed {
                message: "an OpenCode API key from opencode.ai/console is required".into(),
            },
        );
        return (state.flows.clone(), flow_id);
    };
    // `add_account` stores the trimmed key, so the catalog refresh must authenticate with that same
    // value; a pasted trailing newline would otherwise fail only the refresh.
    let api_key = api_key.trim().to_string();
    match crate::opencode_go::add_account(
        &state.client,
        &state.store,
        &api_key,
        label,
        replace_account,
        state.opencode_go_user_agent.as_deref(),
    )
    .await
    {
        Ok(account) => {
            state.pool.lock().unwrap().sync_account(account.clone());
            if let Err(error) = crate::opencode_go::refresh_catalog(
                &state.client,
                &state.store,
                &state.pool,
                &api_key,
                state.opencode_go_user_agent.as_deref(),
            )
            .await
            {
                crate::logging::log_catalog_refresh(&account, None, Some(&error.to_string()));
            }
            state.flows.set_state(
                &flow_id,
                crate::flows::FlowState::Authorized {
                    account_id: account.id,
                },
            );
        }
        Err(error) => {
            // Only a `Rejected` error means the key itself was refused; anything else is a transient
            // upstream problem and must not tell the operator to replace a working key.
            let message = match error {
                crate::opencode_go::AuthError::Rejected(why) => {
                    format!("OpenCode Console rejected the API key: {why}")
                }
                crate::opencode_go::AuthError::BadReplaceTarget => {
                    "that account no longer exists or is not an OpenCode Go account; close this dialog and re-key it from the account list".to_string()
                }

                other => format!("could not verify the OpenCode API key, try again ({other})"),
            };
            state
                .flows
                .set_state(&flow_id, crate::flows::FlowState::Failed { message });
        }
    }
    (state.flows.clone(), flow_id)
}

pub async fn start_flow(
    State(state): State<Arc<AppState>>,
    body: Option<Json<StartFlowBody>>,
) -> Response {
    let Some(Json(body)) = body else {
        return (StatusCode::BAD_REQUEST, "missing body").into_response();
    };
    let Some(backend) = BackendId::parse(&body.backend) else {
        return (StatusCode::BAD_REQUEST, "unknown backend").into_response();
    };
    if backend == BackendId::Cline || backend == BackendId::OpencodeGo {
        let replace_account = body.account_id.clone().filter(|id| !id.trim().is_empty());
        if let Some(account_id) = replace_account.as_deref() {
            let Some(account) = state.store.get_account(account_id) else {
                return (StatusCode::NOT_FOUND, "unknown account").into_response();
            };
            if account.backend != backend {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("account is not a {} account", backend.as_str()),
                )
                    .into_response();
            }
        }
        let (flows, flow_id) = match backend {
            BackendId::OpencodeGo => {
                start_go_flow(
                    &state,
                    body.api_key.clone(),
                    body.label.clone(),
                    replace_account,
                )
                .await
            }
            _ => {
                start_cline_flow(
                    &state,
                    body.api_key.clone(),
                    body.label.clone(),
                    replace_account,
                )
                .await
            }
        };
        return Json(json!({ "flow_id": flow_id, "flow": flows.get(&flow_id) })).into_response();
    }
    let result = match backend {
        BackendId::Codex => crate::codex::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            None,
        )
        .await
        .map_err(|e| e.to_string()),
        BackendId::Copilot => crate::copilot::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            None,
            body.enterprise_url.clone(),
        )
        .await
        .map_err(|e| e.to_string()),
        BackendId::Claude => crate::claude::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            None,
        )
        .await
        .map_err(|e| e.to_string()),
        BackendId::Cline | BackendId::OpencodeGo => unreachable!("handled above"),
    };
    match result {
        Ok(flow_id) => {
            let flow = state.flows.get(&flow_id);
            Json(json!({ "flow_id": flow_id, "flow": flow })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("failed to start device flow: {e}"),
        )
            .into_response(),
    }
}

pub async fn flow_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.flows.get(&id) {
        Some(flow) => Json(json!({ "flow_id": id, "flow": flow })).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown flow").into_response(),
    }
}

#[derive(Deserialize)]
pub struct FlowCodeBody {
    pub code: String,
}

/// @cc [owner:ghuntley,label:security] claude-flow-code-never-echoed
/// `submit_flow_code` MUST exchange the pasted authorization code for tokens and MUST NOT echo
/// the code, the state, or any token back to the caller.
pub async fn submit_flow_code(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<FlowCodeBody>,
) -> Response {
    match crate::claude::submit_code(
        state.client.clone(),
        state.store.clone(),
        state.flows.clone(),
        &id,
        &body.code,
    )
    .await
    {
        Ok(account_id) => {
            Json(json!({ "account_id": account_id, "status": "authorized" })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "flow_failed", "message": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn delete_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    state.store.delete_account(&id);
    state.pool.lock().unwrap().remove_account(&id);
    // The backend observation is keyed by account id and is never reaped elsewhere; a re-added
    // account gets a fresh uuid, so leaving it here would accumulate one entry per deletion.
    state.observations.lock().unwrap().remove(&id);
    StatusCode::NO_CONTENT.into_response()
}

pub async fn disable_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    set_status(&state, &id, AccountStatus::Disabled, 0)
}

/// @cc [owner:ghuntley,label:pool] enable-clears-recoverable-status
/// `enable_account` MUST return 404 for an unknown account id. For a `Disabled` or `AuthError`
/// account it MUST set the status to `Healthy` and clear both `reset_at` and every scoped cooling
/// deadline to 0, so an operator can recover an account that `PoolCore::select` would otherwise
/// never return. For a `Cooling` account it MUST preserve the `Cooling` status and its existing
/// `reset_at` deadline, since that deadline originates from upstream quota rather than an operator
/// action, and MUST leave scoped deadlines untouched. For an already `Healthy` account it MUST
/// leave the status `Healthy`, clear `reset_at`, and clear every scoped deadline.
/// The read, the pool mutation, and the persistence MUST happen in one critical section, so a quota
/// or auth transition that lands concurrently is neither lost to a stale snapshot nor resurrected by
/// the next `sync_from_store`.
pub async fn enable_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let mut pool = state.pool.lock().unwrap();
    let Some(current) = pool.account(&id).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown account").into_response();
    };
    let (next, reset_at, clear_scopes) = match current.status {
        AccountStatus::Cooling => (AccountStatus::Cooling, current.reset_at, false),
        AccountStatus::Disabled | AccountStatus::AuthError => (AccountStatus::Healthy, 0, true),
        AccountStatus::Healthy => (AccountStatus::Healthy, 0, true),
    };
    let mut updated = current;
    updated.status = next;
    updated.reset_at = reset_at;
    if clear_scopes {
        updated.scope_cooling.clear();
    }
    updated.updated_at = now_ms();
    state.store.update_account_health(&updated);
    pool.sync_account(updated);
    StatusCode::NO_CONTENT.into_response()
}

/// @cc [owner:ghuntley,label:auth] manual-refresh-endpoint
/// `refresh_account` MUST return 404 for an unknown account id and 409 `not_refreshable` for a
/// backend that does not `refreshes_with_issuer`. Otherwise it MUST call `force_refresh` exactly
/// once. On failure it MUST return 502 `refresh_failed` and MUST NOT change the account's status,
/// pool state, or cooldown. On success it MUST apply `status_after_manual_refresh` to both store and
/// pool, and MUST respond with the resulting status and token expiry but never any credential.
pub async fn refresh_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(account) = state.store.get_account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown_account", "message": "unknown account" })),
        )
            .into_response();
    };
    if !crate::tokens::refreshes_with_issuer(account.backend) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "not_refreshable",
                "message": format!("{} tokens do not expire or refresh", account.backend.as_str()),
            })),
        )
            .into_response();
    }
    if let Err(e) = state.tokens.force_refresh(&id).await {
        crate::logging::log_token_rotation(&id, &account.label, "rotation_failed", &e.to_string());
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "refresh_failed", "message": e.to_string() })),
        )
            .into_response();
    }
    crate::logging::log_token_rotation(&id, &account.label, "rotated", "manual");
    let refreshed = state.store.get_account(&id).unwrap_or(account);
    let status = match crate::tokens::status_after_manual_refresh(&refreshed) {
        Some(next) => {
            state.store.update_account_status(&id, next, 0);
            state.pool.lock().unwrap().set_status(&id, next, 0);
            next
        }
        None => refreshed.status,
    };
    Json(json!({
        "account_id": refreshed.id,
        "label": refreshed.label,
        "backend": refreshed.backend.as_str(),
        "outcome": "refreshed",
        "previous_status": refreshed.status,
        "status": status,
        "expires_at": refreshed.expires_at,
    }))
    .into_response()
}



pub async fn relogin_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(account) = state.store.get_account(&id) else {
        return (StatusCode::NOT_FOUND, "unknown account").into_response();
    };
    let result = match account.backend {
        BackendId::Codex => crate::codex::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            Some(id.clone()),
        )
        .await
        .map_err(|e| e.to_string()),
        BackendId::Copilot => crate::copilot::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            Some(id.clone()),
            account.enterprise_url.clone(),
        )
        .await
        .map_err(|e| e.to_string()),
        BackendId::Claude => crate::claude::start_flow(
            state.client.clone(),
            state.store.clone(),
            state.flows.clone(),
            Some(id.clone()),
        )
        .await
        .map_err(|e| e.to_string()),
        // A Cline or OpenCode key cannot be refreshed, so re-keying is a paste-a-new-key flow that
        // keeps this account's id and clears its cooldown. No polling flow is created: the browser
        // collects the new key and posts it to `/admin/api/flows` with this account id.
        BackendId::Cline | BackendId::OpencodeGo => {
            // The dialog needs to know which key box to show; the row the operator clicked already
            // says which backend this account belongs to.
            return Json(json!({ "rekey": id, "backend": account.backend.as_str() }))
                .into_response();
        }
    };
    match result {
        Ok(flow_id) => {
            let flow = state.flows.get(&flow_id);
            Json(json!({ "flow_id": flow_id, "flow": flow })).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("failed to start flow: {e}"),
        )
            .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct ResetQuery {
    #[serde(default)]
    pub force: bool,
}

/// @cc [owner:ghuntley,label:pool] manual-reset-endpoint-outcomes
/// `reset_account` MUST return 404 for an unknown account id and otherwise delegate to
/// `ResetManager::manual_reset` with the `force` query flag for a ChatGPT/Codex account, or to
/// the Claude Code redemption path for a Claude account. Every non-2xx response MUST carry a
/// JSON body with a stable `error` code and a human `message`; refusals that `force` can override
/// (`not_rate_limited`, `already_redeemed`) and `no_credit` MUST be 409, `not_codex` 400, and
/// upstream failures 502.
pub async fn reset_account(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<ResetQuery>,
) -> Response {
    use crate::resets::ManualResetError;
    let Some(account) = state.store.get_account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown_account", "message": "unknown account" })),
        )
            .into_response();
    };
    if account.backend == BackendId::Claude {
        let token = match state.tokens.access_token(&account).await {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!(error = %e, "claude.reset.token_unavailable");
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": "upstream_error", "message": "no usable credential" })),
                )
                    .into_response();
            }
        };
        return match crate::claude::manual_reset(
            &state.client,
            &account,
            &token,
            &state.pool,
            &state.store,
            query.force,
        )
        .await
        {
            Ok(outcome) => Json(json!({
                "account_id": account.id,
                "label": account.label,
                "outcome": outcome.outcome,
                "credit_id": outcome.credit_id,
                "status": outcome.status,
                "available_resets": outcome.available_resets,
            }))
            .into_response(),
            Err(error) => {
                // An upstream failure is transient: reporting it as a conflict would tell the
                // operator their request was rejected when retrying is the right response.
                let status = match error {
                    crate::claude::ClaudeResetError::Upstream(_) => StatusCode::BAD_GATEWAY,
                    _ => StatusCode::CONFLICT,
                };
                (
                    status,
                    Json(json!({ "error": error.code(), "message": error.message() })),
                )
                    .into_response()
            }
        };
    }
    match state
        .resets
        .manual_reset(
            &account,
            &state.tokens,
            &state.pool,
            &state.store,
            query.force,
        )
        .await
    {
        Ok(outcome) => Json(json!({
            "account_id": account.id,
            "label": account.label,
            "outcome": outcome.outcome,
            "credit_id": outcome.credit_id,
            "status": outcome.status,
            "available_resets": outcome.available_resets,
        }))
        .into_response(),
        Err(err) => {
            let status = match err {
                ManualResetError::NotCodex => StatusCode::BAD_REQUEST,
                ManualResetError::NotRateLimited
                | ManualResetError::AlreadyRedeemed
                | ManualResetError::NoCredit => StatusCode::CONFLICT,
                ManualResetError::Upstream(_) => StatusCode::BAD_GATEWAY,
            };
            (
                status,
                Json(json!({ "error": err.code(), "message": err.message() })),
            )
                .into_response()
        }
    }
}

fn set_status(state: &Arc<AppState>, id: &str, status: AccountStatus, reset_at: i64) -> Response {
    if state.store.get_account(id).is_none() {
        return (StatusCode::NOT_FOUND, "unknown account").into_response();
    }
    // Keep the pool and the store in one critical section: a health write outside it can race a
    // concurrent transition and be reloaded by `sync_from_store` as the newer state.
    let mut pool = state.pool.lock().unwrap();
    pool.set_status(id, status, reset_at);
    state.store.update_account_status(id, status, reset_at);
    // An operator decision supersedes any automatic disable, so the plan gate leaves it alone.
    state.store.set_disabled_reason(id, None);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct CatalogBody {
    pub models: Vec<crate::models::ModelInfo>,
}

pub async fn get_catalog(
    State(state): State<Arc<AppState>>,
    Path(backend): Path<String>,
) -> Response {
    let Some(backend) = BackendId::parse(&backend) else {
        return (StatusCode::NOT_FOUND, "unknown backend").into_response();
    };
    Json(json!({ "backend": backend.as_str(), "models": state.store.catalog(backend) }))
        .into_response()
}

pub async fn put_catalog(
    State(state): State<Arc<AppState>>,
    Path(backend): Path<String>,
    Json(body): Json<CatalogBody>,
) -> Response {
    let Some(backend) = BackendId::parse(&backend) else {
        return (StatusCode::NOT_FOUND, "unknown backend").into_response();
    };
    let mut models = body.models;
    if backend == BackendId::Cline {
        // A hand-edited or older client payload may omit `scope`; an empty scope would silently
        // return Cline to account-wide quota and reintroduce the credits-402 regression.
        crate::cline::backfill_scopes(&mut models);
    }
    state.store.set_catalog(backend, &models);
    state.pool.lock().unwrap().set_catalog(backend, &models);
    Json(json!({ "backend": backend.as_str(), "models": state.store.catalog(backend) }))
        .into_response()
}

/// @cc [owner:ghuntley,label:pool] cline-catalog-refresh-endpoint
/// `refresh_cline_catalog` MUST attempt a catalog refresh for every Cline account that holds a key,
/// MUST count only non-empty successful refreshes, and MUST report one `label: error` string per
/// failed account without failing the whole request.
pub async fn refresh_cline_catalog(State(state): State<Arc<AppState>>) -> Response {
    let accounts: Vec<_> = state
        .store
        .list_accounts()
        .into_iter()
        .filter(|account| account.backend == BackendId::Cline)
        .collect();
    let mut refreshed = 0usize;
    let mut errors = Vec::new();
    for account in accounts {
        let Some(key) = account.refresh_token.clone() else {
            continue;
        };
        match crate::cline::refresh_catalog(
            &state.client,
            &state.store,
            &state.pool,
            &key,
            state.cline_user_agent.as_deref(),
        )
        .await
        {
            Ok(models) if !models.is_empty() => refreshed += 1,
            Ok(_) => {}
            Err(error) => errors.push(format!("{}: {error}", account.label)),
        }
    }
    Json(json!({ "refreshed_accounts": refreshed, "errors": errors })).into_response()
}

/// @cc [owner:ghuntley,label:pool] opencode-go-catalog-refresh-endpoint
/// `refresh_opencode_go_catalog` MUST attempt a catalog refresh for every Go account that holds a key, MUST
/// count only non-empty successful refreshes, and MUST report one `label: error` string per failed
/// account without failing the whole request.
pub async fn refresh_opencode_go_catalog(State(state): State<Arc<AppState>>) -> Response {
    let accounts: Vec<_> = state
        .store
        .list_accounts()
        .into_iter()
        .filter(|account| account.backend == BackendId::OpencodeGo)
        .collect();
    let mut refreshed = 0usize;
    let mut errors = Vec::new();
    for account in accounts {
        let Some(key) = account.refresh_token.clone() else {
            continue;
        };
        match crate::opencode_go::refresh_catalog(
            &state.client,
            &state.store,
            &state.pool,
            &key,
            state.opencode_go_user_agent.as_deref(),
        )
        .await
        {
            Ok(models) if !models.is_empty() => refreshed += 1,
            Ok(_) => {}
            Err(error) => errors.push(format!("{}: {error}", account.label)),
        }
    }
    Json(json!({ "refreshed_accounts": refreshed, "errors": errors })).into_response()
}

pub async fn client_key(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "proxy_key": state.proxy_key })).into_response()
}

pub async fn refresh_copilot_catalog(State(state): State<Arc<AppState>>) -> Response {
    let accounts: Vec<_> = state
        .store
        .list_accounts()
        .into_iter()
        .filter(|a| a.backend == BackendId::Copilot)
        .collect();
    let mut refreshed = 0usize;
    let mut errors = Vec::new();
    for account in accounts {
        let Some(token) = account.refresh_token.clone() else {
            continue;
        };
        match crate::copilot::fetch_catalog(&state.client, &account, &token).await {
            Ok(models) if !models.is_empty() => {
                state.store.set_catalog(BackendId::Copilot, &models);
                state
                    .pool
                    .lock()
                    .unwrap()
                    .set_catalog(BackendId::Copilot, &models);
                refreshed += 1;
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{}: {e}", account.label)),
        }
    }
    Json(json!({ "refreshed_accounts": refreshed, "errors": errors })).into_response()
}
