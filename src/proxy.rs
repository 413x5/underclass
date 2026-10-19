use crate::logging;
use crate::models::{now_ms, BackendId, Outcome, RequestLogEntry};
use crate::pool::{Decision, PoolCore, SelectError, Selection};
use crate::provider::BackendMap;
use crate::store::Store;
use crate::store::{UpstreamEvent, UsageRecord};
use crate::usage::{StreamEnd, StreamOutcome, UsageTap, usage_from_json};
use crate::tokens::TokenManager;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::request_id::RequestId;

pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const LOG_BUFFER: usize = 200;

pub struct AppState {
    pub store: Arc<Store>,
    pub pool: Arc<Mutex<PoolCore>>,
    pub tokens: Arc<TokenManager>,
    pub backends: Arc<BackendMap>,
    pub client: reqwest::Client,
    pub logs: Arc<Mutex<VecDeque<RequestLogEntry>>>,
    pub flows: crate::flows::FlowRegistry,
    pub proxy_key: Option<String>,
    pub ui_token: String,
    pub resets: Arc<crate::resets::ResetManager>,
    pub stream_usage_unsupported: Mutex<std::collections::HashSet<BackendId>>,
    /// How long usage rows, upstream events, and daily rollups are kept.
    pub stats_retention_days: i64,
}

/// @cc [owner:ghuntley,label:accounting] stream-finish-recorded
/// When a 2xx response body ends for any reason (EOF, upstream error, or client drop),
/// `UsageFinish` MUST persist exactly one usage row carrying the stream outcome, sanitized error
/// code, `saw_output`, and duration; MUST log `stream.finished`; MUST annotate the attempt's
/// ring-buffer entry; and MUST record an upstream event for every `failed` outcome and for an
/// `incomplete` outcome whose reason is a capacity, policy, or quota code.
struct UsageFinish {
    store: Arc<Store>,
    logs: Arc<Mutex<VecDeque<RequestLogEntry>>>,
    record: UsageRecord,
    tap: UsageTap,
    ended: Option<StreamEnd>,
}

impl Drop for UsageFinish {
    fn drop(&mut self) {
        if let Some(counts) = self.tap.counts() {
            self.record.input_tokens = Some(counts.input_tokens);
            self.record.output_tokens = Some(counts.output_tokens);
        }
        let report = self.tap.report(self.ended.unwrap_or(StreamEnd::Dropped));
        let duration_ms = (now_ms() - self.record.ts).max(0);
        self.record.duration_ms = Some(duration_ms);
        self.record.stream_outcome = Some(report.outcome.as_str().to_string());
        self.record.error_code = report.error_code.clone();
        self.record.saw_output = Some(report.saw_output);
        logging::log_stream_finished(
            &self.record.request_id,
            &self.record.model,
            &self.record.account_label,
            report.outcome.as_str(),
            report.error_code.as_deref(),
            report.saw_output,
            duration_ms,
        );
        if let Some(entry) = self
            .logs
            .lock()
            .unwrap()
            .iter_mut()
            .rev()
            .find(|entry| entry.request_id == self.record.request_id && entry.account.as_deref() == Some(self.record.account_id.as_str()))
        {
            entry.stream_outcome = self.record.stream_outcome.clone();
            entry.error_code = self.record.error_code.clone();
        }
        let kind = crate::usage::event_kind(report.error_code.as_deref());
        let notable = match report.outcome {
            StreamOutcome::Failed => true,
            StreamOutcome::Incomplete => kind != "upstream_failed",
            _ => false,
        };
        if notable {
            self.store.insert_upstream_event(&event_for(&self.record, kind, "stream", report.error_code.clone()));
        }
        // Written last: once the usage row is visible, the event and annotation already exist.
        persist_usage(&self.store, &self.record);
    }
}

fn event_for(record: &UsageRecord, kind: &str, detail: &str, error_code: Option<String>) -> UpstreamEvent {
    UpstreamEvent {
        ts: now_ms(),
        source: "proxy".into(),
        kind: kind.into(),
        detail: Some(detail.into()),
        error_code,
        backend: Some(record.backend.clone()),
        model: Some(record.model.clone()),
        account_id: Some(record.account_id.clone()),
        account_label: Some(record.account_label.clone()),
        request_id: Some(record.request_id.clone()),
        until_ms: None,
        dedupe_key: None,
    }
}

