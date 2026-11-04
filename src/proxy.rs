use crate::logging;
use crate::models::{AccountStatus, BackendId, Outcome, RequestLogEntry, Surface, now_ms};
use crate::pool::{Decision, PoolCore, SelectError, Selection};
use crate::provider::BackendMap;
use crate::store::Store;
use crate::store::{UpstreamEvent, UsageRecord};
use crate::tokens::TokenManager;
use crate::usage::{
    StreamEnd, StreamOutcome, UPSTREAM_STREAM_ERROR, UsageDialect, UsageTap, usage_from_dialect,
};
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
/// How long a streamed response is held back waiting for its first output or terminal event.
/// Upstream capacity failures before output observed in practice arrive within 60s (265 of 266).
pub(crate) const PRE_OUTPUT_HOLD: std::time::Duration = std::time::Duration::from_secs(60);
/// Upper bound on bytes buffered while holding a stream before its first output.
pub(crate) const PRE_OUTPUT_HOLD_BYTES: usize = 1024 * 1024;

/// The bounds each attempt is subject to. None of them bounds how long a *response* may take:
/// `pre_output_hold` only decides how long the first output is held back for failover and expiry
/// just starts forwarding, `header_timeout` covers only the wait for the response head, and
/// `idle_timeout` covers only the gap between body chunks. A long but active response is never cut.
#[derive(Clone, Debug)]
pub struct StreamTuning {
    /// How long to hold a stream before its first output while failover is still possible.
    pub pre_output_hold: std::time::Duration,
    /// How many bytes may be buffered while holding a stream before its first output.
    pub pre_output_hold_bytes: usize,
    /// How long an established stream may produce nothing at all before it is ended with a
    /// terminal error event. `None` never ends a stream for being quiet. The timer wraps one
    /// `next()` call and resets on every chunk, so it bounds the silence *between chunks*, not the
    /// body's total time.
    pub idle_timeout: Option<std::time::Duration>,
    /// How long one attempt may wait for upstream response headers. It covers only the head:
    /// wrapping the send this way leaves the body untouched, which a whole-request timeout would
    /// not. `None` disables the bound.
    pub header_timeout: Option<std::time::Duration>,
}

impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            pre_output_hold: PRE_OUTPUT_HOLD,
            pre_output_hold_bytes: PRE_OUTPUT_HOLD_BYTES,
            idle_timeout: Some(std::time::Duration::from_secs(
                crate::config::DEFAULT_STREAM_IDLE_TIMEOUT_SECS,
            )),
            header_timeout: Some(std::time::Duration::from_secs(
                crate::config::DEFAULT_UPSTREAM_RESPONSE_HEADER_TIMEOUT_SECS,
            )),
        }
    }
}

/// Client headers that never travel upstream: credentials and transport-level headers. The
/// proxy injects the selected account's credential itself (CONTRACTS
/// `outbound-credential-stripping`), and the transport headers belong to the client's hop.
const STRIPPED_CLIENT_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "cookie",
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "accept-encoding",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-request-id",
];

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
    /// Per-account observations a backend reported about its last upstream response, keyed by
    /// account id. Backends decide what to report; the proxy only stores it.
    pub observations: Mutex<std::collections::HashMap<String, Value>>,
    /// How long usage rows, upstream events, and daily rollups are kept.
    pub stats_retention_days: i64,
    /// The outbound user agent for the fork's own backends (Cline and OpenCode Go), used for the
    /// catalog, identity, and onboarding calls those modules make themselves. `None` leaves each
    /// backend its built-in default. Inference requests resolve their own value from the client
    /// request and the backend's configured override.
    pub cline_user_agent: Option<String>,
    pub opencode_go_user_agent: Option<String>,
    pub vercel_user_agent: Option<String>,
    /// The bounds a streamed response is subject to. Defaults are unbounded for wall-clock time.
    pub stream_tuning: StreamTuning,
}

fn terminal_stream_error(dialect: UsageDialect, backend: &str, reason: &str) -> Vec<u8> {
    let message = format!("upstream {backend} stream ended early: {reason}");
    let body = match dialect {
        UsageDialect::Chat => serde_json::json!({
            "error": {
                "code": UPSTREAM_STREAM_ERROR,
                "type": "server_error",
                "message": message,
            }
        }),
        UsageDialect::Responses => serde_json::json!({
            "type": "error",
            "code": UPSTREAM_STREAM_ERROR,
            "message": message,
        }),
        UsageDialect::Anthropic => serde_json::json!({
            "type": "error",
            "error": {"type": UPSTREAM_STREAM_ERROR, "message": message},
        }),
    };
    let mut out = Vec::new();
    if dialect == UsageDialect::Responses {
        out.extend_from_slice(b"event: error\n");
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(&serde_json::to_vec(&body).unwrap_or_default());
    out.extend_from_slice(b"\n\n");
    out
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
    /// Where an upstream event surfaced: `stream`, or `stream_failover` when the attempt was
    /// discarded and retried on another account.
    detail: &'static str,
    /// The code to record when the upstream sent no terminal event of its own but the proxy ended
    /// the body with one. It is the same code the client was told, so a recorded failure can be
    /// traced to what the client saw.
    synthesized_code: Option<&'static str>,
}

impl Drop for UsageFinish {
    fn drop(&mut self) {
        if let Some(counts) = self.tap.counts() {
            self.record.input_tokens = Some(counts.input_tokens);
            self.record.output_tokens = Some(counts.output_tokens);
        }
        let mut report = self.tap.report(self.ended.unwrap_or(StreamEnd::Dropped));
        if report.error_code.is_none() {
            report.error_code = self.synthesized_code.map(str::to_string);
        }
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
        if let Some(entry) = self.logs.lock().unwrap().iter_mut().rev().find(|entry| {
            entry.request_id == self.record.request_id
                && entry.account.as_deref() == Some(self.record.account_id.as_str())
        }) {
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
            self.store.insert_upstream_event(&event_for(
                &self.record,
                kind,
                self.detail,
                report.error_code.clone(),
            ));
        }
        // Written last: once the usage row is visible, the event and annotation already exist.
        persist_usage(&self.store, &self.record);
    }
}

fn event_for(
    record: &UsageRecord,
    kind: &str,
    detail: &str,
    error_code: Option<String>,
) -> UpstreamEvent {
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
fn usage_record(
    request_id: &str,
    path: &str,
    model: &str,
    sticky: Option<&str>,
    selection: &Selection,
    account: &crate::models::Account,
    status: u16,
    started_ms: i64,
) -> UsageRecord {
    UsageRecord {
        id: 0,
        request_id: request_id.to_string(),
        ts: started_ms,
        endpoint: path.to_string(),
        backend: selection.backend.as_str().to_string(),
        model: model.to_string(),
        account_id: selection.account_id.clone(),
        account_label: account.label.clone(),
        cache_key: sticky.map(str::to_string),
        status,
        input_tokens: None,
        output_tokens: None,
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
/// `Authorization` header carries exactly `Bearer <proxy_key>`, or the `x-api-key` header
/// carries exactly `<proxy_key>`. When no key is configured (localhost-only mode) requests pass.
pub async fn require_proxy_key(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.proxy_key {
        // Either header may carry the key. Testing them independently matters for the Claude Code
        // client, which sends its own `Authorization` when the operator supplied the key through
        // `ANTHROPIC_API_KEY` (`x-api-key`); preferring `Authorization` would reject that request.
        let bearer = req
            .headers()
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let api_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());
        let authorized = [bearer, api_key]
            .into_iter()
            .flatten()
            .any(|key| key == expected.as_str());
        match authorized {
            true => {}
            false => {
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
        [(http::header::CONTENT_TYPE, content_type.to_string())],
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

/// @cc [owner:ghuntley,label:proxy] header-timeout-tells-the-client
/// When no account produced an upstream response and at least one attempt hit the response-header
/// bound, the request MUST answer `504` carrying the stable code `upstream_response_timeout` in the
/// existing error envelope, not a generic `pool_exhausted` failure, so the client's error and the
/// usage row it joins on name the same condition.
fn header_timeout_response() -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        axum::Json(serde_json::json!({
            "error": {
                "message": "upstream sent no response headers within the configured bound; retry the request",
                "type": crate::usage::UPSTREAM_RESPONSE_TIMEOUT,
            }
        })),
    )
        .into_response()
}

fn attach_request_id(mut resp: Response, request_id: &str) -> Response {
    if let Ok(v) = http::HeaderValue::from_str(request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

/// Where one attempt was in its life when it stopped, as far as this guard can see. Everything
/// after the upstream's response head is covered by `UsageFinish` and the per-body logging, so the
/// guard deliberately stops watching there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptStage {
    /// Waiting for the account's access token (a refresh can block on the issuer). The upstream was
    /// never contacted, so a request that ends here never reached it at all.
    Token,
    /// Waiting for the upstream's response headers.
    AwaitingHeaders,
}

impl AttemptStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Token => "token",
            Self::AwaitingHeaders => "awaiting_headers",
        }
    }
}

/// How an attempt stopped being watched. Every variant except none means the guard must stay
/// silent, because something else already recorded the end of this attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttemptEnd {
    /// The upstream's response head arrived: `request.completed`, `upstream.network_error`,
    /// `usage.recorded`, and `stream.finished` cover what happens next.
    Covered,
    /// The header bound expired; `upstream.headers_timeout` was logged explicitly.
    HeaderTimeout,
    /// The attempt failed for a reason it logged itself (`token.*`, `upstream.network_error`).
    Failed,
}

/// What a guard reports when an attempt stops without ever getting upstream response headers. It is
/// a plain value so the decision can be tested apart from tracing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AttemptAbandon {
    stage: &'static str,
    reason: &'static str,
    elapsed_ms: u64,
}

/// @cc [owner:ghuntley,label:logging] attempt-abandonment-visible
/// An attempt that starts waiting for upstream response headers MUST log `request.awaiting_headers`
/// first, and MUST emit exactly one `request.abandoned` line whenever it ends without that head
/// arriving - whether the attempt returned, errored, or its future was dropped by a client abort,
/// cancellation, or shutdown. The line MUST carry `stage`, `reason`, `attempt`, `cache_key`, and
/// `elapsed_ms`.
///
/// It MUST NOT emit that line when the upstream's response head arrived (existing logging covers
/// the attempt), when the attempt failed for a reason it already logged, or when the header bound
/// expired (that path logs `upstream.headers_timeout` instead). It MUST never log request bodies,
/// prompts, tokens, or credentials.
struct AttemptWatch {
    request_id: String,
    attempt: u32,
    cache_key: String,
    model: String,
    upstream_model: String,
    endpoint: String,
    backend: String,
    account_id: String,
    label: Option<String>,
    started: Instant,
    stage: AttemptStage,
    ended: Option<AttemptEnd>,
}

impl AttemptWatch {
    #[allow(clippy::too_many_arguments)]
    fn new(
        req: &AttemptRequest<'_>,
        attempt: u32,
        upstream_model: &str,
        selection: &Selection,
        label: Option<&str>,
    ) -> Self {
        Self {
            request_id: req.request_id.to_string(),
            attempt,
            cache_key: req.sticky.unwrap_or("none").to_string(),
            model: req.model.to_string(),
            upstream_model: upstream_model.to_string(),
            endpoint: req.endpoint.to_string(),
            backend: selection.backend.as_str().to_string(),
            account_id: selection.account_id.clone(),
            label: label.map(str::to_string),
            started: Instant::now(),
            stage: AttemptStage::Token,
            ended: None,
        }
    }

    /// The start mark, and the transition to awaiting the upstream's response head.
    fn awaiting_headers(&mut self) {
        self.stage = AttemptStage::AwaitingHeaders;
        logging::log_request_awaiting_headers(
            &self.request_id,
            self.attempt,
            &self.cache_key,
            &self.model,
            &self.upstream_model,
            &self.endpoint,
            &self.backend,
            &self.account_id,
            self.label.as_deref(),
        );
    }

    /// Stops watching: every end this crate knows about is either covered by other logging or
    /// logged explicitly, so this is the only way to avoid a duplicate line.
    fn finish(&mut self, end: AttemptEnd) {
        self.ended = Some(end);
    }

    /// The terminal line this guard would emit, or `None` when it must stay silent.
    ///
    /// Reaching here with no recorded end means the attempt's future stopped being polled without
    /// this crate observing any outcome: the client went away, the request was cancelled, or the
    /// server shut down. `stage` then separates the two cases an operator cares about - before the
    /// send nothing upstream was contacted, and while awaiting headers the upstream had sent
    /// nothing back - which is why the two reasons are distinguishable at all.
    fn abandonment(&self) -> Option<AttemptAbandon> {
        if self.ended.is_some() {
            return None;
        }
        let (stage, reason) = match self.stage {
            AttemptStage::Token => (self.stage.as_str(), "client_aborted"),
            AttemptStage::AwaitingHeaders => (self.stage.as_str(), "upstream_silent"),
        };
        Some(AttemptAbandon {
            stage,
            reason,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        })
    }
}

impl Drop for AttemptWatch {
    fn drop(&mut self) {
        let Some(abandon) = self.abandonment() else {
            return;
        };
        logging::log_request_abandoned(
            &self.request_id,
            self.attempt,
            &self.cache_key,
            &self.model,
            &self.upstream_model,
            &self.endpoint,
            &self.backend,
            &self.account_id,
            self.label.as_deref(),
            abandon.stage,
            abandon.reason,
            abandon.elapsed_ms,
        );
    }
}

/// How one bounded wait ended. `TimedOut` is deliberately distinct from `Transport`, because the
/// two are different conditions for an operator and for accounting.
enum Bounded<T, E> {
    Value(T),
    Transport(E),
    TimedOut,
}

/// @cc [owner:ghuntley,label:proxy] response-header-bound-covers-the-head-only
/// `send_bounded` MUST apply `limit` around the future that resolves at the upstream's response
/// head and MUST NOT bound the response body, so a slow body after prompt headers is never cut.
/// `None` MUST await the send with no bound at all. Expiry MUST be reported as `TimedOut` rather
/// than as a transport error, so the caller can log it as its own condition instead of a failed
/// connection.
async fn send_bounded<F, T, E>(send: F, limit: Option<std::time::Duration>) -> Bounded<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let Some(limit) = limit else {
        return match send.await {
            Ok(value) => Bounded::Value(value),
            Err(error) => Bounded::Transport(error),
        };
    };
    match tokio::time::timeout(limit, send).await {
        Ok(Ok(value)) => Bounded::Value(value),
        Ok(Err(error)) => Bounded::Transport(error),
        Err(_) => Bounded::TimedOut,
    }
}