fn record_saturated(state: &AppState, request_id: &str, model: &str, until_ms: i64) {
    let now = now_ms();
    logging::log_saturated(request_id, until_ms, now);
    state.store.insert_upstream_event(&UpstreamEvent {
        ts: now,
        source: "proxy".into(),
        kind: "pool_saturated".into(),
        detail: Some("pool".into()),
        model: Some(model.to_string()),
        request_id: Some(request_id.to_string()),
        until_ms: Some(until_ms),
        ..Default::default()
    });
}

fn persist_usage(store: &Store, record: &UsageRecord) {
    let mut record = record.clone();
    if record.duration_ms.is_none() {
        record.duration_ms = Some((now_ms() - record.ts).max(0));
    }
    let record = &record;
    if let Err(error) = store.insert_usage(record) {
        tracing::error!(request_id = %record.request_id, error = %error, "usage.persist_failed");
    } else {
        tracing::info!(request_id = %record.request_id, measured = record.input_tokens.is_some(), input_tokens = record.input_tokens, output_tokens = record.output_tokens, "usage.recorded");
    }
}

/// `started_ms` is when the attempt began, so `ts .. ts + duration_ms` is the attempt's lifetime.
#[allow(clippy::too_many_arguments)]
fn usage_record(request_id: &str, path: &str, model: &str, sticky: Option<&str>, selection: &Selection, account: &crate::models::Account, status: u16, started_ms: i64) -> UsageRecord {
    UsageRecord {
        id: 0, request_id: request_id.to_string(), ts: started_ms, endpoint: path.to_string(), backend: selection.backend.as_str().to_string(), model: model.to_string(), account_id: selection.account_id.clone(), account_label: account.label.clone(), cache_key: sticky.map(str::to_string), status, input_tokens: None, output_tokens: None,
        ..Default::default()
    }
}

pub fn extract_sticky_key(body: &Value, session_header: Option<&str>) -> Option<String> {
    body.get("prompt_cache_key")
        .or_else(|| body.get("promptCacheKey"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| session_header.map(str::to_string))
        .filter(|s| !s.is_empty())
}

/// @cc [owner:ghuntley,label:proxy] enveloped-and-translated-bodies-require-sse
/// `upstream_is_sse` MUST report false for any non-event-stream content type so a buffered-body
/// request whose upstream answers with a single JSON document takes the buffered path. Consuming a
/// JSON body as SSE yields no frames, which would drop a translated response entirely and forward an
/// enveloped one as an unusable wrapper.
fn upstream_is_sse(response_content_type: &[u8]) -> bool {
    response_content_type.starts_with(b"text/event-stream")
}

fn usage_is_stream(body: &Value, content_type: &[u8]) -> bool {
    body.get("stream").and_then(Value::as_bool) == Some(true)
        || content_type.starts_with(b"text/event-stream")
}

/// @cc [owner:ghuntley,label:security] proxy-key-gate
/// When a proxy key is configured, requests to `/v1/*` MUST be rejected with 401 unless the
/// `Authorization` header carries exactly `Bearer <proxy_key>`. When no key is configured
/// (localhost-only mode) requests pass.
pub async fn require_proxy_key(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.proxy_key {
        let provided = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string);
        match provided {
            Some(key) if key == *expected => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({
                        "error": {"message": "invalid or missing proxy api key", "type": "authentication_error"}
                    })),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// @cc [owner:ghuntley,label:proxy] inflight-released-on-drop
/// Dropping `InflightGuard` MUST decrement the guarded account's in-flight counter exactly once —
/// including on early returns, error paths, and client-aborted streams.
struct InflightGuard {
    pool: Arc<Mutex<PoolCore>>,
    account_id: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.pool.lock().unwrap().release(&self.account_id);
    }
}

fn error_response(status: StatusCode, message: &str, error_type: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": {"message": message, "type": error_type}
        })),
    )
        .into_response()
}

fn upstream_error_response(status: StatusCode, content_type: &str, body: String) -> Response {
    (
        status,
        [
            (http::header::CONTENT_TYPE, content_type.to_string()),
        ],
        body,
    )
        .into_response()
}

fn saturated_response(until_ms: i64) -> Response {
    let now = now_ms();
    let retry_after_secs = ((until_ms - now).max(0) + 999) / 1000;
    let mut resp = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "all subscriptions for this model are out of quota; retry when the pool resets",
        "quota_exhausted",
    )
    .into_response();
    resp.headers_mut().insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_str(&retry_after_secs.to_string()).expect("retry-after"),
    );
    resp
}

fn attach_request_id(mut resp: Response, request_id: &str) -> Response {
    if let Ok(v) = http::HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

enum Attempt {
    Respond(Response),
    Failover(Option<Response>),
}

/// @cc [owner:ghuntley,label:proxy] saturation-fail-fast-429
/// When selection returns `Saturated`, `infer` MUST first make one automatic Codex reset attempt
/// for a live Codex-pool outage, then select again. If no account recovers, it MUST respond with
/// 429 and `Retry-After` equal to the seconds until `until_ms`.
pub async fn infer(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let started = Instant::now();
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .and_then(|r| r.header_value().to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let path = req.uri().path().to_string();
    let session_header = req
        .headers()
        .get("session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return attach_request_id(
                error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    &format!("request body error: {e}"),
                    "invalid_request_error",
                ),
                &request_id,
            );
        }
    };

    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return attach_request_id(
                error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("request body must be valid json: {e}"),
                    "invalid_request_error",
                ),
                &request_id,
            );
        }
    };

    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let sticky = extract_sticky_key(&body, session_header.as_deref());

    state.resets.maybe_reset(&state.pool, &state.store, &state.tokens, &request_id).await;

    let total_accounts = state.pool.lock().unwrap().accounts.len().max(1);
    let mut last_upstream: Option<Response> = None;
    let mut refreshed: std::collections::HashSet<String> = Default::default();
    let mut reset_after_quota = false;
    let mut attempts = 0;
    let mut max_attempts = total_accounts;

    while attempts < max_attempts {
        attempts += 1;
        let mut selection = state
            .pool
            .lock()
            .unwrap()
            .select(now_ms(), sticky.as_deref(), &model);

        if matches!(selection, Err(SelectError::Saturated { .. })) && !reset_after_quota {
            reset_after_quota = true;
            // Always select again: the reset attempt can wait on the shared reset lock and on
            // usage fetches, during which cooling deadlines may pass even if nothing was redeemed.
            state.resets.maybe_reset(&state.pool, &state.store, &state.tokens, &request_id).await;
            selection = state.pool.lock().unwrap().select(now_ms(), sticky.as_deref(), &model);
        }

        let selection = match selection {
            Ok(sel) => sel,
            Err(SelectError::Saturated { until_ms }) => {
                record_saturated(&state, &request_id, &model, until_ms);
                return attach_request_id(saturated_response(until_ms), &request_id);
            }
            Err(SelectError::NoAccounts) => {
                return attach_request_id(
                    last_upstream.take().unwrap_or_else(|| {
                        error_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "no subscriptions are configured for this model",
                            "no_accounts",
                        )
                    }),
                    &request_id,
                );
            }
        };

        logging::log_request_selected(
            &request_id,
            &model,
            selection.decision,
            selection.backend.as_str(),
            &selection.account_id,
            account_label(&state, &selection.account_id).as_deref(),
        );

        if selection.decision == Decision::NewBinding || selection.decision == Decision::StickyRebind {
            if let Some(key) = &sticky {
                if let Some(binding) = state.pool.lock().unwrap().bindings().into_iter().find(|b| b.cache_key == *key) {
                    state.store.upsert_binding(&binding);
                    state.store.prune_bindings(
                        now_ms(),
                        crate::pool::BINDING_TTL_MS,
                        crate::pool::DEFAULT_BINDING_CAP,
                    );
                }
            }
        }

        match attempt_account(
            &state,
            &request_id,
            &path,
            &model,
            sticky.as_deref(),
            &selection,
            body_bytes.clone(),
            &mut refreshed,
        )
        .await
        {
            Attempt::Respond(resp) => {
                logging::log_request_completed(&request_id, resp.status().as_u16(), started.elapsed().as_millis() as u64);
                return resp;
            }
            Attempt::Failover(passthrough) => {
                last_upstream = passthrough.or(last_upstream);
                if attempts == max_attempts && !reset_after_quota {
                    reset_after_quota = true;
                    if state.resets.maybe_reset(&state.pool, &state.store, &state.tokens, &request_id).await {
                        max_attempts += 1;
                    }
                }
            }
        }
    }

    let saturated = state.pool.lock().unwrap().select(now_ms(), sticky.as_deref(), &model);
    if let Err(SelectError::Saturated { until_ms }) = saturated {
        record_saturated(&state, &request_id, &model, until_ms);
        return attach_request_id(saturated_response(until_ms), &request_id);
    }

    attach_request_id(
        last_upstream.take().unwrap_or_else(|| {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "pool exhausted: every subscription attempt failed",
                "pool_exhausted",
            )
        }),
        &request_id,
    )
}