enum Attempt {
    Respond(Response),
    Failover(Option<Response>),
    /// The stream failed with a capacity code before any output; `replay` holds what upstream
    /// sent, for the client if no other account succeeds.
    Overloaded {
        account_id: String,
        replay: Response,
    },
    /// This account cannot serve the request and has no upstream response worth keeping (a
    /// transient credential failure); skip it for the rest of the request rather than re-selecting
    /// it, which a sticky binding would otherwise guarantee.
    Excluded(String),
    /// This attempt waited for upstream response headers past the configured bound. Distinguished
    /// from a plain failover so a request no account can serve is told *why* it failed.
    HeaderTimedOut,
}

/// @cc [owner:ghuntley,label:proxy] saturation-fail-fast-429
/// When selection returns `Saturated`, `infer` MUST first make one automatic Codex reset attempt
/// for a live Codex-pool outage, then select again. If no account recovers, it MUST respond with
/// 429 and `Retry-After` equal to the seconds until `until_ms`. Exception: once an account has
/// been excluded for an overload before output, a failed selection MUST return that held upstream
/// response instead, without a reset attempt.
pub async fn infer(State(state): State<Arc<AppState>>, req: Request) -> Response {
    run(state, req, Surface::OpenAI).await
}

/// The Anthropic Messages API surface. Only official Claude Code clients are admitted.
pub async fn messages(State(state): State<Arc<AppState>>, req: Request) -> Response {
    run(state, req, Surface::Anthropic).await
}

/// @cc [owner:ghuntley,label:proxy] surface-client-admission
/// A request MUST be rejected with 403 before any pooled credential is used when no backend
/// serving the requested surface admits it as a client.
fn surface_accepts_client(
    state: &AppState,
    surface: Surface,
    headers: &HeaderMap,
    body: &Value,
) -> bool {
    let serving: Vec<_> = state
        .backends
        .values()
        .filter(|backend| backend.surfaces().contains(&surface))
        .collect();
    if serving.is_empty() {
        return true;
    }
    serving
        .iter()
        .any(|backend| backend.accepts_client(headers, body))
}

async fn run(state: Arc<AppState>, req: Request, surface: Surface) -> Response {
    let started = Instant::now();
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .and_then(|r| r.header_value().to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let path = req.uri().path().to_string();
    // The routes are nested under `/v1`, so the router hands the handler a path without that
    // prefix. The upstream URL is built from it; the usage record keeps the client's route.
    let endpoint = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|uri| uri.path().to_string())
        .unwrap_or_else(|| path.clone());
    // The Anthropic API is called with the client's own query string (`?beta=true`).
    let target = match surface {
        Surface::Anthropic => req
            .uri()
            .path_and_query()
            .map(|value| value.as_str().to_string())
            .unwrap_or_else(|| path.clone()),
        Surface::OpenAI => path.clone(),
    };
    let client_headers = req.headers().clone();
    let session_header = client_headers
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

    if !surface_accepts_client(&state, surface, &client_headers, &body) {
        return attach_request_id(
            error_response(
                StatusCode::FORBIDDEN,
                "this endpoint only accepts official Claude Code clients",
                "client_not_allowed",
            ),
            &request_id,
        );
    }

    // The Anthropic catalog is lowercase and clients vary the case, so that surface keeps its
    // case-insensitive canonical form. Every other surface matches the id against an
    // operator-editable catalog verbatim.
    let raw_model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let requested_model = match surface {
        Surface::Anthropic => crate::claude::canonical_model(raw_model),
        Surface::OpenAI => {
            crate::opencode_go::normalize_model_id(&crate::models::normalize_model_name(raw_model))
        }
    };
    // Exact concrete match takes precedence over an alias: if the requested id exists in any
    // backend catalog, treat it as a concrete model, not an alias.
    // `model` is the value passed to selection (alias or concrete). Exact concrete matches
    // take precedence over aliases, handled inside PoolCore::is_alias.
    let model = requested_model.clone();
    let sticky = match surface {
        Surface::Anthropic => crate::claude::sticky_key(&client_headers, &body),
        Surface::OpenAI => extract_sticky_key(&body, session_header.as_deref()),
    };

    // A banked ChatGPT reset is spent from the operator's own subscription, so it is only ever
    // redeemed for a request that could actually be served by a Codex account. An Anthropic
    // request never is, and spending a credit on its behalf would be a silent cost.
    let codex_resets = surface == Surface::OpenAI;
    if codex_resets {
        state
            .resets
            .maybe_reset(&state.pool, &state.store, &state.tokens, &request_id)
            .await;
    }

    // Only accounts that can actually serve this request's surface are selectable, so the budget
    // must count those: a pool with one Codex and ten Claude accounts would otherwise retry the
    // single Codex account eleven times on an OpenAI request.
    let total_accounts = {
        let pool = state.pool.lock().unwrap();
        pool.accounts
            .values()
            .filter(|s| pool.serves_surface(s.account.backend, surface))
            .count()
            .max(1)
    };
    let mut last_upstream: Option<Response> = None;
    // Whether any attempt hit the response-header bound. Only used to explain a request no account
    // could serve: the client gets `upstream_response_timeout` rather than a generic failure.
    let mut header_timed_out = false;
    let mut refreshed: std::collections::HashSet<String> = Default::default();
    let mut reset_after_quota = false;
    let mut attempts = 0;
    let mut max_attempts = total_accounts;
    // Accounts whose stream failed with a capacity code before output, skipped for this request.
    let mut overloaded: std::collections::HashSet<String> = Default::default();
    // Accounts skipped because a token refresh was merely deferred. Kept apart from `overloaded`
    // because such an account is still healthy: a rebind it causes must not outlive the request.
    // Shared with `AffinityGuard`, which is built before the loop runs and must observe the
    // exclusions recorded inside it.
    let deferred: Arc<std::sync::Mutex<std::collections::HashSet<String>>> = Default::default();

    // The session's binding before any rebind this request causes, so a deferral-induced rebind
    // can be undone instead of quietly moving the session off its account.
    let original_binding = sticky.as_deref().and_then(|key| {
        state
            .pool
            .lock()
            .unwrap()
            .binding(key)
            .map(|(account_id, bound_at)| (key.to_string(), account_id, bound_at))
    });

    // Restores the session's affinity on every exit from this request, however it ends.
    let _affinity = AffinityGuard {
        state: state.clone(),
        original: original_binding.clone(),
        deferred: deferred.clone(),
    };

    let req = AttemptRequest {
        request_id: &request_id,
        endpoint: &endpoint,
        target: &target,
        model: &model,
        sticky: sticky.as_deref(),
        body_bytes: &body_bytes,
        client_headers: &client_headers,
    };

    while attempts < max_attempts {
        attempts += 1;
        let mut selection = state.pool.lock().unwrap().select_excluding(
            now_ms(),
            sticky.as_deref(),
            &model,
            surface,
            &overloaded,
        );

        if selection.is_err()
            && !overloaded.is_empty()
            && let Some(replay) = last_upstream.take()
        {
            logging::log_request_completed(
                &request_id,
                replay.status().as_u16(),
                started.elapsed().as_millis() as u64,
            );
            return attach_request_id(replay, &request_id);
        }
        if codex_resets
            && matches!(selection, Err(SelectError::Saturated { .. }))
            && !reset_after_quota
        {
            reset_after_quota = true;
            // Always select again: the reset attempt can wait on the shared reset lock and on
            // usage fetches, during which cooling deadlines may pass even if nothing was redeemed.
            state
                .resets
                .maybe_reset(&state.pool, &state.store, &state.tokens, &request_id)
                .await;
            selection = state.pool.lock().unwrap().select_excluding(
                now_ms(),
                sticky.as_deref(),
                &model,
                surface,
                &overloaded,
            );
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

        if (selection.decision == Decision::NewBinding
            || selection.decision == Decision::StickyRebind)
            && let Some(key) = &sticky
            && let Some(binding) = state
                .pool
                .lock()
                .unwrap()
                .bindings()
                .into_iter()
                .find(|b| b.cache_key == *key)
        {
            state.store.upsert_binding(&binding);
            state.store.prune_bindings(
                now_ms(),
                crate::pool::BINDING_TTL_MS,
                crate::pool::DEFAULT_BINDING_CAP,
            );
        }

        let upstream_model = selection.concrete_model.clone();
        match attempt_account(
            &state,
            &req,
            // `attempts` was incremented at the top of this iteration, so this is 1-based.
            attempts as u32,
            &upstream_model,
            &selection,
            &mut refreshed,
        )
        .await
        {
            Attempt::Respond(resp) => {
                logging::log_request_completed(
                    &request_id,
                    resp.status().as_u16(),
                    started.elapsed().as_millis() as u64,
                );
                return resp;
            }
            Attempt::Overloaded { account_id, replay } => {
                overloaded.insert(account_id);
                last_upstream = Some(replay);
            }
            Attempt::Excluded(account_id) => {
                deferred.lock().unwrap().insert(account_id.clone());
                overloaded.insert(account_id);
            }
            Attempt::HeaderTimedOut => {
                header_timed_out = true;
            }
            Attempt::Failover(passthrough) => {
                last_upstream = passthrough.or(last_upstream);
                if codex_resets && attempts == max_attempts && !reset_after_quota {
                    reset_after_quota = true;
                    if state
                        .resets
                        .maybe_reset(&state.pool, &state.store, &state.tokens, &request_id)
                        .await
                    {
                        max_attempts += 1;
                    }
                }
            }
        }
    }

    let saturated = state
        .pool
        .lock()
        .unwrap()
        .select(now_ms(), sticky.as_deref(), &model, surface);
    if let Err(SelectError::Saturated { until_ms }) = saturated {
        record_saturated(&state, &request_id, &model, until_ms);
        return attach_request_id(saturated_response(until_ms), &request_id);
    }

    let response = last_upstream.take().unwrap_or_else(|| {
        if header_timed_out {
            header_timeout_response()
        } else {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "pool exhausted: every subscription attempt failed",
                "pool_exhausted",
            )
        }
    });
    logging::log_request_completed(
        &request_id,
        response.status().as_u16(),
        started.elapsed().as_millis() as u64,
    );
    attach_request_id(response, &request_id)
}

/// @cc [owner:ghuntley,label:proxy] affinity-restored-after-deferral
/// When selection skipped the account bound to a sticky key because a token refresh was merely
/// deferred, the resulting rebind MUST be undone when the request ends, however it ends: the
/// account was never unhealthy, and leaving the session elsewhere would lose its prompt-cache
/// affinity long after the transient condition cleared. An exclusion caused by anything else MUST
/// be left in place.
struct AffinityGuard {
    state: Arc<AppState>,
    original: Option<(String, String, i64)>,
    deferred: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Drop for AffinityGuard {
    fn drop(&mut self) {
        let Some((key, account_id, bound_at)) = &self.original else {
            return;
        };
        // Only a deferral on the account that held the session justifies moving it back; an
        // overload on that account is a real reason to have rebound.
        if !self.deferred.lock().unwrap().contains(account_id) {
            return;
        }
        let mut pool = self.state.pool.lock().unwrap();
        if pool.binding(key).map(|(id, _)| id).as_deref() == Some(account_id.as_str()) {
            return;
        }
        let Some(backend) = pool.account(account_id).map(|a| a.backend) else {
            return;
        };
        pool.restore_binding(key, account_id, *bound_at);
        self.state.store.upsert_binding(&crate::models::Binding {
            cache_key: key.clone(),
            account_id: account_id.clone(),
            backend,
            bound_at: *bound_at,
        });
    }
}

/// The parts of one proxy request that stay fixed across every attempt. Bundled so an attempt's
/// signature names only what varies per attempt (`selection` and its `upstream_model`).
struct AttemptRequest<'a> {
    request_id: &'a str,
    endpoint: &'a str,
    target: &'a str,
    model: &'a str,
    sticky: Option<&'a str>,
    body_bytes: &'a axum::body::Bytes,
    client_headers: &'a HeaderMap,
}