/// @cc [owner:ghuntley,label:proxy] failover-bounded-by-accounts
/// A request MUST make at most as many attempts as configured accounts, plus one additional attempt
/// after a successful banked reset, and the first successful upstream response MUST be returned
/// to the client; upstream error bodies MUST be preserved and
/// returned when every attempt fails (or a 503 `pool_exhausted` error when no upstream response
/// was ever received). An upstream 401 or 403 MUST trigger exactly one forced token refresh and one
/// same-account retry per request; a second 401 or 403 (or a failed refresh) MUST classify the
/// account `AuthFailed` and fail over.
async fn attempt_account(
    state: &Arc<AppState>,
    request_id: &str,
    path: &str,
    model: &str,
    sticky: Option<&str>,
    selection: &Selection,
    body_bytes: axum::body::Bytes,
    refreshed: &mut std::collections::HashSet<String>,
) -> Attempt {
    let started = Instant::now();
    let started_ms = now_ms();
    let Some(backend) = state.backends.get(&selection.backend).cloned() else {
        return Attempt::Failover(None);
    };
    let account = state.pool.lock().unwrap().account(&selection.account_id).cloned();
    let Some(account) = account else {
        return Attempt::Failover(None);
    };
    let label = Some(account.label.as_str());

    {
        let mut pool = state.pool.lock().unwrap();
        pool.acquire(&selection.account_id);
    }
    let guard = InflightGuard {
        pool: state.pool.clone(),
        account_id: selection.account_id.clone(),
    };

    let token = match state.tokens.access_token(&account).await {
        Ok(token) => token,
        Err(e) => {
            tracing::warn!(request_id = %request_id, error = %e, "token.unavailable");
            report_outcome(state, &selection.account_id, Outcome::AuthFailed);
            logging::log_account_state(&selection.account_id, label, "auth_error", "missing or unusable credential");
            drop(guard);
            return Attempt::Failover(None);
        }
    };

    let url = backend.rewrite_url(path, &account);
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/event-stream, application/json"),
    );
    let mut outbound_body: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let responses_path = path.ends_with("/responses");
    let translate_responses = responses_path && backend.responses_via_chat();
    if translate_responses {
        outbound_body = crate::responses::request_to_chat(&outbound_body);
    }
    backend.inject_headers(&account, &token, sticky, &outbound_body, &mut headers);
    backend.prepare_body(&mut outbound_body);
    let fallback_body = outbound_body.clone();
    let auto_usage = !state.stream_usage_unsupported.lock().unwrap().contains(&selection.backend)
        && backend.auto_stream_usage(path, &mut outbound_body);
    let outbound_bytes = serde_json::to_vec(&outbound_body).unwrap_or_else(|_| body_bytes.to_vec());

    let mut response = match state
        .client
        .post(&url)
        .headers(headers.clone())
        .body(outbound_bytes)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(request_id = %request_id, error = %e, backend = %selection.backend.as_str(), "upstream.network_error");
            let record = usage_record(request_id, path, model, sticky, selection, &account, 0, started_ms);
            persist_usage(&state.store, &record);
            report_outcome(state, &selection.account_id, Outcome::Transient);
            drop(guard);
            return Attempt::Failover(None);
        }
    };

    if auto_usage && matches!(response.status().as_u16(), 400 | 422) {
        let status = response.status();
        let rejection = response.text().await.unwrap_or_default();
        if rejection.contains("stream_options") || rejection.contains("include_usage") {
            let rejected = usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms);
            persist_usage(&state.store, &rejected);
            state.stream_usage_unsupported.lock().unwrap().insert(selection.backend);
            tracing::warn!(request_id = %request_id, backend = %selection.backend.as_str(), "usage.stream_option_unsupported");
            let retry_bytes = serde_json::to_vec(&fallback_body).unwrap_or_else(|_| body_bytes.to_vec());
            response = match state.client.post(&url).headers(headers).body(retry_bytes).send().await {
                Ok(resp) => resp,
                Err(error) => {
                    tracing::warn!(request_id = %request_id, error = %error, "upstream.network_error");
                    let record = usage_record(request_id, path, model, sticky, selection, &account, 0, started_ms);
                    persist_usage(&state.store, &record);
                    drop(guard);
                    return Attempt::Failover(None);
                }
            };
        } else {
            let record = usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms);
            persist_usage(&state.store, &record);
            push_log(state, request_id, model, Some(selection.backend.as_str().to_string()), Some(selection.account_id.clone()), Some(account.label.clone()), sticky.unwrap_or("none"), status.as_u16(), started.elapsed().as_millis() as u64);
            drop(guard);
            return Attempt::Failover(Some(upstream_error_response(status, "application/json", rejection)));
        }
    }

    let status = response.status();

    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        && refreshed.insert(selection.account_id.clone())
    {
        let body_text = response.text().await.unwrap_or_default();
        let mut record = usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms);
        if let Ok(value) = serde_json::from_str::<Value>(&body_text)
            && let Some(counts) = usage_from_json(&value, backend.usage_is_chat(path)) {
            record.input_tokens = Some(counts.input_tokens);
            record.output_tokens = Some(counts.output_tokens);
        }
        persist_usage(&state.store, &record);
        match state.tokens.force_refresh(&selection.account_id).await {
            Ok(_) => {
                report_outcome(state, &selection.account_id, Outcome::Transient);
                drop(guard);
                return Attempt::Failover(None);
            }
            Err(e) => {
                tracing::warn!(request_id = %request_id, error = %e, "token.refresh_failed");
                report_outcome(state, &selection.account_id, Outcome::AuthFailed);
                logging::log_account_state(&selection.account_id, label, "auth_error", "refresh rejected");
                drop(guard);
                return Attempt::Failover(None);
            }
        }
    }

    if !status.is_success() {
        let resp_headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let mut record = usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms);
        if let Ok(value) = serde_json::from_str::<Value>(&body_text) {
            if let Some(counts) = usage_from_json(&value, backend.usage_is_chat(path)) {
                record.input_tokens = Some(counts.input_tokens);
                record.output_tokens = Some(counts.output_tokens);
            }
            record.error_code = value
                .get("error")
                .and_then(crate::usage::error_code)
                .or_else(|| crate::usage::error_code(&value));
        }
        persist_usage(&state.store, &record);
        let kind = crate::usage::event_kind(record.error_code.as_deref());
        if record.error_code.is_some() && matches!(kind, "overloaded" | "policy_blocked") {
            state.store.insert_upstream_event(&event_for(&record, kind, "http", record.error_code.clone()));
        }
        let outcome = backend.classify(status.as_u16(), &body_text, &resp_headers, now_ms());
        let body_text = if translate_responses {
            crate::responses::error_body(&body_text)
        } else {
            body_text
        };
        if let Some(new_status) = {
            let mut pool = state.pool.lock().unwrap();
            pool.report(&selection.account_id, outcome.clone())
        } {
            let reset_at = {
                let pool = state.pool.lock().unwrap();
                pool.account(&selection.account_id).map(|a| a.reset_at).unwrap_or(0)
            };
            state
                .store
                .update_account_status(&selection.account_id, new_status, reset_at);
            match &outcome {
                Outcome::QuotaExhausted { until_ms } => {
                    logging::log_account_state(&selection.account_id, label, "cooling", &format!("until {until_ms}"));
                    let mut event = event_for(&record, "quota_cooling", "http", record.error_code.clone());
                    event.until_ms = Some(*until_ms);
                    state.store.insert_upstream_event(&event);
                }
                Outcome::AuthFailed => {
                    logging::log_account_state(&selection.account_id, label, "auth_error", "upstream rejected credentials");
                }
                _ => {}
            }
        }
        report_outcome(state, &selection.account_id, Outcome::Ok);
        drop(guard);
        push_log(
            state,
            request_id,
            model,
            Some(selection.backend.as_str().to_string()),
            Some(selection.account_id.clone()),
            Some(account.label.clone()),
            sticky.unwrap_or("none"),
            status.as_u16(),
            started.elapsed().as_millis() as u64,
        );
        Attempt::Failover(Some(upstream_error_response(
            status,
            &resp_headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json")
                .to_string(),
            body_text,
        )))
    } else {
        let mut content_type = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .cloned()
            .unwrap_or(http::HeaderValue::from_static("application/json"));
        let status_code = status;
        let streaming = usage_is_stream(&outbound_body, content_type.as_bytes());
        let envelope = backend.response_envelope();
        // The client's `stream: true` is a request, not a guarantee: an upstream may answer a
        // streaming request with a single JSON document. Streaming an enveloped or translated body
        // would forward an unusable wrapper (or drop it entirely), so both only stream when the
        // response really is an event stream and the buffered path handles the JSON answer.
        let needs_buffering = translate_responses || envelope.is_some();
        let streaming = if needs_buffering {
            upstream_is_sse(content_type.as_bytes())
        } else {
            streaming
        };
        let translate_stream = translate_responses && streaming;
        if !streaming && needs_buffering {
            let raw = match response.bytes().await {
                Ok(raw) => raw,
                Err(e) => {
                    return Attempt::Respond(attach_request_id(
                        error_response(
                            StatusCode::BAD_GATEWAY,
                            &format!("upstream body error: {e}"),
                            "upstream_error",
                        ),
                        request_id,
                    ));
                }
            };
            let body = unwrap_envelope(&raw, envelope);
            // Parse before the finish record exists: a 2xx body that is not the JSON we can convert
            // is a failure of this attempt, and dropping a record already marked `Eof` would record
            // it as a clean completion.
            let converted = if translate_responses {
                match serde_json::from_slice::<Value>(&body) {
                    Ok(chat) => Some(chat),
                    Err(e) => {
                        let mut finish = UsageFinish {
                            store: state.store.clone(),
                            logs: state.logs.clone(),
                            record: usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms),
                            tap: UsageTap::new(false, backend.usage_is_chat(path)),
                            // The body arrived and ended; it just was not convertible.
                            ended: Some(StreamEnd::UpstreamError),
                        };
                        finish.tap.feed(&body);
                        push_log(
                            state,
                            request_id,
                            model,
                            Some(selection.backend.as_str().to_string()),
                            Some(selection.account_id.clone()),
                            Some(account.label.clone()),
                            sticky.unwrap_or("none"),
                            status_code.as_u16(),
                            started.elapsed().as_millis() as u64,
                        );
                        drop(finish);
                        return Attempt::Respond(attach_request_id(
                            error_response(
                                StatusCode::BAD_GATEWAY,
                                &format!("upstream returned invalid json: {e}"),
                                "upstream_error",
                            ),
                            request_id,
                        ));
                    }
                }
            } else {
                None
            };
            push_log(
                state,
                request_id,
                model,
                Some(selection.backend.as_str().to_string()),
                Some(selection.account_id.clone()),
                Some(account.label.clone()),
                sticky.unwrap_or("none"),
                status_code.as_u16(),
                started.elapsed().as_millis() as u64,
            );
            let bytes = match converted {
                Some(chat) => {
                    let response_id = format!("resp_{}", request_id.replace('-', ""));
                    let converted = crate::responses::completion_to_response(
                        &chat,
                        model,
                        &response_id,
                        started_ms / 1000,
                    );
                    content_type = http::HeaderValue::from_static("application/json");
                    serde_json::to_vec(&converted).unwrap_or_default()
                }
                None => body.clone(),
            };
            let mut finish = UsageFinish {
                store: state.store.clone(),
                logs: state.logs.clone(),
                record: usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms),
                tap: UsageTap::new(false, backend.usage_is_chat(path)),
                ended: None,
            };
            finish.tap.feed(&body);
            finish.ended = Some(StreamEnd::Eof);
            // Dropping the finish record annotates the log entry pushed above.
            drop(finish);
            let mut resp = Response::builder()
                .status(status_code)
                .body(Body::from(bytes))
                .expect("valid response");
            resp.headers_mut().insert(http::header::CONTENT_TYPE, content_type);
            return Attempt::Respond(resp);
        }

        let stream = response.bytes_stream();
        let finish = UsageFinish {
            store: state.store.clone(),
            logs: state.logs.clone(),
            record: usage_record(request_id, path, model, sticky, selection, &account, status.as_u16(), started_ms),
            tap: UsageTap::new(streaming, backend.usage_is_chat(path)),
            ended: None,
        };
        let translator = translate_stream.then(|| {
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::responses::ChatStreamTranslator::new(
                    model,
                    &format!("resp_{}", request_id.replace('-', "")),
                    started_ms / 1000,
                ),
            ))
        });
        let body_stream = Body::from_stream(futures::stream::unfold(
            (stream, Some(guard), finish, translator),
            |state| async move {
                let (mut stream, guard, mut finish, translator) = state;
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        finish.tap.feed(&chunk);
                        let out = match &translator {
                            Some(translator) => translator.lock().unwrap().feed(&chunk),
                            None => chunk.to_vec(),
                        };
                        Some((Ok::<_, std::io::Error>(out), (stream, guard, finish, translator)))
                    },
                    Some(Err(_)) => {
                        finish.ended.get_or_insert(StreamEnd::UpstreamError);
                        // A translated stream must close with exactly one terminal event. If the
                        // translator already emitted it (e.g. after `[DONE]`), end the body cleanly
                        // rather than aborting a completed response.
                        match translator.as_ref().map(|t| t.lock().unwrap().finish_on_eof()) {
                            None => Some((
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::Interrupted,
                                    "upstream stream error",
                                )),
                                (stream, guard, finish, translator),
                            )),
                            Some(tail) if tail.is_empty() => None,
                            Some(tail) => {
                                Some((Ok::<_, std::io::Error>(tail), (stream, guard, finish, translator)))
                            }
                        }
                    }
                    None => {
                        finish.ended.get_or_insert(StreamEnd::Eof);
                        let tail = translator
                            .as_ref()
                            .map(|translator| translator.lock().unwrap().finish_on_eof())
                            .unwrap_or_default();
                        if tail.is_empty() {
                            None
                        } else {
                            Some((
                                Ok::<_, std::io::Error>(tail),
                                (stream, guard, finish, translator),
                            ))
                        }
                    }
                }
            },
        ));
        if translate_stream {
            content_type = http::HeaderValue::from_static("text/event-stream");
        }
        push_log(
            state,
            request_id,
            model,
            Some(selection.backend.as_str().to_string()),
            Some(selection.account_id.clone()),
            Some(account.label.clone()),
            sticky.unwrap_or("none"),
            status_code.as_u16(),
            started.elapsed().as_millis() as u64,
        );
        let mut resp = Response::builder()
            .status(status_code)
            .body(body_stream)
            .expect("valid stream response");
        resp.headers_mut().insert(http::header::CONTENT_TYPE, content_type);
        Attempt::Respond(resp)
    }
}