/// @cc [owner:ghuntley,label:proxy] failover-bounded-by-accounts
/// A request MUST make at most as many attempts as configured accounts, plus one additional attempt
/// after a successful banked reset, and the first successful upstream response MUST be returned
/// to the client; a 2xx stream that fails with a capacity code before any output is not successful:
/// the account MUST be excluded for the rest of the request (without changing its health) and
/// the next account tried. Upstream error bodies MUST be preserved and
/// returned when every attempt fails (or a 503 `pool_exhausted` error when no upstream response
/// was ever received). An upstream 401 or 403 MUST trigger exactly one forced token refresh and one
/// same-account retry per request; a second 401 or 403 (or a failed refresh) MUST classify the
/// account `AuthFailed` and fail over.
/// `attempt` is this attempt's 1-based position in the request's failover chain, and is what the
/// observability lines carry so N selections can be read as one logical call.
async fn attempt_account(
    state: &Arc<AppState>,
    req: &AttemptRequest<'_>,
    attempt: u32,
    upstream_model: &str,
    selection: &Selection,
    refreshed: &mut std::collections::HashSet<String>,
) -> Attempt {
    let AttemptRequest {
        request_id,
        endpoint,
        target,
        model,
        sticky,
        body_bytes,
        client_headers,
    } = *req;
    let started = Instant::now();
    let started_ms = now_ms();
    let Some(backend) = state.backends.get(&selection.backend).cloned() else {
        return Attempt::Failover(None);
    };
    let account = state
        .pool
        .lock()
        .unwrap()
        .account(&selection.account_id)
        .cloned();
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
    // Watches this attempt until the upstream's response head arrives. Dropping it while it is
    // still waiting is the only record a request that never gets headers would otherwise leave.
    let mut watch = AttemptWatch::new(req, attempt, upstream_model, selection, label);

    let token = match state.tokens.access_token(&account).await {
        Ok(token) => token,
        // A refresh the issuer merely deferred is transient: cooling the account instead of
        // marking it auth_error keeps a valid credential in rotation.
        Err(e @ crate::tokens::TokenError::Transient(_)) => {
            tracing::warn!(request_id = %request_id, error = %e, "token.transient");
            report_outcome(state, &selection.account_id, Outcome::Transient);
            watch.finish(AttemptEnd::Failed);
            drop(guard);
            // Exclude the account for the rest of the request. A sticky binding still points at
            // it, so without this the next iteration would re-select it and re-post the exact
            // refresh the issuer just deferred - and the token endpoint rate-limits independently.
            return Attempt::Excluded(selection.account_id.clone());
        }
        Err(e) => {
            tracing::warn!(request_id = %request_id, error = %e, "token.unavailable");
            report_outcome(state, &selection.account_id, Outcome::AuthFailed);
            logging::log_account_state(
                &selection.account_id,
                label,
                "auth_error",
                "missing or unusable credential",
            );
            watch.finish(AttemptEnd::Failed);
            drop(guard);
            return Attempt::Failover(None);
        }
    };

    let url = backend.rewrite_url(target, upstream_model, &account);
    let mut headers = HeaderMap::new();
    if backend.forward_client_headers() {
        for (name, value) in client_headers.iter() {
            if STRIPPED_CLIENT_HEADERS.contains(&name.as_str()) {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
    }
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    if !headers.contains_key(http::header::ACCEPT) {
        headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("text/event-stream, application/json"),
        );
    }
    let mut outbound_body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
    // For aliases, the client sent the alias id; rewrite the body's model to the concrete id so
    // backends that read the body model (prepare_body, auto_stream_usage) see the concrete id.
    if let Some(obj) = outbound_body.as_object_mut()
        && obj.get("model").and_then(Value::as_str).is_some()
    {
        obj.insert("model".into(), Value::String(upstream_model.to_string()));
    }
    let responses_path = endpoint.ends_with("/responses");
    let translate_responses = responses_path && backend.responses_via_chat(upstream_model);
    // The dialect the *upstream* speaks: a translated request is Chat Completions even though the
    // client's path is `/v1/responses`, so this must not be derived from the client path alone.
    // Used by every body parse - error bodies as much as successful ones.
    let upstream_dialect = if translate_responses {
        crate::usage::UsageDialect::Chat
    } else {
        backend.usage_dialect(endpoint)
    };
    if translate_responses {
        outbound_body = crate::responses::request_to_chat(&outbound_body);
    }
    backend.inject_headers(
        &account,
        &token,
        sticky,
        &outbound_body,
        client_headers,
        &mut headers,
    );
    let mut fallback_body = outbound_body.clone();
    let auto_usage = !state
        .stream_usage_unsupported
        .lock()
        .unwrap()
        .contains(&selection.backend)
        && backend.auto_stream_usage(endpoint, &mut outbound_body);
    let outbound_bytes = if backend.forward_body_verbatim() {
        // The upstream validates the client's own body, so it travels byte for byte — except when
        // an alias rewrote the model, in which case the body must carry the concrete id.
        if model != upstream_model {
            let mut v: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("model".into(), Value::String(upstream_model.to_string()));
            }
            serde_json::to_vec(&v).unwrap_or_else(|_| body_bytes.to_vec())
        } else {
            body_bytes.to_vec()
        }
    } else {
        backend.prepare_body(&mut outbound_body);
        serde_json::to_vec(&outbound_body).unwrap_or_else(|_| body_bytes.to_vec())
    };
    if !backend.forward_body_verbatim() {
        // The retry after a rejected `stream_options` must be the *prepared* body or upstream
        // refuses it again (Cline, for one, would still see its `cline/` prefix), but it must also
        // drop the option that was just rejected. Prepare the fallback the same way, unsupported.
        let mut fallback = fallback_body.clone();
        backend.prepare_body(&mut fallback);
        fallback_body = fallback;
    }

    // The start mark, and the only place a request that never gets headers begins its clock.
    watch.awaiting_headers();
    let mut response = match send_bounded(
        state
            .client
            .post(&url)
            .headers(headers.clone())
            .body(outbound_bytes)
            .send(),
        state.stream_tuning.header_timeout,
    )
    .await
    {
        Bounded::Value(resp) => resp,
        Bounded::Transport(e) => {
            tracing::warn!(request_id = %request_id, error = %e, backend = %selection.backend.as_str(), "upstream.network_error");
            let record = usage_record(
                request_id, endpoint, model, sticky, selection, &account, 0, started_ms,
            );
            persist_usage(&state.store, &record);
            report_outcome(state, &selection.account_id, Outcome::Transient);
            watch.finish(AttemptEnd::Failed);
            drop(guard);
            return Attempt::Failover(None);
        }
        Bounded::TimedOut => {
            // The configured bound expired: a deliberate decision by the proxy, logged under its
            // own event so it is never confused with a client that walked away.
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let limit_ms = state
                .stream_tuning
                .header_timeout
                .map(|limit| limit.as_millis() as u64)
                .unwrap_or(0);
            logging::log_upstream_headers_timeout(
                request_id,
                attempt,
                sticky.unwrap_or("none"),
                selection.backend.as_str(),
                &selection.account_id,
                label,
                endpoint,
                watch.stage.as_str(),
                elapsed_ms,
                limit_ms,
            );
            let mut record = usage_record(
                request_id, endpoint, model, sticky, selection, &account, 0, started_ms,
            );
            record.error_code = Some(crate::usage::UPSTREAM_RESPONSE_TIMEOUT.to_string());
            persist_usage(&state.store, &record);
            report_outcome(state, &selection.account_id, Outcome::Transient);
            watch.finish(AttemptEnd::HeaderTimeout);
            drop(guard);
            // No upstream response was ever received, so there is nothing to pass through; the
            // request fails over like any other attempt that produced no response.
            return Attempt::HeaderTimedOut;
        }
    };
    // The upstream's response head arrived. From here `UsageFinish` and the per-attempt logging
    // own this attempt's record, so the guard must not add an abandonment line.
    watch.finish(AttemptEnd::Covered);

    if auto_usage && matches!(response.status().as_u16(), 400 | 422) {
        let status = response.status();
        let rejection = response.text().await.unwrap_or_default();
        if rejection.contains("stream_options") || rejection.contains("include_usage") {
            let rejected = usage_record(
                request_id,
                endpoint,
                model,
                sticky,
                selection,
                &account,
                status.as_u16(),
                started_ms,
            );
            persist_usage(&state.store, &rejected);
            state
                .stream_usage_unsupported
                .lock()
                .unwrap()
                .insert(selection.backend);
            tracing::warn!(request_id = %request_id, backend = %selection.backend.as_str(), "usage.stream_option_unsupported");
            let retry_bytes =
                serde_json::to_vec(&fallback_body).unwrap_or_else(|_| body_bytes.to_vec());
            response = match send_bounded(
                state
                    .client
                    .post(&url)
                    .headers(headers)
                    .body(retry_bytes)
                    .send(),
                state.stream_tuning.header_timeout,
            )
            .await
            {
                Bounded::Value(resp) => resp,
                Bounded::Transport(error) => {
                    tracing::warn!(request_id = %request_id, error = %error, "upstream.network_error");
                    let record = usage_record(
                        request_id, endpoint, model, sticky, selection, &account, 0, started_ms,
                    );
                    persist_usage(&state.store, &record);
                    watch.finish(AttemptEnd::Failed);
                    drop(guard);
                    return Attempt::Failover(None);
                }
                Bounded::TimedOut => {
                    // The retry's own head is bounded the same way; the first response's head had
                    // already arrived, so this is still waiting on the upstream rather than
                    // anything the proxy decided.
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let limit_ms = state
                        .stream_tuning
                        .header_timeout
                        .map(|limit| limit.as_millis() as u64)
                        .unwrap_or(0);
                    logging::log_upstream_headers_timeout(
                        request_id,
                        attempt,
                        sticky.unwrap_or("none"),
                        selection.backend.as_str(),
                        &selection.account_id,
                        label,
                        endpoint,
                        watch.stage.as_str(),
                        elapsed_ms,
                        limit_ms,
                    );
                    let mut record = usage_record(
                        request_id, endpoint, model, sticky, selection, &account, 0, started_ms,
                    );
                    record.error_code = Some(crate::usage::UPSTREAM_RESPONSE_TIMEOUT.to_string());
                    persist_usage(&state.store, &record);
                    report_outcome(state, &selection.account_id, Outcome::Transient);
                    watch.finish(AttemptEnd::HeaderTimeout);
                    drop(guard);
                    return Attempt::HeaderTimedOut;
                }
            };
        } else {
            let record = usage_record(
                request_id,
                endpoint,
                model,
                sticky,
                selection,
                &account,
                status.as_u16(),
                started_ms,
            );
            persist_usage(&state.store, &record);
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
            drop(guard);
            return Attempt::Failover(Some(upstream_error_response(
                status,
                "application/json",
                rejection,
            )));
        }
    }

    let response_headers = response.headers().clone();
    let status = response.status();
    observe_response(
        state,
        &*backend,
        &account,
        &selection.account_id,
        &response_headers,
    );

    // Whether a status means the credential may be stale is the backend's decision: Copilot redeems
    // a 403 by refreshing, Claude's 403 is a per-resource refusal, and a backend on a static API
    // key cannot be redeemed at all. Asking the backend keeps that difference out of the proxy.
    let refresh_on = |status: StatusCode| backend.refresh_on(status.as_u16());
    if refresh_on(status) && refreshed.insert(selection.account_id.clone()) {
        let body_text = response.text().await.unwrap_or_default();
        let mut record = usage_record(
            request_id,
            endpoint,
            model,
            sticky,
            selection,
            &account,
            status.as_u16(),
            started_ms,
        );
        if let Ok(value) = serde_json::from_str::<Value>(&body_text)
            && let Some(counts) = usage_from_dialect(&value, upstream_dialect)
        {
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
            // A refresh the issuer merely deferred says nothing about the credential: marking the
            // account auth_error would strand a valid subscription until an operator re-logged in.
            Err(e @ crate::tokens::TokenError::Transient(_)) => {
                tracing::warn!(request_id = %request_id, error = %e, "token.transient");
                report_outcome(state, &selection.account_id, Outcome::Transient);
                drop(guard);
                return Attempt::Excluded(selection.account_id.clone());
            }
            Err(e) => {
                tracing::warn!(request_id = %request_id, error = %e, "token.refresh_failed");
                report_outcome(state, &selection.account_id, Outcome::AuthFailed);
                logging::log_account_state(
                    &selection.account_id,
                    label,
                    "auth_error",
                    "refresh rejected",
                );
                drop(guard);
                return Attempt::Failover(None);
            }
        }
    }

    if !status.is_success() {
        let resp_headers = response.headers().clone();
        let body_text = response.text().await.unwrap_or_default();
        let mut record = usage_record(
            request_id,
            endpoint,
            model,
            sticky,
            selection,
            &account,
            status.as_u16(),
            started_ms,
        );
        if let Ok(value) = serde_json::from_str::<Value>(&body_text) {
            if let Some(counts) = usage_from_dialect(&value, upstream_dialect) {
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
            state.store.insert_upstream_event(&event_for(
                &record,
                kind,
                "http",
                record.error_code.clone(),
            ));
        }
        let mut outcome = backend.classify(status.as_u16(), &body_text, &resp_headers, now_ms());
        // Free models (id contains a delimited "free" token) bill at $0 and must never cool a
        // billing surface: a 402 on a free model is model-scoped, not account-wide, so the
        // credits/subscriptions surfaces stay routable for paid models on the same account.
        // `model` here is the requested id (alias or concrete), which is what the free alias
        // and billing-surface logic key on; a free model's concrete id also contains the token.
        if matches!(outcome, Outcome::QuotaExhausted { .. })
            && crate::aliases::contains_free_token(model)
            && let Outcome::QuotaExhausted { until_ms } = outcome
        {
            outcome = Outcome::ModelQuotaExhausted { until_ms };
        }
        let body_text = if translate_responses {
            crate::responses::error_body(&body_text)
        } else {
            body_text
        };
        // The transition, the snapshot, and the write share one critical section: a second lock
        // acquisition would let a concurrent transition interleave, and because the account write
        // covers the whole `scope_cooling` map it would persist a snapshot missing the other scope.
        let transition = {
            let mut pool = state.pool.lock().unwrap();
            match &outcome {
                // A model-scoped rejection is in-memory only: the account keeps serving its other
                // models, so there is no account row to persist.
                Outcome::ModelQuotaExhausted { until_ms } => {
                    pool.report_model_quota(&selection.account_id, model, *until_ms);
                    None
                }
                _ => {
                    let transition = pool.report_for(&selection.account_id, model, outcome.clone());
                    transition.and_then(|transition| {
                        let updated = pool.account(&selection.account_id).cloned();
                        if let Some(updated) = &updated {
                            state.store.update_account_health(updated);
                        }
                        updated.map(|updated| (transition, updated))
                    })
                }
            }
        };
        if let Some((transition, updated)) = transition {
            // The pool keeps the later of an existing and a newly reported deadline, so the event
            // and its log line must carry the effective deadline the routing decision uses rather
            // than this response's own value.
            let effective_until = match &transition {
                crate::pool::HealthTransition::Scope(scope) => {
                    updated.scope_cooling.get(scope).copied()
                }
                crate::pool::HealthTransition::Account(_) => Some(updated.reset_at),
            };
            match &outcome {
                Outcome::QuotaExhausted { until_ms } => {
                    let until_ms = effective_until.unwrap_or(*until_ms);
                    let scope = match &transition {
                        crate::pool::HealthTransition::Scope(scope) => Some(scope.as_str()),
                        crate::pool::HealthTransition::Account(_) => None,
                    };
                    logging::log_account_state(
                        &selection.account_id,
                        label,
                        "cooling",
                        &format!("until {until_ms} scope {}", scope.unwrap_or("account")),
                    );
                    let mut event =
                        event_for(&record, "quota_cooling", "http", record.error_code.clone());
                    event.until_ms = Some(until_ms);
                    event.detail = Some(scope.unwrap_or("account").to_string());
                    state.store.insert_upstream_event(&event);
                }
                Outcome::AuthFailed => {
                    logging::log_account_state(
                        &selection.account_id,
                        label,
                        "auth_error",
                        "upstream rejected credentials",
                    );
                }
                _ => {}
            }
        }
        if let Outcome::ModelQuotaExhausted { until_ms } = outcome {
            // The pool keeps the later of an existing and a newly reported deadline, so the event
            // and its log line must carry the effective deadline rather than this response's own
            // value - otherwise a shorter second 429 would understate how long routing is blocked.
            let effective = state
                .pool
                .lock()
                .unwrap()
                .model_cooling_until(&selection.account_id, model)
                .unwrap_or(until_ms);
            let mut event = event_for(&record, "quota_cooling", "http", record.error_code.clone());
            event.until_ms = Some(effective);
            event.detail = Some(model.to_string());
            state.store.insert_upstream_event(&event);
            logging::log_account_state(
                &selection.account_id,
                label,
                "model_cooling",
                &format!("{model} until {effective}"),
            );
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
            resp_headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json"),
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
                    // The transport error can name the upstream host, so only the operator's log
                    // sees it; the client gets the stable code and an actionable sentence.
                    tracing::warn!(
                        request_id = %request_id,
                        backend = %selection.backend.as_str(),
                        error = %e,
                        "upstream.buffered_body_error"
                    );
                    return Attempt::Respond(attach_request_id(
                        error_response(
                            StatusCode::BAD_GATEWAY,
                            &format!(
                                "upstream {} failed before the response completed; retry the request",
                                selection.backend.as_str()
                            ),
                            UPSTREAM_STREAM_ERROR,
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
                            record: usage_record(
                                request_id,
                                endpoint,
                                model,
                                sticky,
                                selection,
                                &account,
                                status.as_u16(),
                                started_ms,
                            ),
                            tap: UsageTap::new(false, upstream_dialect),
                            // The body arrived and ended; it just was not convertible.
                            ended: Some(StreamEnd::UpstreamError),
                            detail: "buffered",
                            synthesized_code: None,
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
                        upstream_model,
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
                record: usage_record(
                    request_id,
                    endpoint,
                    model,
                    sticky,
                    selection,
                    &account,
                    status.as_u16(),
                    started_ms,
                ),
                tap: UsageTap::new(false, upstream_dialect),
                ended: None,
                detail: "buffered",
                synthesized_code: None,
            };
            finish.tap.feed(&body);
            finish.ended = Some(StreamEnd::Eof);
            // Dropping the finish record annotates the log entry pushed above.
            drop(finish);
            let mut resp = Response::builder()
                .status(status_code)
                .body(Body::from(bytes))
                .expect("valid response");
            resp.headers_mut()
                .insert(http::header::CONTENT_TYPE, content_type);
            return Attempt::Respond(resp);
        }

        let mut stream: futures::stream::BoxStream<'static, UpstreamChunk> = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>))
            .boxed();
        let mut finish = UsageFinish {
            store: state.store.clone(),
            logs: state.logs.clone(),
            record: usage_record(
                request_id,
                endpoint,
                model,
                sticky,
                selection,
                &account,
                status.as_u16(),
                started_ms,
            ),
            tap: UsageTap::new(streaming, upstream_dialect),
            ended: None,
            detail: "stream",
            synthesized_code: None,
        };
        let translator = translate_stream.then(|| {
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::responses::ChatStreamTranslator::new(
                    upstream_model,
                    &format!("resp_{}", request_id.replace('-', "")),
                    started_ms / 1000,
                ),
            ))
        });
        // A translated stream must be held before the client sees anything for the same reason an
        // untranslated one is: the first chunk may be a terminal overload the failover path owns.
        let mut held: VecDeque<axum::body::Bytes> = VecDeque::new();
        if streaming {
            hold_until_output(&mut stream, &mut finish, &mut held, &state.stream_tuning).await;
        }
        if let Some(code) = finish.tap.overload_before_output().map(str::to_string) {
            logging::log_overload_failover(
                request_id,
                upstream_model,
                &selection.account_id,
                &account.label,
                &code,
                started.elapsed().as_millis() as u64,
            );
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
            finish.ended.get_or_insert(StreamEnd::Eof);
            finish.detail = "stream_failover";
            // Dropping `finish` persists the failed attempt and its `overloaded` event.
            drop(finish);
            drop(guard);
            // The replay is handed back to the client when every account overloads, so it must
            // already be in the client's protocol: a translated stream's held chunks are chat
            // completions SSE, not Responses events.
            let replay_body: Vec<u8> = match &translator {
                Some(translator) => {
                    let mut translator = translator.lock().unwrap();
                    let mut out: Vec<u8> = held
                        .iter()
                        .flat_map(|chunk| translator.feed(chunk))
                        .collect();
                    out.extend(translator.finish_on_eof());
                    out
                }
                None => held.into_iter().flatten().collect(),
            };
            let mut replay = Response::builder()
                .status(status_code)
                .body(Body::from(replay_body))
                .expect("valid replay response");
            replay
                .headers_mut()
                .insert(http::header::CONTENT_TYPE, content_type);
            return Attempt::Overloaded {
                account_id: selection.account_id.clone(),
                replay,
            };
        }
        let body_stream = Body::from_stream(futures::stream::unfold(
            StreamBody {
                stream,
                guard: Some(guard),
                finish,
                translator,
                held,
                backend: selection.backend.as_str().to_string(),
                idle_timeout: state.stream_tuning.idle_timeout,
                dialect: upstream_dialect,
                event_stream: translate_stream || upstream_is_sse(content_type.as_bytes()),
                closed: false,
            },
            poll_stream_body,
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
        resp.headers_mut()
            .insert(http::header::CONTENT_TYPE, content_type);
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

/// The terminal error events' fixed reason text. They name the condition, never upstream message
/// text, so a client can act on them and the operator's logs stay free of prompt content.
const CONNECTION_FAILED: &str = "the connection failed before the response completed";
const IDLE_EXCEEDED: &str = "the upstream sent nothing for longer than the configured idle timeout";

/// One upstream body chunk, or the transport failure that ended the transfer.
type UpstreamChunk = Result<axum::body::Bytes, Box<dyn std::error::Error + Send + Sync + 'static>>;

/// State carried across one streamed response body's polls.
struct StreamBody {
    stream: futures::stream::BoxStream<'static, UpstreamChunk>,
    guard: Option<InflightGuard>,
    finish: UsageFinish,
    translator: Option<Arc<Mutex<crate::responses::ChatStreamTranslator>>>,
    held: VecDeque<axum::body::Bytes>,
    backend: String,
    idle_timeout: Option<std::time::Duration>,
    /// The dialect of the *upstream* body, which is the dialect a terminal error event must be
    /// written in for a pass-through stream.
    dialect: UsageDialect,
    /// Whether the body on the wire is event-stream framing. A JSON document answered to a
    /// streaming request is forwarded as JSON, so an SSE event must never be spliced into it.
    event_stream: bool,
    /// Set once the terminal bytes have been emitted. The next poll ends the body, so a client can
    /// never receive the same terminal event twice.
    closed: bool,
}

impl StreamBody {
    /// Releases the account's in-flight slot as soon as the attempt is over, without waiting for
    /// the body to be dropped - a client may keep reading a connection it no longer needs.
    fn release_guard(&mut self) {
        self.guard = None;
    }

    /// The terminal bytes for a failure observed now, or nothing when the body already ended on a
    /// terminal event. A translated stream always gets the translator's single terminal event; a
    /// pass-through stream gets one dialect-shaped error event of its own.
    fn failure_tail(&mut self, reason: &str) -> Vec<u8> {
        let tail = match &self.translator {
            Some(translator) => translator.lock().unwrap().finish_on_error(),
            None if self.event_stream && !self.closed && self.finish.tap.terminal().is_none() => {
                terminal_stream_error(self.dialect, &self.backend, reason)
            }
            None => Vec::new(),
        };
        if !tail.is_empty() {
            self.finish.synthesized_code = Some(UPSTREAM_STREAM_ERROR);
            self.closed = true;
        }
        tail
    }
}

/// @cc [owner:ghuntley,label:proxy] upstream-failure-ends-body-cleanly
/// When the upstream fails after a 2xx response has started, the proxy MUST NOT abort the client's
/// body: it MUST emit exactly one terminal error event in the upstream's own dialect, carrying the
/// stable code `upstream_stream_error` and a message naming the backend, and then MUST end the body
/// normally so a complete `Content-Length` or chunked terminator always arrives. A translated
/// Responses stream MUST keep its single translator terminal event instead of gaining a second one.
/// A body already ended by a terminal event the upstream sent MUST NOT gain another.
async fn poll_stream_body(
    mut body: StreamBody,
) -> Option<(Result<axum::body::Bytes, std::io::Error>, StreamBody)> {
    if body.closed {
        return None;
    }
    // Anything the hold buffered is released first, in order, and translated on the way out
    // exactly as the live stream would have been.
    if let Some(chunk) = body.held.pop_front() {
        let out = match &body.translator {
            Some(translator) => translator.lock().unwrap().feed(&chunk),
            None => chunk.to_vec(),
        };
        return Some((Ok(axum::body::Bytes::from(out)), body));
    }
    if let Some(end) = body.finish.ended {
        // The hold already reached an end; do not poll again. The body still has to close
        // with an event when the end was an upstream failure, or the client sees a 200 body
        // that simply stops.
        let tail = match end {
            StreamEnd::UpstreamError => {
                logging::log_upstream_body_failure(
                    &body.finish.record.request_id,
                    &body.backend,
                    "connection_failed",
                );
                body.failure_tail(CONNECTION_FAILED)
            }
            _ => {
                let tail = body
                    .translator
                    .as_ref()
                    .map(|translator| translator.lock().unwrap().finish_on_eof())
                    .unwrap_or_default();
                body.closed = !tail.is_empty();
                tail
            }
        };
        body.release_guard();
        return (!tail.is_empty()).then_some((Ok(axum::body::Bytes::from(tail)), body));
    }
    let next = match body.idle_timeout {
        Some(idle) => match tokio::time::timeout(idle, body.stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                body.finish.ended = Some(StreamEnd::UpstreamError);
                logging::log_upstream_body_failure(
                    &body.finish.record.request_id,
                    &body.backend,
                    "idle",
                );
                let tail = body.failure_tail(IDLE_EXCEEDED);
                body.release_guard();
                return (!tail.is_empty()).then_some((Ok(axum::body::Bytes::from(tail)), body));
            }
        },
        None => body.stream.next().await,
    };
    match next {
        Some(Ok(chunk)) => {
            body.finish.tap.feed(&chunk);
            let out = match &body.translator {
                Some(translator) => translator.lock().unwrap().feed(&chunk),
                None => chunk.to_vec(),
            };
            Some((Ok(axum::body::Bytes::from(out)), body))
        }
        Some(Err(_)) => {
            body.finish.ended = Some(StreamEnd::UpstreamError);
            logging::log_upstream_body_failure(
                &body.finish.record.request_id,
                &body.backend,
                "connection_failed",
            );
            let tail = body.failure_tail(CONNECTION_FAILED);
            body.release_guard();
            (!tail.is_empty()).then_some((Ok(axum::body::Bytes::from(tail)), body))
        }
        None => {
            body.finish.ended = Some(StreamEnd::Eof);
            let tail = body
                .translator
                .as_ref()
                .map(|translator| translator.lock().unwrap().finish_on_eof())
                .unwrap_or_default();
            body.closed = !tail.is_empty();
            (!tail.is_empty()).then_some((Ok(axum::body::Bytes::from(tail)), body))
        }
    }
}

/// @cc [owner:ghuntley,label:proxy] pre-output-hold-bounded
/// `hold_until_output` MUST buffer upstream chunks (fed to the tap, in order) only until the tap
/// sees model output or a terminal event, the stream ends or errors, the hold deadline elapses,
/// or the hold byte bound is reached, whichever comes first. It MUST NOT drop or reorder a
/// chunk, and a stream end observed while holding MUST be recorded in `finish.ended`. Expiry is
/// not a failure: it only stops holding, so a legitimately long first token is never cut.
async fn hold_until_output<S>(
    stream: &mut S,
    finish: &mut UsageFinish,
    held: &mut VecDeque<axum::body::Bytes>,
    tuning: &StreamTuning,
) where
    S: futures::Stream<Item = UpstreamChunk> + Unpin,
{
    let deadline = tokio::time::Instant::now() + tuning.pre_output_hold;
    let mut bytes = 0usize;
    while !finish.tap.decided() && bytes < tuning.pre_output_hold_bytes {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                finish.tap.feed(&chunk);
                bytes += chunk.len();
                held.push_back(chunk);
            }
            Ok(Some(Err(_))) => {
                finish.ended = Some(StreamEnd::UpstreamError);
                return;
            }
            Ok(None) => {
                finish.ended = Some(StreamEnd::Eof);
                return;
            }
            // The hold window elapsed. Holding any longer would delay a stream that is merely
            // slow to its first token, so streaming simply continues.
            Err(_) => return,
        }
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

/// Records what the backend observed about an upstream response, and cools the account when the
/// upstream reports a window it cannot serve.
fn observe_response(
    state: &Arc<AppState>,
    backend: &dyn crate::provider::Backend,
    account: &crate::models::Account,
    account_id: &str,
    headers: &HeaderMap,
) {
    let now = now_ms();
    if let Some(observation) = backend.observe_response(account, headers, now) {
        state
            .observations
            .lock()
            .unwrap()
            .insert(account_id.to_string(), observation);
    }
    let Some(until) = backend.exhausted_until_ms(headers) else {
        return;
    };
    if until <= now {
        return;
    }
    // Decide and write under one lock, and never overwrite operator state: a response that lands
    // after an operator disabled the account (or after it went auth_error) must not resurrect it
    // to `Cooling`, because `sweep` would then readmit it to `Healthy` and the block would vanish.
    // A second lock acquisition would also let a concurrent longer deadline be shortened.
    {
        let mut pool = state.pool.lock().unwrap();
        let Some(live) = pool.account(account_id).cloned() else {
            return;
        };
        // `Cooling` participates so a later window can extend an existing deadline;
        // `AuthError`/`Disabled` never do, or `sweep` would readmit them as `Healthy`.
        if !matches!(live.status, AccountStatus::Healthy | AccountStatus::Cooling)
            || until <= live.reset_at
        {
            return;
        }
        pool.set_status(account_id, AccountStatus::Cooling, until);
        state
            .store
            .update_account_status(account_id, AccountStatus::Cooling, until);
    }
    logging::log_account_state(
        account_id,
        Some(&account.label),
        "cooling",
        &format!("upstream window exhausted until {until}"),
    );
}

fn report_outcome(state: &Arc<AppState>, account_id: &str, outcome: Outcome) {
    // Transition and persist under one lock, as `attempt_account` and `enable_account` do. Writing
    // after the guard drops would let a concurrent `enable`/`sync_from_store` land in between and
    // be overwritten by this stale status on the next reload.
    let mut pool = state.pool.lock().unwrap();
    if let Some(new_status) = pool.report(account_id, outcome)
        && let Some(account) = pool.account(account_id)
    {
        state
            .store
            .update_account_status(account_id, new_status, account.reset_at);
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
    let catalogs: std::collections::HashMap<BackendId, Vec<_>> = BackendId::all()
        .iter()
        .map(|b| (*b, state.store.catalog(*b)))
        .collect();
    let servable = crate::listing::backends_with_accounts(&state.store.list_accounts());
    // A poisoned pool lock keeps the listing alive with no aliases, the same tolerance the handler
    // had before the merge moved out: a panic elsewhere must not take model discovery down.
    let aliases = state
        .pool
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .aliases
        .clone();
    let data: Vec<Value> = crate::listing::advertised_models(&catalogs, &servable, &aliases)
        .iter()
        .map(crate::listing::AdvertisedModel::to_json)
        .collect();
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
        assert_eq!(
            extract_sticky_key(&body, Some("sess-3")).as_deref(),
            Some("sess-3")
        );
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
        let backend = crate::cline::ClineBackend {
            cooldown_ms: 1000,
            user_agent: None,
        };
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
            if needs_buffering {
                !upstream_is_sse(ct)
            } else {
                usage_is_stream(&json!({"stream": true}), ct)
            }
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

    fn usage_finish_for_test(tap: UsageTap) -> UsageFinish {
        UsageFinish {
            store: Arc::new(Store::in_memory().unwrap()),
            logs: Arc::new(Mutex::new(VecDeque::new())),
            record: UsageRecord {
                request_id: "req-test".into(),
                account_id: "acct-test".into(),
                account_label: "label-test".into(),
                backend: "codex".into(),
                model: "gpt-5.5".into(),
                ..Default::default()
            },
            tap,
            ended: None,
            detail: "stream",
            synthesized_code: None,
        }
    }

    fn body(chunks: Vec<UpstreamChunk>, dialect: UsageDialect) -> StreamBody {
        StreamBody {
            stream: futures::stream::iter(chunks).boxed(),
            guard: None,
            finish: usage_finish_for_test(UsageTap::new(true, dialect)),
            translator: None,
            held: VecDeque::new(),
            backend: "codex".into(),
            idle_timeout: None,
            dialect,
            event_stream: true,
            closed: false,
        }
    }

    fn chunk(text: &'static str) -> UpstreamChunk {
        Ok(axum::body::Bytes::from_static(text.as_bytes()))
    }

    fn failure() -> UpstreamChunk {
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )))
    }

    /// Drains the body the way hyper does and returns everything the client would receive.
    async fn drain(mut body: StreamBody) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some((chunk, next)) = poll_stream_body(body).await {
            out.extend_from_slice(&chunk.expect("the body must never abort the client's frame"));
            body = next;
        }
        out
    }

    #[tokio::test]
    async fn mid_stream_failure_ends_a_chat_stream_with_one_error_event() {
        let out = drain(body(
            vec![
                chunk("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
                failure(),
            ],
            UsageDialect::Chat,
        ))
        .await;
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("data: {\"choices\""), "{text}");
        let last = text
            .split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .last()
            .unwrap();
        let data = last.strip_prefix("data: ").expect("a data frame");
        let value: Value = serde_json::from_str(data).expect("one complete frame");
        assert_eq!(value["error"]["code"], UPSTREAM_STREAM_ERROR);
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("codex")
        );
        assert_eq!(text.matches(UPSTREAM_STREAM_ERROR).count(), 1);
    }

    #[tokio::test]
    async fn mid_stream_failure_ends_a_responses_stream_with_one_error_event() {
        let out = drain(body(
            vec![
                chunk("data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n"),
                failure(),
            ],
            UsageDialect::Responses,
        ))
        .await;
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with("\n\n"),
            "the final frame is terminated: {text:?}"
        );
        assert_eq!(text.matches(UPSTREAM_STREAM_ERROR).count(), 1, "{text}");
        assert!(text.contains("event: error\ndata: "), "{text}");
    }

    #[tokio::test]
    async fn mid_stream_failure_ends_an_anthropic_stream_with_one_error_event() {
        let out = drain(body(
            vec![
                chunk("event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n"),
                failure(),
            ],
            UsageDialect::Anthropic,
        ))
        .await;
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"type\":\"error\""), "{text}");
        assert!(text.contains(UPSTREAM_STREAM_ERROR), "{text}");
        assert_eq!(text.matches(UPSTREAM_STREAM_ERROR).count(), 1, "{text}");
    }

    /// An upstream that already sent its terminal event and then fails must not gain a second one.
    #[tokio::test]
    async fn a_completed_stream_gains_no_error_event() {
        let out = drain(body(
            vec![
                chunk("data: {\"type\":\"response.completed\",\"response\":{}}\n\n"),
                failure(),
            ],
            UsageDialect::Responses,
        ))
        .await;
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains(UPSTREAM_STREAM_ERROR), "{text}");
        assert_eq!(text.matches("response.completed").count(), 1, "{text}");
    }

    /// A translated stream must keep the translator's single terminal event: no second one, and
    /// no truncated frame.
    #[tokio::test]
    async fn a_translated_stream_failure_emits_exactly_one_terminal_event() {
        let mut body = body(
            vec![
                chunk("data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n"),
                failure(),
            ],
            UsageDialect::Chat,
        );
        body.translator = Some(Arc::new(Mutex::new(
            crate::responses::ChatStreamTranslator::new("m", "resp_t", 5),
        )));
        let text = String::from_utf8(drain(body).await).unwrap();
        let terminal = text
            .split("\n\n")
            .filter(|frame| frame.lines().any(|line| line == "event: response.failed"))
            .count();
        assert_eq!(terminal, 1, "exactly one terminal event: {text}");
        assert_eq!(text.matches(UPSTREAM_STREAM_ERROR).count(), 1, "{text}");
        assert!(
            !text.contains("event: error\ndata: {\"type\":\"error\""),
            "{text}"
        );
    }

    #[test]
    fn an_upstream_failure_with_no_output_still_tells_the_client() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let text = runtime.block_on(drain(body(vec![failure()], UsageDialect::Chat)));
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains(UPSTREAM_STREAM_ERROR), "{text}");
    }

    /// The case this change exists for: an attempt whose future is dropped while it is still
    /// waiting for upstream response headers. The guard fires on drop without any explicit call,
    /// which is what makes a request nobody finished visible instead of silent.
    #[test]
    fn dropping_an_attempt_that_never_got_headers_reports_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let reported = std::sync::Arc::new(std::sync::Mutex::new(Vec::<AttemptAbandon>::new()));
        let sink = reported.clone();
        runtime.block_on(async move {
            let watch = AttemptWatch {
                request_id: "req-dropped".into(),
                attempt: 1,
                cache_key: "session-1".into(),
                model: "m".into(),
                upstream_model: "m".into(),
                endpoint: "/v1/responses".into(),
                backend: "codex".into(),
                account_id: "acct-a".into(),
                label: Some("a@example.com".into()),
                started: Instant::now(),
                stage: AttemptStage::AwaitingHeaders,
                ended: None,
            };
            // Stand in for the client walking away: the attempt's future is never polled again.
            let mut pending: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                Box::pin(std::future::pending());
            let _ = tokio::time::timeout(std::time::Duration::from_millis(5), &mut pending).await;
            let abandon = watch.abandonment().expect("a dropped attempt is reported");
            sink.lock().unwrap().push(abandon);
        });
        let reported = reported.lock().unwrap();
        assert_eq!(reported.len(), 1, "exactly one terminal report");
        assert_eq!(reported[0].stage, "awaiting_headers");
        assert_eq!(reported[0].reason, "upstream_silent");
        assert!(reported[0].elapsed_ms > 0, "the elapsed time is non-zero");
    }

    /// Two dropped attempts in a row are logged separately and fall in different stages: the
    /// first stopped before the upstream was contacted, the second while waiting for its head.
    #[test]
    fn two_abandoned_attempts_are_distinguishable_by_stage_and_attempt_number() {
        let first = AttemptWatch {
            request_id: "req-dup".into(),
            attempt: 1,
            cache_key: "none".into(),
            model: "m".into(),
            upstream_model: "m".into(),
            endpoint: "/v1/responses".into(),
            backend: "codex".into(),
            account_id: "acct-a".into(),
            label: Some("a@example.com".into()),
            started: Instant::now(),
            stage: AttemptStage::Token,
            ended: None,
        };
        let first = first.abandonment().expect("a dropped attempt is reported");
        assert_eq!(first.stage, "token");
        assert_eq!(first.reason, "client_aborted");

        let mut second = AttemptWatch {
            request_id: "req-dup".into(),
            attempt: 2,
            cache_key: "none".into(),
            model: "m".into(),
            upstream_model: "m".into(),
            endpoint: "/v1/responses".into(),
            backend: "codex".into(),
            account_id: "acct-b".into(),
            label: Some("b@example.com".into()),
            started: Instant::now(),
            stage: AttemptStage::Token,
            ended: None,
        };
        second.awaiting_headers();
        // `awaiting_headers` only emits the start mark; the guard still reports on drop.
        let second = second.abandonment().expect("a silent upstream is reported");
        assert_eq!(second.stage, "awaiting_headers");
        assert_eq!(second.reason, "upstream_silent");
    }

    /// Once the upstream has failed for a reason it logged itself, dropping the guard must stay
    /// silent: the attempt already has a line.
    #[test]
    fn an_attempt_that_already_failed_is_not_reported_twice() {
        let mut watch = AttemptWatch {
            request_id: "req-failed".into(),
            attempt: 1,
            cache_key: "none".into(),
            model: "m".into(),
            upstream_model: "m".into(),
            endpoint: "/v1/responses".into(),
            backend: "codex".into(),
            account_id: "acct-a".into(),
            label: None,
            started: Instant::now(),
            stage: AttemptStage::AwaitingHeaders,
            ended: None,
        };
        watch.finish(AttemptEnd::Failed);
        assert!(watch.abandonment().is_none());
    }

    /// The start of an attempt is recorded before anything can hide it, and the elapsed time on a
    /// reported abandonment is measured from it rather than computed at drop.
    #[test]
    fn an_abandonment_carries_the_elapsed_time_since_the_attempt_started() {
        let watch = AttemptWatch {
            request_id: "req-time".into(),
            attempt: 1,
            cache_key: "none".into(),
            model: "m".into(),
            upstream_model: "m".into(),
            endpoint: "/v1/responses".into(),
            backend: "codex".into(),
            account_id: "acct-a".into(),
            label: None,
            started: Instant::now() - std::time::Duration::from_millis(857_000),
            stage: AttemptStage::AwaitingHeaders,
            ended: None,
        };
        let abandon = watch.abandonment().expect("reported");
        assert!(
            abandon.elapsed_ms >= 856_000,
            "the 857s gap must be visible: {}",
            abandon.elapsed_ms
        );
    }

    #[tokio::test]
    async fn a_gate_that_resolves_inside_the_bound_is_untouched() {
        let outcome: Bounded<&'static str, &'static str> = send_bounded(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Ok("headers")
            },
            Some(std::time::Duration::from_millis(500)),
        )
        .await;
        assert!(matches!(outcome, Bounded::Value("headers")));
    }

    #[tokio::test]
    async fn a_configured_header_bound_expires_as_a_timeout_not_a_transport_error() {
        let started = Instant::now();
        let outcome: Bounded<&'static str, &'static str> = send_bounded(
            std::future::pending(),
            Some(std::time::Duration::from_millis(20)),
        )
        .await;
        assert!(matches!(outcome, Bounded::TimedOut));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_zero_header_bound_means_no_bound_at_all() {
        // `None` (what a resolved `0` becomes) awaits the send unbounded: a slow head is delivered
        // rather than cut, exactly as before this bound existed.
        let outcome: Bounded<&'static str, &'static str> = send_bounded(
            async {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                Ok("headers")
            },
            None,
        )
        .await;
        assert!(matches!(outcome, Bounded::Value("headers")));
    }

    #[tokio::test]
    async fn a_transport_error_is_still_reported_as_one() {
        let outcome: Bounded<&'static str, &'static str> = send_bounded(
            async { Err("reset") },
            Some(std::time::Duration::from_secs(5)),
        )
        .await;
        assert!(matches!(outcome, Bounded::Transport("reset")));
    }

    /// A body over an arbitrary chunk stream, with the mid-body idle bound under test.
    fn body_with(
        stream: futures::stream::BoxStream<'static, UpstreamChunk>,
        dialect: UsageDialect,
        idle_timeout: Option<std::time::Duration>,
    ) -> StreamBody {
        let mut body = body(Vec::new(), dialect);
        body.stream = stream;
        body.idle_timeout = idle_timeout;
        body
    }

    /// One delta, then complete silence: what an upstream that stopped talking mid-body looks like.
    fn one_chunk_then_silence(
        text: &'static str,
    ) -> futures::stream::BoxStream<'static, UpstreamChunk> {
        futures::stream::iter(vec![chunk(text)])
            .chain(futures::stream::pending())
            .boxed()
    }

    /// A stream that keeps emitting past the idle bound, so the per-chunk timer must reset.
    fn steady_stream(
        count: u32,
        gap: std::time::Duration,
    ) -> futures::stream::BoxStream<'static, UpstreamChunk> {
        futures::stream::unfold(0u32, move |step| async move {
            if step == count {
                return None;
            }
            tokio::time::sleep(gap).await;
            Some((
                chunk("data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n"),
                step + 1,
            ))
        })
        .boxed()
    }

    /// Silence longer than the bound ends the body with exactly one terminal error event, and the
    /// body still frames completely - the #30 regression must not come back through this path.
    #[tokio::test]
    async fn silence_past_the_idle_bound_ends_the_body_cleanly() {
        let out = drain(body_with(
            one_chunk_then_silence(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            ),
            UsageDialect::Responses,
            Some(std::time::Duration::from_millis(30)),
        ))
        .await;
        let text = String::from_utf8(out).expect("the body is complete utf-8");
        assert!(text.contains("partial"), "{text}");
        assert_eq!(
            text.matches(UPSTREAM_STREAM_ERROR).count(),
            1,
            "exactly one terminal error event: {text}"
        );
        assert!(
            text.ends_with("\n\n"),
            "the last frame is terminated: {text:?}"
        );
    }

    /// The timer wraps one `next()` call and resets on every chunk, so a stream that is long but
    /// active is never cut.
    #[tokio::test]
    async fn an_active_stream_is_never_cut_by_the_idle_bound() {
        // Twelve 10ms gaps: well over the 60ms bound in total, never near it in one gap.
        let out = drain(body_with(
            steady_stream(12, std::time::Duration::from_millis(10)),
            UsageDialect::Responses,
            Some(std::time::Duration::from_millis(60)),
        ))
        .await;
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("\"delta\":\"x\"").count(), 12, "{text}");
        assert!(!text.contains(UPSTREAM_STREAM_ERROR), "{text}");
    }

    /// `0` disables the bound: silence longer than any small bound is delivered, and the stream
    /// completes on its own terms.
    #[tokio::test]
    async fn a_disabled_idle_bound_lets_a_quiet_stream_finish() {
        let stream = futures::stream::unfold(0u32, |step| async move {
            match step {
                0 => Some((
                    chunk("data: {\"type\":\"response.output_text.delta\",\"delta\":\"slow\"}\n\n"),
                    1,
                )),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                    Some((
                        chunk("data: {\"type\":\"response.completed\",\"response\":{}}\n\n"),
                        2,
                    ))
                }
                _ => None,
            }
        })
        .boxed();
        let out = drain(body_with(stream, UsageDialect::Responses, None)).await;
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("slow"), "{text}");
        assert!(text.contains("response.completed"), "{text}");
        assert!(!text.contains(UPSTREAM_STREAM_ERROR), "{text}");
    }

    #[test]
    fn stream_request_overrides_incorrect_json_content_type() {
        let body = json!({"stream": true});
        assert!(usage_is_stream(&body, b"application/json"));
        assert!(usage_is_stream(
            &json!({}),
            b"text/event-stream; charset=utf-8"
        ));
        assert!(!usage_is_stream(&json!({}), b"application/json"));
    }
}