/// @cc [owner:ghuntley,label:proxy] response-envelope-helper-unwraps
/// When `envelope` names a key and the body is a JSON object carrying a non-null value under it,
/// the value is re-serialized as the body. Anything else passes through byte-for-byte, so this
/// helper MUST NOT drop a body it cannot unwrap. This implements the `response-envelope-unwrapped`
/// requirement declared on `Backend::response_envelope`.
fn unwrap_envelope(body: &[u8], envelope: Option<&str>) -> Vec<u8> {
    let Some(key) = envelope else {
        return body.to_vec();
    };
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    match value.get(key).filter(|inner| !inner.is_null()) {
        Some(inner) => serde_json::to_vec(inner).unwrap_or_else(|_| body.to_vec()),
        None => body.to_vec(),
    }
}

fn account_label(state: &AppState, account_id: &str) -> Option<String> {
    state
        .pool
        .lock()
        .unwrap()
        .account(account_id)
        .map(|a| a.label.clone())
}

fn report_outcome(state: &Arc<AppState>, account_id: &str, outcome: Outcome) {
    let status_change = {
        let mut pool = state.pool.lock().unwrap();
        pool.report(account_id, outcome)
    };
    if let Some(new_status) = status_change {
        let reset_at = {
            let pool = state.pool.lock().unwrap();
            pool.account(account_id).map(|a| a.reset_at).unwrap_or(0)
        };
        state.store.update_account_status(account_id, new_status, reset_at);
    }
}

#[allow(clippy::too_many_arguments)]
fn push_log(
    state: &Arc<AppState>,
    request_id: &str,
    model: &str,
    backend: Option<String>,
    account: Option<String>,
    label: Option<String>,
    sticky: &str,
    status: u16,
    duration_ms: u64,
) {
    let mut logs = state.logs.lock().unwrap();
    logs.push_back(RequestLogEntry {
        ts: now_ms(),
        request_id: request_id.to_string(),
        model: model.to_string(),
        backend,
        account,
        label,
        sticky: sticky.to_string(),
        status,
        duration_ms,
        stream_outcome: None,
        error_code: None,
    });
    while logs.len() > LOG_BUFFER {
        logs.pop_front();
    }
}

pub async fn models(State(state): State<Arc<AppState>>) -> Response {
    let mut merged: std::collections::BTreeMap<String, Value> = Default::default();
    for backend in BackendId::all() {
        for m in state.store.catalog(backend) {
            let entry = serde_json::json!({
                "id": m.id,
                "object": "model",
                "name": m.name,
                "context_window": m.context,
                "max_input_tokens": m.input,
                "max_output_tokens": m.output,
                "owned_by": format!("underclass-{}", backend.as_str()),
            });
            merged
                .entry(m.id)
                .and_modify(|existing| {
                    if m.context > existing["context_window"].as_i64().unwrap_or(0) {
                        *existing = entry.clone();
                    }
                })
                .or_insert(entry);
        }
    }
    let data: Vec<Value> = merged.into_values().collect();
    axum::Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sticky_key_from_body_cache_key() {
        let body = json!({"model": "gpt-5.5", "prompt_cache_key": "sess-1"});
        assert_eq!(extract_sticky_key(&body, None).as_deref(), Some("sess-1"));
    }

    #[test]
    fn sticky_key_camel_case_fallback() {
        let body = json!({"model": "gpt-5.5", "promptCacheKey": "sess-2"});
        assert_eq!(extract_sticky_key(&body, None).as_deref(), Some("sess-2"));
    }

    #[test]
    fn sticky_key_falls_back_to_session_header() {
        let body = json!({"model": "gpt-5.5"});
        assert_eq!(extract_sticky_key(&body, Some("sess-3")).as_deref(), Some("sess-3"));
    }

    #[test]
    fn sticky_key_none_when_absent() {
        let body = json!({"model": "gpt-5.5"});
        assert!(extract_sticky_key(&body, None).is_none());
        let empty = json!({"prompt_cache_key": ""});
        assert!(extract_sticky_key(&empty, None).is_none());
    }

    #[test]
    fn envelope_unwraps_data_key() {
        let body = br#"{"data":{"choices":[{"message":{"content":"hi"}}]},"success":true}"#;
        let unwrapped = unwrap_envelope(body, Some("data"));
        let value: Value = serde_json::from_slice(&unwrapped).unwrap();
        assert_eq!(value["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn envelope_passes_through_plain_and_unwrappable_bodies() {
        let plain = br#"{"choices":[]}"#;
        assert_eq!(unwrap_envelope(plain, Some("data")), plain);
        assert_eq!(unwrap_envelope(plain, None), plain);
        assert_eq!(unwrap_envelope(b"not json", Some("data")), b"not json");
        let null = br#"{"data":null}"#;
        assert_eq!(unwrap_envelope(null, Some("data")), null);
    }

    #[test]
    fn cline_declares_the_data_envelope() {
        let backend = crate::cline::ClineBackend { cooldown_ms: 1000 };
        use crate::provider::Backend;
        assert_eq!(backend.response_envelope(), Some("data"));
    }

        #[test]
    fn json_body_answered_to_a_streaming_request_is_not_parsed_as_sse() {
        // Cline can answer a `stream: true` request with a single JSON document. Consuming that as
        // SSE yields no frames, so a translated client gets a synthetic `stream_truncated` failure
        // and an enveloped one gets the wrapper forwarded verbatim.
        assert!(!upstream_is_sse(b"application/json"));
        assert!(!upstream_is_sse(b"application/json; charset=utf-8"));
        assert!(upstream_is_sse(b"text/event-stream"));
        assert!(upstream_is_sse(b"text/event-stream; charset=utf-8"));
    }

    #[test]
    fn envelope_and_translation_both_route_json_bodies_to_the_buffered_path() {
        // `needs_buffering` must cover the enveloped chat-completions case, not just responses
        // translation, or a chat client receives `{"data":…,"success":true}` unmodified.
        let buffered = |translate: bool, envelope: Option<&str>, ct: &[u8]| {
            let needs_buffering = translate || envelope.is_some();
            if needs_buffering { !upstream_is_sse(ct) } else { usage_is_stream(&json!({"stream": true}), ct) }
        };
        // A plain chat-completions backend keeps its upstream framing untouched.
        assert!(buffered(false, None, b"application/json"));
        // Enveloped or translated, a JSON answer is buffered even though the client asked to stream.
        assert!(buffered(false, Some("data"), b"application/json"));
        assert!(buffered(true, None, b"application/json"));
        assert!(buffered(true, Some("data"), b"application/json"));
        // A real event stream still streams in every case.
        assert!(!buffered(false, Some("data"), b"text/event-stream"));
        assert!(!buffered(true, None, b"text/event-stream"));
    }

    #[test]
    fn stream_request_overrides_incorrect_json_content_type() {
        let body = json!({"stream": true});
        assert!(usage_is_stream(&body, b"application/json"));
        assert!(usage_is_stream(&json!({}), b"text/event-stream; charset=utf-8"));
        assert!(!usage_is_stream(&json!({}), b"application/json"));
    }
}
