use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use underclass::claude::ClaudeBackend;
use underclass::cline::ClineBackend;
use underclass::codex::CodexBackend;
use underclass::copilot::CopilotBackend;
use underclass::flows::FlowRegistry;
use underclass::models::{
    Account, AccountStatus, BackendId, ModelInfo, RequestLogEntry, Surface, now_ms,
};
use underclass::opencode_go::OpencodeGoBackend;
use underclass::pool::PoolCore;
use underclass::provider::BackendMap;
use underclass::proxy::AppState;
use underclass::store::Store;
use underclass::tokens::TokenManager;

#[derive(Default)]
struct MockState {
    failing: BTreeSet<String>,
    /// Models whose upstream answers `402 Payment Required` (Cline's exhausted-credits signal).
    payment_required: BTreeSet<String>,
    /// Upstream model IDs in request order, so a test can assert what was attempted.
    models: VecDeque<String>,
    served_by: VecDeque<String>,
    fourtwonined: VecDeque<String>,
    /// `stream_options.include_usage` seen per bearer, in request order. Keyed by bearer so a
    /// parallel test's request cannot be mistaken for this test's own.
    chat_usage_options: std::collections::HashMap<String, VecDeque<bool>>,
    reject_stream_usage: bool,
    /// Remaining 403 responses to serve per bearer token, modelling a redemption-triggered
    /// credential rejection that a token refresh clears.
    forbidden: std::collections::HashMap<String, u32>,
    /// The paths Go requests arrived on, in order, so a test can assert that a model was sent to
    /// the upstream API its family requires rather than the path the client used.
    go_paths: VecDeque<String>,
    /// The `x-opencode-session` value per request, in order.
    go_sessions: VecDeque<String>,
    /// The `User-Agent` the upstream saw, keyed by bearer and in request order. The mock upstream
    /// is shared by every test in this binary, so a bare queue would let a parallel test's request
    /// be mistaken for this one's.
    user_agents: std::collections::HashMap<String, VecDeque<String>>,
}

/// The ClinePass models this mock's recommended-models feed publishes. The real feed is public and
/// process-global, so the mock serves a fixed set rather than per-test state a parallel test could
/// observe mid-assertion.
const MOCK_CLINE_PASS_FEED: &[&str] = &[
    "cline-pass/deepseek-v4.1-flash-1017",
    "cline-pass/feed-only-model-e2e",
];

/// The free models this mock's feed publishes, which the listing omits just like the pass models.
const MOCK_CLINE_FREE_FEED: &[&str] = &["reflection/beam-free"];

/// The credit-based ids this mock's `/api/v1/models` listing carries.
const MOCK_CLINE_LISTING: &[&str] = &["anthropic/claude-sonnet-4-6", "openai/gpt-5.4:batch"];

async fn mock_responses(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: String,
) -> Response {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("unknown")
        .to_string();
    let wants_stream = body.contains("\"stream\": true") || body.contains("\"stream\":true");
    // Cline's recommended-models feed: the only published list of ClinePass models. Served without
    // a credential, the way the real endpoint is.
    if uri.path().ends_with("/ai/cline/recommended-models") {
        let pass = MOCK_CLINE_PASS_FEED
            .iter()
            .map(|id| serde_json::json!({"id": id, "name": id, "description": "", "tags": []}))
            .collect::<Vec<_>>();
        let free = MOCK_CLINE_FREE_FEED
            .iter()
            .map(|id| serde_json::json!({"id": id, "name": "Beam", "description": "", "tags": []}))
            .collect::<Vec<_>>();
        return (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            serde_json::json!({
                "recommended": [{"id": "anthropic/claude-sonnet-5.5"}],
                "free": free,
                "clinePass": pass,
            })
            .to_string(),
        )
            .into_response();
    }
    // Cline's public `/api/v1/models` listing, which carries no `cline-pass/` id at all.
    if uri.path().ends_with("/api/v1/models") {
        let data = MOCK_CLINE_LISTING
            .iter()
            .map(|id| serde_json::json!({"id": id, "object": "model"}))
            .collect::<Vec<_>>();
        return (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            serde_json::json!({"object": "list", "data": data}).to_string(),
        )
            .into_response();
    }
    state
        .lock()
        .unwrap()
        .user_agents
        .entry(bearer.clone())
        .or_default()
        .push_back(
            headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string(),
        );
    if bearer.starts_with("go-") {
        let session = headers
            .get("x-opencode-session")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut mock = state.lock().unwrap();
        mock.go_paths.push_back(uri.path().to_string());
        mock.go_sessions.push_back(session);
    }

    if bearer.starts_with("bad-") {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"message":"bad token"}}"#.to_string(),
        )
            .into_response();
    }

    let remaining = state
        .lock()
        .unwrap()
        .forbidden
        .get(&bearer)
        .copied()
        .unwrap_or(0);
    if remaining > 0 {
        state
            .lock()
            .unwrap()
            .forbidden
            .insert(bearer.clone(), remaining - 1);
        return (
            axum::http::StatusCode::FORBIDDEN,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"message":"credential rejected"}}"#.to_string(),
        )
            .into_response();
    }

    // A model-scoped refusal applies to one model only, the way Anthropic's weekly
    // other-models window does.
    let requested_model = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    state
        .lock()
        .unwrap()
        .models
        .push_back(requested_model.clone());
    let payment_required = state
        .lock()
        .unwrap()
        .payment_required
        .contains(&requested_model);
    if payment_required {
        return (
            axum::http::StatusCode::PAYMENT_REQUIRED,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"code":"insufficient_credits","message":"credit balance exhausted"}}"#
                .to_string(),
        )
            .into_response();
    }

    let model_scoped_refusal = bearer.starts_with("claude-model-quota-")
        && uri.path().ends_with("/messages")
        && requested_model == "claude-opus-5-5";
    // Codex refuses a model the account's plan cannot use, naming the model and nothing else.
    if bearer.starts_with("tok-unsupported-") && requested_model == "gpt-6-astra" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            format!(
                r#"{{"detail":"The '{requested_model}' model is not supported when using Codex with a ChatGPT account."}}"#
            ),
        )
            .into_response();
    }
    // Go reports a model it cannot currently serve as 402 "Endpoint is unavailable", which is one
    // model's outage and must not take the account out of rotation for every other model.
    if bearer.starts_with("go-endpoint-") && requested_model == "kimi-k3" {
        return (
            axum::http::StatusCode::PAYMENT_REQUIRED,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"type":"server_error","message":"Upstream request failed: Endpoint is unavailable."}}"#
                .to_string(),
        )
            .into_response();
    }
    // Go meters usage per model and reports the exhaustion as a rate limit on that model.
    if bearer.starts_with("go-quota-") && requested_model == "kimi-k3" {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"type":"server_error","message":"Upstream request failed: Go usage limit reached for this model"}}"#
                .to_string(),
        )
            .into_response();
    }
    let failing = state.lock().unwrap().failing.contains(&bearer)
        || (bearer.starts_with("claude-quota-") && uri.path().ends_with("/messages"))
        || model_scoped_refusal;
    if failing {
        state.lock().unwrap().fourtwonined.push_back(bearer.clone());
        if uri.path().ends_with("/messages") {
            // Anthropic reports the exhausted subscription window on the response headers: a
            // rejected shared window blocks the account, a rejected model window blocks the
            // model only.
            let reset = (now_ms() / 1000 + 3600).to_string();
            let model_scoped = bearer.starts_with("claude-model-quota-");
            fn name(value: &'static str) -> axum::http::header::HeaderName {
                axum::http::header::HeaderName::from_static(value)
            }
            let mut headers = vec![
                (
                    name("anthropic-ratelimit-unified-status"),
                    "rejected".to_string(),
                ),
                (name("anthropic-ratelimit-unified-reset"), reset),
            ];
            if model_scoped {
                headers.push((
                    name("anthropic-ratelimit-unified-5h-status"),
                    "allowed".to_string(),
                ));
                headers.push((
                    name("anthropic-ratelimit-unified-7d-status"),
                    "allowed".to_string(),
                ));
                headers.push((
                    name("anthropic-ratelimit-unified-7d_oi-status"),
                    "rejected".to_string(),
                ));
            }
            let mut builder = axum::response::Response::builder()
                .status(axum::http::StatusCode::TOO_MANY_REQUESTS)
                .header(axum::http::header::CONTENT_TYPE, "application/json");
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            return builder
                .body(axum::body::Body::from(
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limit exceeded"}}"#,
                ))
                .unwrap();
        }
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/json".to_string(),
            )],
            r#"{"error":{"code":"usage_limit_reached","message":"usage limit reached"}}"#
                .to_string(),
        )
            .into_response();
    }

    state.lock().unwrap().served_by.push_back(bearer.clone());
    if uri.path().ends_with("/messages") {
        // Echo what the Anthropic upstream received, so the test can assert the credential
        // swap, the forwarded client headers, and the verbatim body.
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json".to_string())],
            serde_json::json!({
                "account": bearer,
                "app": header("x-app"),
                "beta": header("anthropic-beta"),
                "session": header("x-claude-code-session-id"),
                "body": serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null),
                "raw": body,
                "usage": {"input_tokens": 11, "output_tokens": 3}
            })
            .to_string(),
        )
            .into_response();
    }
    let chat = uri.path().ends_with("/chat/completions");
    if chat {
        let enabled = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/stream_options/include_usage")
                    .and_then(|v| v.as_bool())
            })
            == Some(true);
        let mut mock = state.lock().unwrap();
        mock.chat_usage_options
            .entry(bearer.clone())
            .or_default()
            .push_back(enabled);
        // Bearer-keyed so a test can exercise the retry without flipping process-wide state that
        // the other tests in this binary depend on.
        let rejects_for_bearer = bearer == "key-reject-usage";
        if enabled && (mock.reject_stream_usage || rejects_for_bearer) {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "application/json".to_string(),
                )],
                r#"{"error":{"message":"stream_options.include_usage unsupported"}}"#.to_string(),
            )
                .into_response();
        }
    }
    if chat && bearer.starts_with("key-chat-ovl-") {
        // A chat upstream that fails before any output; the frames are chat-shaped, so a
        // translated request only reaches the client correctly through the translator.
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"error\":{\"message\":\"Selected model is at capacity\",\"type\":\"server_is_overloaded\"}}\n\n"
                .to_string(),
        )
            .into_response();
    }
    if serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("overload").and_then(|flag| flag.as_bool()))
        == Some(true)
    {
        // A 200 stream that fails before any output, like upstream capacity errors in practice.
        // A chat upstream speaks chat frames, which is what the translator must be handed.
        let payload = if chat {
            "data: {\"error\":{\"message\":\"Selected model is at capacity\",\"type\":\"server_is_overloaded\"}}\n\n"
        } else {
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"Selected model is at capacity. Please try a different model.\"}}}\n\n"
        };
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            payload.to_string(),
        )
            .into_response();
    }
    if bearer.starts_with("tok-ovl-") && wants_stream {
        // The shape Codex upstream sends today: a 200 stream, a pause, then a stream `error` event
        // whose code is nested under `error`.
        let stream = futures::stream::unfold(0, |step| async move {
            match step {
                0 => Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                        b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
                    )),
                    1,
                )),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    Some((Ok(axum::body::Bytes::from_static(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"service_unavailable_error\",\"code\":\"server_is_overloaded\",\"message\":\"Selected model is at capacity. Please try a different model.\"}}\n\n")), 2))
                }
                _ => None,
            }
        });
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response();
    }
    if bearer.starts_with("tok-late-ovl-") && wants_stream {
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"server_is_overloaded\"}}\n\n",
        ).into_response();
    }
    if serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("slow_abort").and_then(|flag| flag.as_bool()))
        == Some(true)
    {
        let stream = futures::stream::unfold(0, |step| async move {
            match step {
                0 => Some((Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n")), 1)),
                1 => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    Some((Ok(axum::body::Bytes::from_static(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n")), 2))
                }
                _ => None,
            }
        });
        return (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(stream),
        )
            .into_response();
    }
    let usage_event = if chat {
        "data: {\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n"
    } else {
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":3}}}\n\n"
    };
    let payload = format!(
        "data: {{\"type\":\"response.started\",\"account\":\"{bearer}\"}}\n\ndata: {{\"type\":\"output_text.delta\",\"delta\":\"hello from {bearer}\"}}\n\n{usage_event}data: [DONE]\n\n"
    );
    if wants_stream {
        (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/event-stream".to_string(),
            )],
            payload,
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json".to_string())],
            if chat { format!(r#"{{"account":"{bearer}","usage":{{"prompt_tokens":11,"completion_tokens":3}}}}"#) }
            else { format!(r#"{{"account":"{bearer}","output_text":"hello from {bearer}","usage":{{"input_tokens":11,"output_tokens":3}}}}"#) },
        ).into_response()
    }
}

/// Starts the shared mock upstream on a dedicated OS thread and returns its state.
///
/// The server deliberately does not live on a test's own runtime: every `#[tokio::test]` builds
/// and then drops its runtime, which would cancel the server task and leave the cached URL
/// pointing at a dead port for every later test in the binary. A dedicated thread with its own
/// runtime keeps the server reachable for the lifetime of the test process.
fn mock_upstream() -> &'static Arc<Mutex<MockState>> {
    static STATE: OnceLock<Arc<Mutex<MockState>>> = OnceLock::new();
    static URL: OnceLock<String> = OnceLock::new();
    let state = STATE.get_or_init(Default::default);
    URL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        let served = state.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let app = axum::Router::new()
                    .fallback(mock_responses)
                    .with_state(served);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tx.send(addr).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let addr = rx.recv().expect("mock upstream failed to bind");
        unsafe {
            std::env::set_var("UNDERCLASS_CODEX_UPSTREAM", format!("http://{addr}"));
            std::env::set_var("UNDERCLASS_COPILOT_UPSTREAM", format!("http://{addr}"));
            std::env::set_var("UNDERCLASS_CLAUDE_UPSTREAM", format!("http://{addr}"));
            std::env::set_var("UNDERCLASS_CLINE_UPSTREAM", format!("http://{addr}"));
            std::env::set_var("UNDERCLASS_GO_UPSTREAM", format!("http://{addr}"));
        }
        format!("http://{addr}")
    });
    state
}

fn account(id: &str, backend: BackendId, token: &str) -> Account {
    let ts = now_ms();
    Account {
        id: id.into(),
        backend,
        label: id.into(),
        refresh_token: Some(format!("refresh-{id}")),
        access_token: Some(token.into()),
        expires_at: ts + 3_600_000,
        token_refreshed_at: ts,
        account_id: Some(format!("chatgpt-{id}")),
        residency: None,
        enterprise_url: None,
        plan: None,
        scope_cooling: Default::default(),
        status: AccountStatus::Healthy,
        reset_at: 0,
        created_at: ts,
        updated_at: ts,
    }
}

fn codex_account(id: &str, token: &str) -> Account {
    account(id, BackendId::Codex, token)
}

fn copilot_account(id: &str, token: &str) -> Account {
    let mut a = account(id, BackendId::Copilot, "unused-access");
    a.refresh_token = Some(token.to_string());
    a
}

/// The surfaces the end-to-end app serves.
fn scoped_model(id: &str, scope: &str) -> ModelInfo {
    ModelInfo {
        scope: scope.into(),
        ..model(id)
    }
}

/// Go holds a static Console API key in the same column as Cline's.
fn go_account(id: &str, token: &str) -> Account {
    let mut a = account(id, BackendId::OpencodeGo, "unused-access");
    a.refresh_token = Some(token.to_string());
    a
}

/// The user agent the upstream saw on request `n` (0-based) for `bearer`.
fn upstream_user_agent(bearer: &str, n: usize) -> String {
    mock_upstream()
        .lock()
        .unwrap()
        .user_agents
        .get(bearer)
        .and_then(|values| values.get(n))
        .cloned()
        .unwrap_or_else(|| panic!("no upstream request recorded for {bearer} at index {n}"))
}

/// A Cline model id the mock serves over Chat Completions.
const CLINE_TEST_MODEL: &str = "cline/anthropic/claude-sonnet-4-6";

fn cline_account(id: &str, token: &str) -> Account {
    // Cline holds a static API key; `TokenManager` hands the refresh token to the backend.
    let mut a = account(id, BackendId::Cline, "unused-access");
    a.refresh_token = Some(token.to_string());
    a
}

fn openai_surfaces() -> std::collections::HashMap<BackendId, Vec<Surface>> {
    std::collections::HashMap::from([
        (BackendId::Codex, vec![Surface::OpenAI]),
        (BackendId::Copilot, vec![Surface::OpenAI]),
        (BackendId::Claude, vec![Surface::Anthropic]),
        (BackendId::Cline, vec![Surface::OpenAI]),
        (BackendId::OpencodeGo, vec![Surface::OpenAI]),
    ])
}

fn backend_surfaces(backends: &BackendMap) -> std::collections::HashMap<BackendId, Vec<Surface>> {
    backends
        .iter()
        .map(|(id, backend)| (*id, backend.surfaces().to_vec()))
        .collect()
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        name: id.into(),
        context: 400_000,
        input: 272_000,
        output: 128_000,
        scope: String::new(),
    }
}

async fn spawn_app_with_user_agent(
    store: Arc<Store>,
    cooldown_ms: i64,
    user_agent: Option<&str>,
) -> (
    String,
    Arc<Mutex<VecDeque<RequestLogEntry>>>,
    Arc<Mutex<PoolCore>>,
) {
    let client = reqwest::Client::new();
    let tokens = Arc::new(TokenManager::new(store.clone(), client.clone()));
    let mut backends: BackendMap = std::collections::HashMap::new();
    backends.insert(BackendId::Codex, Arc::new(CodexBackend { cooldown_ms }));
    backends.insert(BackendId::Copilot, Arc::new(CopilotBackend { cooldown_ms }));
    backends.insert(BackendId::Claude, Arc::new(ClaudeBackend { cooldown_ms }));
    backends.insert(
        BackendId::Cline,
        Arc::new(ClineBackend {
            cooldown_ms,
            user_agent: user_agent.map(str::to_string),
        }),
    );
    backends.insert(
        BackendId::OpencodeGo,
        Arc::new(OpencodeGoBackend {
            cooldown_ms,
            user_agent: user_agent.map(str::to_string),
        }),
    );
    let core = PoolCore::new(&store, backend_surfaces(&backends));
    let pool = Arc::new(Mutex::new(core));
    let logs: Arc<Mutex<VecDeque<RequestLogEntry>>> = Arc::new(Mutex::new(VecDeque::new()));
    let state = Arc::new(AppState {
        store,
        pool: pool.clone(),
        tokens,
        backends: Arc::new(backends),
        client,
        logs: logs.clone(),
        flows: FlowRegistry::default(),
        proxy_key: Some("test-key".to_string()),
        ui_token: "unused".to_string(),
        resets: Arc::new(underclass::resets::ResetManager::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_string(),
            false,
        )),
        stream_usage_unsupported: Mutex::new(Default::default()),
        observations: Mutex::new(Default::default()),
        stats_retention_days: 730,
        cline_user_agent: user_agent.map(str::to_string),
        opencode_go_user_agent: user_agent.map(str::to_string),
    });

    let v1 = axum::Router::new()
        .route("/models", axum::routing::get(underclass::proxy::models))
        .route("/responses", axum::routing::post(underclass::proxy::infer))
        .route(
            "/chat/completions",
            axum::routing::post(underclass::proxy::infer),
        )
        .route(
            "/messages",
            axum::routing::post(underclass::proxy::messages),
        )
        .route(
            "/messages/count_tokens",
            axum::routing::post(underclass::proxy::messages),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            underclass::proxy::require_proxy_key,
        ))
        .with_state(state.clone());

    let anthropic_alias = axum::Router::new()
        .route(
            "/messages",
            axum::routing::post(underclass::proxy::messages),
        )
        .route(
            "/messages/count_tokens",
            axum::routing::post(underclass::proxy::messages),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            underclass::proxy::require_proxy_key,
        ))
        .with_state(state.clone());

    let admin = axum::Router::new()
        .route(
            "/admin/api/state",
            axum::routing::get(underclass::ui::state),
        )
        .route(
            "/admin/api/catalog/{backend}",
            axum::routing::get(underclass::ui::get_catalog).put(underclass::ui::put_catalog),
        )
        .route(
            "/admin/api/catalog/refresh-cline",
            axum::routing::post(underclass::ui::refresh_cline_catalog),
        )
        .route(
            "/admin/api/monitor",
            axum::routing::get(underclass::monitor::snapshot),
        )
        .route(
            "/admin/api/usage",
            axum::routing::get(underclass::ui::usage_summary),
        )
        .route(
            "/admin/api/usage/requests",
            axum::routing::get(underclass::ui::usage_requests),
        )
        .route(
            "/admin/api/stats",
            axum::routing::get(underclass::stats_api::stats_handler),
        )
        .route(
            "/admin/api/accounts/{id}/enable",
            axum::routing::post(underclass::ui::enable_account),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            underclass::ui::require_ui_token,
        ));
    let app = axum::Router::new()
        .nest("/v1", v1)
        .merge(anthropic_alias)
        .merge(admin)
        .layer(axum::middleware::from_fn(
            underclass::correlation::middleware,
        ))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), logs, pool)
}

/// The common case: no configured user-agent override, so the fork's backends resolve one from
/// the client request. Tests that exercise the override call `spawn_app_with_user_agent`.
async fn spawn_app(
    store: Arc<Store>,
    cooldown_ms: i64,
) -> (
    String,
    Arc<Mutex<VecDeque<RequestLogEntry>>>,
    Arc<Mutex<PoolCore>>,
) {
    spawn_app_with_user_agent(store, cooldown_ms, None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_mid_stream_overload_is_recorded() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-5.5")]);
    store.upsert_account(&codex_account("acc-ovl", "tok-overload"));
    let (base, logs, _pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();
    let response = http.post(format!("{base}/v1/responses"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model": "gpt-5.5", "input": "hi", "stream": true, "overload": true}))
        .send().await.unwrap();
    assert_eq!(
        response.status(),
        200,
        "mid-stream failures keep the upstream status"
    );
    assert!(
        response.text().await.unwrap().contains("response.failed"),
        "failure passes through to the client"
    );

    let mut row = None;
    for _ in 0..50 {
        row = store
            .usage_records(&Default::default())
            .unwrap()
            .into_iter()
            .next();
        if row.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let row = row.expect("usage row written when the stream ends");
    assert_eq!(row.stream_outcome.as_deref(), Some("failed"));
    assert_eq!(row.error_code.as_deref(), Some("server_is_overloaded"));
    assert_eq!(row.saw_output, Some(false));
    assert!(row.duration_ms.is_some());
    let entry = logs
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.request_id == row.request_id)
        .cloned()
        .unwrap();
    assert_eq!(entry.stream_outcome.as_deref(), Some("failed"));
    assert_eq!(entry.error_code.as_deref(), Some("server_is_overloaded"));

    // Windows are half-open at `now`; step past the millisecond the event was stamped with.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let stats: serde_json::Value = http
        .get(format!("{base}/admin/api/stats?since=1h&bucket=minute"))
        .header("Authorization", "Bearer unused")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["totals"]["events"]["overloaded"], 1, "{stats}");
    assert_eq!(stats["totals"]["outcomes"]["failed"], 1, "{stats}");
    assert_eq!(stats["totals"]["requests"], 1);
    assert!(
        !stats.to_string().contains("capacity. Please"),
        "upstream message text must not be stored"
    );
    let denied = http
        .get(format!("{base}/admin/api/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_full_pool_story() {
    let mock = mock_upstream();

    let store = Arc::new(Store::in_memory().unwrap());
    let m = model("gpt-5.5");
    store.set_catalog(BackendId::Codex, &[m.clone()]);
    store.set_catalog(BackendId::Copilot, &[m]);
    store.upsert_account(&codex_account("acc-1", "tok-acc-1"));
    store.upsert_account(&copilot_account("acc-2", "cop-acc-2"));
    let (base, logs, _pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    let post = |key: Option<&str>, stream: bool| {
        let http = http.clone();
        let base = base.clone();
        let key = key.map(str::to_string);
        async move {
            let mut body = serde_json::json!({"model": "gpt-5.5", "input": "hi", "stream": stream});
            if let Some(k) = &key {
                body["prompt_cache_key"] = serde_json::Value::String(k.clone());
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            let response = http
                .post(format!("{base}/v1/responses"))
                .header("Authorization", "Bearer test-key")
                .header("x-request-id", &request_id)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.headers()["x-request-id"], request_id);
            response
        }
    };

    // phase 1: success + streaming passthrough + stickiness
    let r1 = post(Some("sess-a"), true).await;
    assert_eq!(r1.status(), 200);
    assert_eq!(
        r1.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
        "text/event-stream"
    );
    let body1 = r1.text().await.unwrap();
    assert!(
        body1.contains("[DONE]"),
        "stream passthrough broken: {body1}"
    );
    assert!(
        body1.contains("hello from tok-acc-1"),
        "unexpected payload: {body1}"
    );

    let r2 = post(Some("sess-a"), true).await;
    let body2 = r2.text().await.unwrap();
    assert!(
        body2.contains("hello from tok-acc-1"),
        "sticky session moved accounts: {body2}"
    );
    let rows = store
        .usage_records(&underclass::store::UsageQuery {
            cache_key: Some("sess-a".into()),
            ..Default::default()
        })
        .unwrap();
    let denied = http
        .get(format!("{base}/admin/api/monitor"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let monitor = http
        .get(format!("{base}/admin/api/monitor"))
        .bearer_auth("unused")
        .send()
        .await
        .unwrap();
    assert_eq!(monitor.status(), 200);
    let monitor_text = monitor.text().await.unwrap();
    assert!(!monitor_text.contains("sess-a"));
    assert!(!monitor_text.contains("tok-acc-1"));
    assert!(!monitor_text.contains("refresh-acc-1"));
    let monitor: serde_json::Value = serde_json::from_str(&monitor_text).unwrap();
    assert_eq!(monitor["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(monitor["month"]["attempts"], 2);
    assert_eq!(monitor["minute_bins"].as_array().unwrap().len(), 60);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.input_tokens == Some(11)
        && r.output_tokens == Some(3)
        && r.account_id == "acc-1"));

    // phase 2: an unsticky request is served by the pool
    let r3 = post(None, false).await;
    assert_eq!(r3.status(), 200);

    // phase 3: quota on the bound account -> transparent failover to the other backend
    mock.lock().unwrap().failing.insert("tok-acc-1".into());
    let r4 = post(Some("sess-a"), true).await;
    assert_eq!(r4.status(), 200, "quota on bound account must fail over");
    let failover_id = r4.headers()["x-request-id"].to_str().unwrap().to_owned();
    let body4 = r4.text().await.unwrap();
    assert!(
        body4.contains("hello from cop-acc-2"),
        "failover should land on the healthy copilot account (bearer = its stored token): {body4}"
    );
    assert!(
        mock.lock()
            .unwrap()
            .fourtwonined
            .contains(&"tok-acc-1".to_string()),
        "mock never saw the 429-causing request for acc-1"
    );
    assert!(
        logs.lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.request_id == failover_id)
            .count()
            >= 2,
        "failed and successful attempts must share the correlation ID in request history"
    );
    let rows = store
        .usage_records(&underclass::store::UsageQuery {
            cache_key: Some("sess-a".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        rows.iter().filter(|r| r.request_id == failover_id).count(),
        2
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.request_id == failover_id && r.account_id == "acc-1")
            .unwrap()
            .input_tokens,
        None
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.request_id == failover_id && r.account_id == "acc-2")
            .unwrap()
            .input_tokens,
        Some(11)
    );

    let summary = http
        .get(format!(
            "{base}/admin/api/usage?from_ms=0&group_by=account_id"
        ))
        .header("Authorization", "Bearer unused")
        .send()
        .await
        .unwrap();
    assert_eq!(summary.status(), 200);
    let groups: serde_json::Value = summary.json().await.unwrap();
    assert!(
        groups["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["account_id"] == "acc-2" && g["input_tokens"].as_i64().unwrap() >= 11)
    );
    let filtered = http.get(format!("{base}/admin/api/usage/requests?from_ms=0&to_ms={}&model=gpt-5.5&account_id=acc-2&cache_key=sess-a", now_ms() + 100_000))
        .header("Authorization", "Bearer unused").send().await.unwrap();
    assert_eq!(filtered.status(), 200);
    let filtered_body: serde_json::Value = filtered.json().await.unwrap();
    let filtered_rows = filtered_body["requests"].as_array().unwrap();
    assert_eq!(filtered_rows.len(), 1);
    assert_eq!(filtered_rows[0]["input_tokens"], 11);
    let forbidden = http
        .get(format!("{base}/admin/api/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 401);
    let forbidden_details = http
        .get(format!("{base}/admin/api/usage/requests"))
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden_details.status(), 401);

    // phase 4: both accounts exhausted -> fail fast with earliest reset
    mock.lock().unwrap().failing.insert("cop-acc-2".into());
    let r5 = post(Some("sess-c"), true).await;
    assert_eq!(r5.status(), 429, "saturation must fail fast");
    let retry_after = r5
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .expect("saturation response missing Retry-After")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        retry_after.parse::<u64>().is_ok(),
        "Retry-After not numeric"
    );

    let accounts = store.list_accounts();
    let cooling: Vec<_> = accounts
        .iter()
        .filter(|a| a.status == AccountStatus::Cooling)
        .collect();
    assert_eq!(
        cooling.len(),
        2,
        "both accounts should be cooling after 429s"
    );

    // phase 5: cooldown expiry restores the pool automatically
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    mock.lock().unwrap().failing.clear();
    let r6 = post(Some("sess-d"), true).await;
    assert_eq!(
        r6.status(),
        200,
        "accounts must return to rotation after cooldown"
    );
    let body6 = r6.text().await.unwrap();
    assert!(
        body6.contains("hello from tok-acc-"),
        "served by a pool account"
    );

    // persisted cooling rows are normalized (what the sync loop / a restart does)
    let mut fresh_core = PoolCore::new(&store, openai_surfaces());
    fresh_core.sync_from_store(&store);
    let accounts = store.list_accounts();
    assert!(
        accounts.iter().all(|a| a.status == AccountStatus::Healthy),
        "stale cooling rows must be normalized in the store: {accounts:?}"
    );

    // phase 6: request log records the serving account
    {
        let log = logs.lock().unwrap();
        let entry = log
            .iter()
            .find(|e| e.sticky == "sess-a")
            .expect("log entry for sess-a");
        assert_eq!(entry.account.as_deref(), Some("acc-1"));
        assert_eq!(entry.backend.as_deref(), Some("codex"));
        assert_eq!(entry.status, 200);
    }

    // phase 7: 401 on one backend fails over and marks auth_error
    let store2 = Arc::new(Store::in_memory().unwrap());
    let m2 = model("m");
    store2.set_catalog(BackendId::Codex, &[m2.clone()]);
    store2.set_catalog(BackendId::Copilot, &[m2]);
    store2.upsert_account(&codex_account("acc-1", "bad-token"));
    store2.upsert_account(&copilot_account("acc-2", "tok-good"));
    let (base2, _logs2, _pool2) = spawn_app(store2.clone(), 400).await;
    let resp = http
        .post(format!("{base2}/v1/chat/completions"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model": "m", "messages": [{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "401 on codex must fail over to copilot account"
    );
    let acc1 = store2.get_account("acc-1").unwrap();
    assert_eq!(
        acc1.status,
        AccountStatus::AuthError,
        "401 must mark auth_error"
    );
    let acc2 = store2.get_account("acc-2").unwrap();
    assert_eq!(acc2.status, AccountStatus::Healthy);
    let streamed_chat = http.post(format!("{base2}/v1/chat/completions"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true,"prompt_cache_key":"chat-session"}))
        .send().await.unwrap();
    assert_eq!(streamed_chat.status(), 200);
    streamed_chat.text().await.unwrap();
    assert_eq!(
        mock.lock()
            .unwrap()
            .chat_usage_options
            .get("tok-good")
            .and_then(|options| options.back())
            .copied(),
        Some(true)
    );
    let chat_rows = store2
        .usage_records(&underclass::store::UsageQuery {
            cache_key: Some("chat-session".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(chat_rows.len(), 1);
    assert_eq!(
        (chat_rows[0].input_tokens, chat_rows[0].output_tokens),
        (Some(11), Some(3))
    );
    mock.lock().unwrap().reject_stream_usage = true;
    let fallback_chat = http.post(format!("{base2}/v1/chat/completions"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true,"prompt_cache_key":"chat-fallback"}))
        .send().await.unwrap();
    assert_eq!(fallback_chat.status(), 200);
    fallback_chat.text().await.unwrap();
    let after_fallback = http.post(format!("{base2}/v1/chat/completions"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true,"prompt_cache_key":"chat-after-fallback"}))
        .send().await.unwrap();
    assert_eq!(after_fallback.status(), 200);
    after_fallback.text().await.unwrap();
    let options = mock
        .lock()
        .unwrap()
        .chat_usage_options
        .get("tok-good")
        .cloned()
        .unwrap_or_default();
    assert!(options.len() >= 4);
    assert_eq!(
        options.iter().rev().take(3).copied().collect::<Vec<_>>(),
        vec![false, false, true]
    );
    let fallback_rows = store2
        .usage_records(&underclass::store::UsageQuery {
            cache_key: Some("chat-fallback".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(fallback_rows.len(), 2);
    assert!(
        fallback_rows
            .iter()
            .any(|r| r.status == 400 && r.input_tokens.is_none())
    );
    assert!(
        fallback_rows
            .iter()
            .any(|r| r.status == 200 && r.input_tokens == Some(11))
    );
    mock.lock().unwrap().reject_stream_usage = false;

    let aborted = http.post(format!("{base2}/v1/responses"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model":"m","input":"hi","stream":true,"prompt_cache_key":"aborted-session","slow_abort":true}))
        .send().await.unwrap();
    assert_eq!(aborted.status(), 200);
    drop(aborted);
    let abort_rows = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let rows = store2
                .usage_records(&underclass::store::UsageQuery {
                    cache_key: Some("aborted-session".into()),
                    ..Default::default()
                })
                .unwrap();
            if !rows.is_empty() {
                break rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("aborted stream was never recorded");
    assert_eq!(abort_rows.len(), 1);
    assert_eq!(
        (abort_rows[0].input_tokens, abort_rows[0].output_tokens),
        (None, None)
    );

    // phase 8: models endpoint + auth enforcement
    let models = http
        .get(format!("{base}/v1/models"))
        .header("Authorization", "Bearer test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let models_body: serde_json::Value = models.json().await.unwrap();
    let ids: Vec<&str> = models_body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"gpt-5.5"));

    let unauth = http
        .post(format!("{base}/v1/responses"))
        .json(&serde_json::json!({"model": "gpt-5.5"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401, "missing proxy key must be rejected");

    store.delete_account("acc-1");
    let history = store
        .usage_records(&underclass::store::UsageQuery {
            account_id: Some("acc-1".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(
        !history.is_empty(),
        "account removal must retain token accounting history"
    );
}

/// A 403 that clears after a token refresh must not escalate the account to `auth_error`:
/// selection never returns an `AuthError` account, so a single redemption-triggered 403 would
/// otherwise strand a perfectly good subscription outside the pool with no way back.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_forbidden_403_refreshes_instead_of_bricking_account() {
    let mock = mock_upstream();
    // Copilot accounts are used deliberately: their `force_refresh` returns the stored token
    // without a network round trip, so this exercises the 403 path hermetically.
    let store = Arc::new(Store::in_memory().unwrap());
    let m = model("m");
    store.set_catalog(BackendId::Codex, &[m.clone()]);
    store.set_catalog(BackendId::Copilot, &[m]);
    store.upsert_account(&copilot_account("cop-403", "cop-403-token"));
    store.upsert_account(&copilot_account("cop-spare", "cop-spare-token"));
    mock.lock()
        .unwrap()
        .forbidden
        .insert("cop-403-token".into(), 1);
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model": "m", "input": "hi", "stream": true}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "403 account body: {body}");
    assert!(
        body.contains("cop-403-token"),
        "the 403 account must be retried after refresh and serve the request, got: {body}"
    );
    assert_eq!(
        store.get_account("cop-403").unwrap().status,
        AccountStatus::Healthy,
        "a 403 that survives one token refresh must not mark the account auth_error"
    );
    assert_eq!(
        mock.lock().unwrap().forbidden.get("cop-403-token"),
        Some(&0)
    );
}

/// `enable` is the operator's recovery lever. It must clear a stranded `auth_error` account, and it
/// must not silently discard an upstream quota deadline that `PoolCore::sweep` would honour.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_enable_clears_auth_error_but_preserves_quota_cooldown() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let m = model("m");
    store.set_catalog(BackendId::Codex, &[m.clone()]);
    store.set_catalog(BackendId::Copilot, &[m]);
    store.upsert_account(&copilot_account("acct-auth", "tok-auth"));
    store.upsert_account(&copilot_account("acct-cooling", "tok-cooling"));
    store.upsert_account(&copilot_account("acct-disabled", "tok-disabled"));
    let (base, _logs, pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    let enable = |id: &str| {
        let http = http.clone();
        let base = base.clone();
        let id = id.to_string();
        async move {
            http.post(format!("{base}/admin/api/accounts/{id}/enable"))
                .header("Authorization", "Bearer unused")
                .send()
                .await
                .unwrap()
        }
    };

    // `enable` reads the pool's live account, so the fixture must move the pool too. A real status
    // transition persists first and syncs the pool; `PoolCore::sync_from_store` does the same on
    // its tick, which this mirrors without waiting for it.
    let set_status = |id: &str, status: AccountStatus, reset_at: i64| {
        store.update_account_status(id, status, reset_at);
        let account = store.get_account(id).unwrap();
        pool.lock().unwrap().sync_account(account);
    };

    assert_eq!(enable("no-such-account").await.status(), 404);

    // A stranded auth_error account is exactly the dead end this endpoint exists to undo.
    set_status("acct-auth", AccountStatus::AuthError, 0);
    assert_eq!(enable("acct-auth").await.status(), 204);
    let authed = store.get_account("acct-auth").unwrap();
    assert_eq!(authed.status, AccountStatus::Healthy);
    assert_eq!(authed.reset_at, 0);

    // An operator-set disable still clears.
    set_status("acct-disabled", AccountStatus::Disabled, 0);
    assert_eq!(enable("acct-disabled").await.status(), 204);
    assert_eq!(
        store.get_account("acct-disabled").unwrap().status,
        AccountStatus::Healthy
    );

    // A quota cooldown is upstream state, not an operator block: enabling must not wipe it,
    // otherwise the account is re-selected while still out of quota.
    let deadline = now_ms() + 3_600_000;
    set_status("acct-cooling", AccountStatus::Cooling, deadline);
    assert_eq!(enable("acct-cooling").await.status(), 204);
    let cooling = store.get_account("acct-cooling").unwrap();
    assert_eq!(cooling.status, AccountStatus::Cooling);
    assert_eq!(
        cooling.reset_at, deadline,
        "enable must preserve an upstream quota deadline"
    );

    // Scoped deadlines follow the same split: an operator disable clears them so the account is
    // fully usable again, while an upstream account-wide cooldown keeps them.
    let mut scoped = store.get_account("acct-disabled").unwrap();
    scoped.scope_cooling.insert("credits".into(), deadline);
    store.update_account_health(&scoped);
    set_status("acct-disabled", AccountStatus::Disabled, 0);
    assert_eq!(enable("acct-disabled").await.status(), 204);
    let cleared = store.get_account("acct-disabled").unwrap();
    assert_eq!(cleared.status, AccountStatus::Healthy);
    assert!(
        cleared.scope_cooling.is_empty(),
        "an operator recovery must clear scoped cooling: {:?}",
        cleared.scope_cooling
    );

    let mut account_wide = store.get_account("acct-cooling").unwrap();
    account_wide
        .scope_cooling
        .insert("credits".into(), deadline);
    store.update_account_health(&account_wide);
    pool.lock().unwrap().sync_account(account_wide);
    assert_eq!(enable("acct-cooling").await.status(), 204);
    let preserved = store.get_account("acct-cooling").unwrap();
    assert_eq!(preserved.status, AccountStatus::Cooling);
    assert_eq!(
        preserved.scope_cooling.get("credits"),
        Some(&deadline),
        "an upstream cooldown must keep its scoped deadlines"
    );
}

/// The reported bug: a credits `402` is an account-level-looking signal on one billing surface.
/// It must cool only that surface, so a ClinePass request through the same key is still attempted
/// instead of failing fast with "all subscriptions are out of quota".

async fn wait_for_rows(store: &Store, count: usize) -> Vec<underclass::store::UsageRecord> {
    for _ in 0..100 {
        let rows = store.usage_records(&Default::default()).unwrap();
        if rows.len() >= count {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    store.usage_records(&Default::default()).unwrap()
}

async fn post_stream(base: &str, key: &str) -> (u16, String) {
    post_model_stream(base, "gpt-5.5", key).await
}

async fn post_model_stream(base: &str, model: &str, key: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({"model": model, "input": "hi", "stream": true, "prompt_cache_key": key}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap())
}

/// Codex refuses a model the account's plan cannot use, naming only that model. The refusal must
/// fail over to another account without cooling the refusing account: that subscription still
/// serves its other models, and an account-wide cooldown would also re-enter the reset manager's
/// cool/re-admit loop, which fetches usage every cycle and re-admits the account on the same
/// refusal.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_unsupported_model_cools_the_model_not_the_account() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-6-astra"), model("gpt-5.5")]);
    // `au-a` sorts first, so a new sticky key binds to the account whose plan refuses the model.
    store.upsert_account(&codex_account("au-a", "tok-unsupported-1"));
    store.upsert_account(&codex_account("au-b", "tok-ok-1"));
    let (base, _logs, pool) = spawn_app(store.clone(), 400).await;

    let (status, body) = post_model_stream(&base, "gpt-6-astra", "unsupported-session").await;
    assert_eq!(status, 200, "the refusal fails over: {body}");
    assert!(
        body.contains("hello from tok-ok-1"),
        "served by the account whose plan supports the model: {body}"
    );
    assert!(
        !body.contains("is not supported when using Codex"),
        "the discarded refusal never reaches the client: {body}"
    );

    let cooled = pool
        .lock()
        .unwrap()
        .model_cooling_until("au-a", "gpt-6-astra");
    assert!(
        cooled.is_some_and(|until| until > underclass::models::now_ms()),
        "the refused model is cooling on that account"
    );
    assert_eq!(
        pool.lock()
            .unwrap()
            .model_cooling_until("au-b", "gpt-6-astra"),
        None,
        "the serving account is not cooled for the model"
    );

    // The refusing subscription is untouched and keeps serving its other models.
    let refusing = store.get_account("au-a").expect("account");
    assert_eq!(
        refusing.status,
        AccountStatus::Healthy,
        "a model-scoped refusal must not cool the account"
    );
    let (status, body) = post_model_stream(&base, "gpt-5.5", "same-account-session").await;
    assert_eq!(status, 200, "the other model still serves: {body}");
    assert!(
        body.contains("hello from tok-unsupported-1"),
        "served by the subscription whose plan refused gpt-6-astra: {body}"
    );
}

/// With no account whose plan supports the model, the refusal is not replayed: the pool reports
/// saturation for that model until the cooldown expires, so a client does not re-hit a permanent
/// plan refusal on every retry.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_unsupported_model_without_an_alternative_fails_fast() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-6-astra")]);
    store.upsert_account(&codex_account("solo", "tok-unsupported-solo"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let (status, body) = post_model_stream(&base, "gpt-6-astra", "solo-session").await;
    assert_eq!(status, 429, "no eligible account left: {body}");
    assert!(
        body.contains("quota_exhausted"),
        "reported as an exhausted model: {body}"
    );
    let account = store.get_account("solo").expect("account");
    assert_eq!(
        account.status,
        AccountStatus::Healthy,
        "the only subscription must stay in rotation for the models its plan supports"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_overload_before_output_fails_over_to_another_account() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-5.5")]);
    // `fo-a` sorts first, so a new sticky key binds to the overloaded account.
    store.upsert_account(&codex_account("fo-a", "tok-ovl-fo-a"));
    store.upsert_account(&codex_account("fo-b", "tok-fo-b"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let (status, body) = post_stream(&base, "failover-session").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("hello from tok-fo-b"),
        "served by the healthy account: {body}"
    );
    assert!(
        !body.contains("server_is_overloaded"),
        "the discarded attempt never reaches the client"
    );

    let rows = wait_for_rows(&store, 2).await;
    let failed = rows
        .iter()
        .find(|r| r.account_id == "fo-a")
        .expect("overloaded attempt recorded");
    assert_eq!(failed.stream_outcome.as_deref(), Some("failed"));
    assert_eq!(failed.error_code.as_deref(), Some("server_is_overloaded"));
    assert_eq!(failed.saw_output, Some(false));
    let served = rows
        .iter()
        .find(|r| r.account_id == "fo-b")
        .expect("retry recorded");
    assert_eq!(
        served.request_id, failed.request_id,
        "one client request, two attempts"
    );

    // The session was rebound, so the next turn goes straight to the healthy account.
    let (_, body) = post_stream(&base, "failover-session").await;
    assert!(body.contains("hello from tok-fo-b"));
    assert_eq!(
        wait_for_rows(&store, 3).await.len(),
        3,
        "no second overloaded attempt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_overload_on_every_account_replays_upstream_error() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-5.5")]);
    store.upsert_account(&codex_account("all-a", "tok-ovl-all-a"));
    store.upsert_account(&codex_account("all-b", "tok-ovl-all-b"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let (status, body) = post_stream(&base, "doomed-session").await;
    assert_eq!(status, 200, "the upstream status is preserved");
    assert!(
        body.contains("server_is_overloaded"),
        "the client sees the upstream error: {body}"
    );
    let rows = wait_for_rows(&store, 2).await;
    let accounts: BTreeSet<_> = rows.iter().map(|r| r.account_id.as_str()).collect();
    assert_eq!(
        accounts,
        BTreeSet::from(["all-a", "all-b"]),
        "each account tried once"
    );
    for account in ["all-a", "all-b"] {
        let healthy = store
            .list_accounts()
            .into_iter()
            .find(|a| a.id == account)
            .unwrap()
            .status;
        assert_eq!(
            healthy,
            AccountStatus::Healthy,
            "an overload must not cool the account"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_overload_after_output_passes_through() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Codex, &[model("gpt-5.5")]);
    store.upsert_account(&codex_account("late-a", "tok-late-ovl-late-a"));
    store.upsert_account(&codex_account("late-b", "tok-late-b"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let (status, body) = post_stream(&base, "late-session").await;
    assert_eq!(status, 200);
    assert!(
        body.contains("partial") && body.contains("server_is_overloaded"),
        "{body}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let rows = wait_for_rows(&store, 1).await;
    assert_eq!(
        rows.len(),
        1,
        "output already reached the client, so no retry"
    );
    assert_eq!(rows[0].saw_output, Some(true));
}

/// The Anthropic surface serves only official Claude Code clients, forwards their headers and
/// body verbatim, and spends the pooled account's credential instead of the client's own.
#[tokio::test]
async fn e2e_anthropic_surface_serves_claude_code_clients_only() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let mut claude = account("claude-1", BackendId::Claude, "claude-token-1");
    claude.refresh_token = Some("claude-refresh-1".into());
    store.upsert_account(&claude);
    let (base, _logs, _pool) = spawn_app(store, 60_000).await;

    // A client that is not Claude Code is rejected before a pooled credential is spent.
    let foreign = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("Authorization", "Bearer test-key")
        .json(&serde_json::json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        foreign.status(),
        403,
        "only Claude Code clients are admitted"
    );
    let spent = mock
        .lock()
        .unwrap()
        .served_by
        .iter()
        .any(|token| token == "claude-token-1");
    assert!(
        !spent,
        "a rejected client must not spend a pooled credential"
    );

    // Claude Code traffic travels upstream with the pooled credential and the client's own
    // headers and body.
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/messages?beta=true"))
        .header("Authorization", "Bearer test-key")
        .header("x-app", "cli")
        .header("user-agent", "claude-cli/2.1.304 (external, sdk-cli)")
        .header(
            "anthropic-beta",
            "oauth-2025-04-20,claude-code-20250219",
        )
        .header("x-claude-code-session-id", "sess-claude")
        .json(&serde_json::json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 16,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.304.c61; cc_entrypoint=sdk-cli;"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["account"], "claude-token-1",
        "the pooled credential replaces the client's own"
    );
    assert_eq!(body["app"], "cli", "client headers travel upstream");
    assert_eq!(body["session"], "sess-claude");
    assert!(
        body["beta"].as_str().unwrap().contains("oauth-2025-04-20"),
        "the oauth beta is present: {}",
        body["beta"]
    );
    assert_eq!(
        body["body"]["system"][0]["text"],
        "x-anthropic-billing-header: cc_version=2.1.304.c61; cc_entrypoint=sdk-cli;",
        "the request body travels upstream unchanged"
    );
}

/// A Claude Code subscription window that is exhausted cools the account until the window
/// resets, and the next Claude account serves the request.
#[tokio::test]
async fn e2e_anthropic_window_quota_cools_the_account() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    for id in ["claude-1", "claude-2"] {
        let token = if id == "claude-1" {
            "claude-quota-1".to_string()
        } else {
            format!("{id}-token")
        };
        let mut claude = account(id, BackendId::Claude, &token);
        claude.refresh_token = Some(format!("{id}-refresh"));
        store.upsert_account(&claude);
    }
    let (base, _logs, _pool) = spawn_app(store.clone(), 60_000).await;

    let request = || async {
        reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("Authorization", "Bearer test-key")
            .header("x-app", "cli")
            .header("user-agent", "claude-cli/2.1.304 (external, sdk-cli)")
            .header(
                "anthropic-beta",
                "oauth-2025-04-20,claude-code-20250219",
            )
            .json(&serde_json::json!({
                "model": "claude-haiku-5-5",
                "max_tokens": 16,
                "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.304.c61; cc_entrypoint=sdk-cli;"}],
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap()
    };

    let first = request().await;
    assert_eq!(
        first.status(),
        200,
        "the second Claude account serves the request"
    );
    let served_healthy = mock
        .lock()
        .unwrap()
        .served_by
        .iter()
        .any(|token| token == "claude-2-token");
    assert!(served_healthy, "the healthy account served the request");

    // The exhausted account is cooling until the window reset the upstream reported.
    let accounts = store.list_accounts();
    let cooling = accounts
        .iter()
        .find(|a| a.id == "claude-1")
        .expect("the exhausted account");
    assert_eq!(cooling.status, AccountStatus::Cooling);
    assert!(
        cooling.reset_at > now_ms(),
        "the cooldown follows the window reset"
    );
}

/// A model-scoped rejection cools that model on the account; the account keeps serving its
/// other models.
#[tokio::test]
async fn e2e_anthropic_model_scoped_limit_leaves_other_models_alone() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let mut claude = account("claude-1", BackendId::Claude, "claude-model-quota-1");
    claude.refresh_token = Some("claude-refresh-1".into());
    store.upsert_account(&claude);
    let (base, _logs, _pool) = spawn_app(store.clone(), 60_000).await;

    let request = |model: &'static str| {
        let base = base.clone();
        async move {
            reqwest::Client::new()
            .post(format!("{base}/v1/messages"))
            .header("Authorization", "Bearer test-key")
            .header("x-app", "cli")
            .header("user-agent", "claude-cli/2.1.304 (external, sdk-cli)")
            .header(
                "anthropic-beta",
                "oauth-2025-04-20,claude-code-20250219",
            )
            .json(&serde_json::json!({
                "model": model,
                "max_tokens": 16,
                "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.304.c61; cc_entrypoint=sdk-cli;"}],
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap()
        }
    };

    let refused = request("claude-opus-5-5").await;
    assert_eq!(
        refused.status(),
        429,
        "the refused model is reported to the client"
    );

    // The account is not cooling as a whole: another model still reaches it.
    let accounts = store.list_accounts();
    let account = accounts.first().expect("the account");
    assert_ne!(
        account.status,
        AccountStatus::Cooling,
        "a model-scoped limit must not cool the account"
    );

    let other = request("claude-sonnet-5-5").await;
    assert_eq!(
        other.status(),
        200,
        "the account still serves its other models"
    );
    let served = mock
        .lock()
        .unwrap()
        .served_by
        .iter()
        .any(|t| t == "claude-model-quota-1");
    assert!(served, "the account served the other model");
}

/// The root-level `/messages` alias serves the same Anthropic surface as `/v1/messages`.
#[tokio::test]
async fn e2e_anthropic_messages_alias_serves_the_same_surface() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let mut claude = account("claude-1", BackendId::Claude, "claude-alias-token");
    claude.refresh_token = Some("claude-refresh-1".into());
    store.upsert_account(&claude);
    let (base, _logs, _pool) = spawn_app(store, 60_000).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/messages"))
        .header("Authorization", "Bearer test-key")
        .header("x-app", "cli")
        .header("user-agent", "claude-cli/2.1.304 (external, sdk-cli)")
        .header("anthropic-beta", "oauth-2025-04-20,claude-code-20250219")
        .json(&serde_json::json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 16,
            "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.304.c61; cc_entrypoint=sdk-cli;"}],
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["account"], "claude-alias-token");
    let served = mock
        .lock()
        .unwrap()
        .served_by
        .iter()
        .any(|token| token == "claude-alias-token");
    assert!(served, "the alias reached the upstream");
}

/// The reported bug: a credits `402` is an account-level-looking signal on one billing surface.
/// It must cool only that surface, so a ClinePass request through the same key is still attempted
/// instead of failing fast with "all subscriptions are out of quota".
#[tokio::test(flavor = "multi_thread")]
async fn e2e_credits_402_keeps_the_clinepass_surface_routable() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    // A test-unique model id keeps the process-wide mock's `payment_required` set from leaking a
    // 402 into a parallel test that requests a shared Cline model.
    let credits = scoped_model("cline/anthropic/claude-sonnet-4-6-e2e-blank", "credits");
    let pass = scoped_model("cline/cline-pass/kimi-k3-e2e-blank", "cline-pass");
    store.set_catalog(BackendId::Cline, &[credits.clone(), pass.clone()]);
    store.upsert_account(&cline_account("cline-1", "key-cline-1"));
    // The mock sees the upstream id: `prepare_body` strips the pool-facing `cline/` prefix.
    let unavailable = underclass::cline::upstream_model_id(&credits.id).to_string();
    mock.lock()
        .unwrap()
        .payment_required
        .insert(unavailable.clone());
    let (base, _logs, _pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    let post = |model: &str| {
        let http = http.clone();
        let base = base.clone();
        let model = model.to_string();
        async move {
            http.post(format!("{base}/v1/chat/completions"))
                .header("Authorization", concat!("Bear", "er ", "test-key"))
                .json(&serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
                .send()
                .await
                .unwrap()
        }
    };

    // 1. Credits is out of balance: the only account cools for that surface, so this fails fast.
    let first = post(&credits.id).await;
    assert_eq!(
        first.status(),
        429,
        "an exhausted surface has no other account to fail over to"
    );
    assert!(
        first
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_some()
    );

    let account = store.get_account("cline-1").unwrap();
    assert_eq!(
        account.status,
        AccountStatus::Healthy,
        "a 402 on credits must not cool the whole Cline account"
    );
    assert!(
        account.scope_cooling.contains_key("credits"),
        "the credits surface must be recorded, got {:?}",
        account.scope_cooling
    );

    // 2. The regression: ClinePass must still be attempted and served.
    let second = post(&pass.id).await;
    let status = second.status();
    let body = second.text().await.unwrap();
    assert_eq!(
        status, 200,
        "ClinePass must still be tried, got {status}: {body}"
    );
    assert!(
        body.contains("key-cline-1"),
        "the request must reach upstream on the same account, got: {body}"
    );
    let attempted = mock.lock().unwrap().models.clone();
    let expected_pass = underclass::cline::upstream_model_id(&pass.id).to_string();
    assert!(
        attempted.contains(&expected_pass),
        "upstream never saw the ClinePass model; attempted: {attempted:?}"
    );

    // 3. The exhausted surface still fails fast, and the monitor names it.
    let third = post(&credits.id).await;
    assert_eq!(third.status(), 429);
    // The quota_cooling event is recorded for the exhausted surface, so statistics agree with
    // routing about which surface stopped serving.
    let stats: serde_json::Value = http
        .get(format!("{base}/admin/api/stats?since=1h&bucket=minute"))
        .header("Authorization", concat!("Bear", "er ", "unused"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The first credits request reached upstream and recorded exactly one quota event; the
    // ClinePass request completed. The third credits request never reached upstream: it was refused
    // by the pool, so it records saturation instead of a second quota event.
    assert_eq!(stats["totals"]["events"]["quota_cooling"], 1, "{stats}");
    assert_eq!(stats["totals"]["outcomes"]["completed"], 1, "{stats}");
    assert!(
        stats["totals"]["events"]["pool_saturated"]
            .as_i64()
            .unwrap_or(0)
            >= 1,
        "the refused credits request must record saturation: {stats}"
    );
    let monitor: serde_json::Value = http
        .get(format!("{base}/admin/api/monitor"))
        .header("Authorization", concat!("Bear", "er ", "unused"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &monitor["accounts"][0];
    assert_eq!(entry["status"], "healthy");
    assert!(
        entry["cooling_scopes"]["credits"].as_i64().unwrap_or(0) > now_ms(),
        "monitor must expose the cooling surface: {entry}"
    );
    assert!(
        entry["cooling_scopes"].get("cline-pass").is_none(),
        "the pass surface was never cooled: {entry}"
    );

    // The admin state view renders remaining time server-side, so a skewed browser clock cannot
    // display a live cooldown as expired.
    let state: serde_json::Value = http
        .get(format!("{base}/admin/api/state"))
        .header("Authorization", concat!("Bear", "er ", "unused"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let scopes = state["accounts"][0]["cooling_scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 1, "{state}");
    assert_eq!(scopes[0]["scope"], "credits");
    assert!(
        scopes[0]["remaining_ms"].as_i64().unwrap() > 0,
        "the view must carry a live server-computed remainder: {state}"
    );

    mock.lock().unwrap().payment_required.remove(&unavailable);
}

/// An operator-edited Cline catalog must not silently drop billing scopes: an empty scope means
/// account-wide quota, which would reintroduce the credits-402 regression for a hand-edited row.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_catalog_put_backfills_cline_scopes() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(
        BackendId::Cline,
        &[scoped_model("cline/anthropic/claude-sonnet-4-6", "credits")],
    );
    store.upsert_account(&cline_account("cline-1", "key-cline-1"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    // A payload that omits `scope` entirely, as an older client or a hand edit would.
    let response = http
        .put(format!("{base}/admin/api/catalog/cline"))
        .header("Authorization", concat!("Bear", "er ", "unused"))
        .json(&serde_json::json!({"models": [
            {"id": "cline/anthropic/claude-sonnet-4-6", "name": "Claude"},
            {"id": "cline/cline-pass/kimi-k3", "name": "Kimi K3"}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let saved: serde_json::Value = response.json().await.unwrap();
    let scopes: Vec<&str> = saved["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["scope"].as_str().unwrap())
        .collect();
    assert_eq!(
        scopes,
        vec!["credits", "cline-pass"],
        "a PUT must not leave Cline rows account-wide: {saved}"
    );
}

/// A rejected key is not a quota signal: it must never be recorded as a cooling surface, or the
/// next request would keep probing an account whose credentials upstream already refused.
/// The account-wide `AuthError` transition itself is covered by `pool::tests` and the backend
/// classification test; this checks the proxy path produces no scope entry.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_cline_auth_failure_never_creates_a_cooling_scope() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let credits = scoped_model("cline/anthropic/claude-sonnet-4-6-auth-e2e", "credits");
    let pass = scoped_model("cline/cline-pass/kimi-k3", "cline-pass");
    store.set_catalog(BackendId::Cline, &[credits.clone(), pass.clone()]);
    // The mock rejects any bearer starting with `bad-`.
    store.upsert_account(&cline_account("cline-bad", "bad-cline-key"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": pass.id, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        200,
        "a rejected key must not serve the request"
    );

    let account = store.get_account("cline-bad").unwrap();
    assert!(
        account.scope_cooling.is_empty(),
        "a 401 is not a quota scope: {:?}",
        account.scope_cooling
    );
    assert_ne!(
        account.status,
        AccountStatus::Cooling,
        "a 401 must not cool the account as if it were out of quota"
    );
}

/// The stream-usage retry must resend the *prepared* body. Cline's `prepare_body` strips the
/// pool-facing `cline/` prefix, so a retry built from the client body would reach upstream still
/// namespaced and be refused - the original attempt would then look like the only one that worked.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_stream_usage_retry_resends_the_prepared_body() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let credits = scoped_model("cline/anthropic/claude-sonnet-4-6-retry-e2e", "credits");
    store.set_catalog(BackendId::Cline, std::slice::from_ref(&credits));
    store.upsert_account(&cline_account("cline-retry", "key-reject-usage"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({
            "model": credits.id,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "the retry must succeed");
    response.text().await.unwrap();

    let (attempted, options) = {
        let mock = mock.lock().unwrap();
        (
            mock.models.clone(),
            mock.chat_usage_options
                .get("key-reject-usage")
                .cloned()
                .unwrap_or_default(),
        )
    };
    // Prove the retry happened at all: the first attempt asked for usage and was rejected, the
    // second went without it. Without this the test would pass on a single successful attempt and
    // would not catch a retry built from the unprepared body.
    assert_eq!(
        options.iter().copied().collect::<Vec<_>>(),
        vec![true, false],
        "expected a rejected attempt then a retry without stream usage"
    );
    assert!(
        attempted.iter().all(|model| !model.starts_with("cline/")),
        "no attempt may reach Cline's upstream with the pool namespace: {attempted:?}"
    );
}

/// A translated stream (Responses client, Cline chat upstream) that fails before its first output
/// must still close with Responses-protocol event frames. The replay handed back when every
/// account overloads is built from held upstream chunks, which are chat-completions SSE until the
/// translator sees them.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_translated_overload_replay_is_a_responses_stream() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let credits = scoped_model("cline/anthropic/claude-sonnet-4-6-ovl-e2e", "credits");
    store.set_catalog(BackendId::Cline, std::slice::from_ref(&credits));
    store.upsert_account(&cline_account("cline-ovl", "key-chat-ovl-1"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/responses"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({
            "model": credits.id,
            "input": "hi",
            "stream": true,
        }))
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    // Every SSE frame the client sees must be a Responses event. A raw chat-completions frame
    // (`data: {"choices":...}` or `[DONE]`) reaching this client is the bug.
    for frame in body.split("\n\n") {
        let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
            continue;
        };
        if data.trim() == "[DONE]" {
            panic!("chat-completions terminator leaked to a Responses client: {body}");
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        assert!(
            value.get("choices").is_none(),
            "chat-completions frame leaked to a Responses client: {data}"
        );
    }
    assert!(
        body.contains("response."),
        "a Responses client must receive Responses events, got: {body}"
    );
    // Without these the test passes on a truncated stream: the translator synthesises a
    // `response.failed` for any EOF, so only the upstream code proves the overload was actually
    // recognised and its attempt replayed rather than merely cut short.
    assert!(
        body.contains("server_is_overloaded"),
        "the upstream capacity code must survive translation: {body}"
    );
    assert!(
        body.contains("response.failed"),
        "an overloaded reply is a failed response: {body}"
    );
}

/// @cc [owner:ghuntley,label:proxy] opencode-go-routes-by-model-family
/// A Go request MUST reach the upstream path its model's family requires, whatever path the
/// client used, and MUST carry a session id. The client's own path is not the family: a
/// Chat-Completions model asked for over `/v1/responses` must still go upstream as chat (and be
/// translated), and a Responses-family model asked for over `/v1/chat/completions` must still go
/// upstream as responses.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_go_routes_each_model_to_its_own_upstream_api() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let chat_model = model("opencode_go/kimi-k3");
    let responses_model = model("opencode_go/gpt-6-luna");
    store.set_catalog(
        BackendId::OpencodeGo,
        &[chat_model.clone(), responses_model.clone()],
    );
    store.upsert_account(&go_account("go-routing", "go-acc-1"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    // A Responses client asking for a Chat-family model: translated, and sent to chat.
    let response = http
        .post(format!("{base}/v1/responses"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({
            "model": chat_model.id,
            "input": "hi",
            "prompt_cache_key": "go-session-1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "the translated request must serve");
    let body = response.text().await.unwrap();
    assert!(
        body.contains("\"object\":\"response\""),
        "a Responses client must get a Responses object: {body}"
    );
    assert!(
        !body.contains("\"choices\""),
        "a chat completion must not leak to a Responses client: {body}"
    );

    // A Chat client asking for a Responses-family model: not translated, sent to responses.
    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": responses_model.id, "messages": []}))
        .send()
        .await
        .unwrap();
    response.text().await.unwrap();

    let (paths, sessions, attempted) = {
        let mock = mock.lock().unwrap();
        (
            mock.go_paths.iter().cloned().collect::<Vec<_>>(),
            mock.go_sessions.iter().cloned().collect::<Vec<_>>(),
            mock.models.iter().cloned().collect::<Vec<_>>(),
        )
    };
    assert!(
        paths
            .iter()
            .all(|path| path.ends_with("/openai/v1/chat/completions")
                || path.ends_with("/openai/v1/responses")),
        "Go requests must use its OpenAI surface: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("/chat/completions")),
        "the chat-family model must go upstream as chat: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.ends_with("/responses")),
        "the Responses-family model must go upstream as responses: {paths:?}"
    );
    assert!(
        attempted.iter().all(|m| !m.starts_with("opencode_go/")),
        "the pool namespace must not reach Go: {attempted:?}"
    );
    assert!(
        sessions.contains(&"go-session-1".to_string()),
        "the sticky key must travel as x-opencode-session: {sessions:?}"
    );
    assert!(
        sessions.iter().all(|s| !s.is_empty()),
        "every request needs a session id or Go refuses it: {sessions:?}"
    );
}

/// @cc [owner:ghuntley,label:pool] opencode-go-session-pinned-to-one-account
/// A session that starts on a Go account MUST keep reaching that same account while it is eligible,
/// so Go's prompt cache stays warm, and MUST only move when that account can no longer serve. A
/// second account in the pool MUST NOT attract the session, however idle it is.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_go_session_stays_pinned_to_its_account() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let model = model("opencode_go/kimi-k3");
    store.set_catalog(BackendId::OpencodeGo, std::slice::from_ref(&model));
    // Two accounts so a non-sticky choice has somewhere else to go.
    store.upsert_account(&go_account("go-pin-a", "go-pin-a"));
    store.upsert_account(&go_account("go-pin-b", "go-pin-b"));
    let (base, _logs, pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    let call = |key: &str| {
        let http = http.clone();
        let base = base.clone();
        let model = model.id.clone();
        let key = key.to_string();
        async move {
            http.post(format!("{base}/v1/chat/completions"))
                .header("Authorization", concat!("Bear", "er ", "test-key"))
                .json(&serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "hi"}],
                    "prompt_cache_key": key,
                }))
                .send()
                .await
                .unwrap()
        }
    };

    // Three turns of one conversation. Only the first is allowed to choose an account; the rest
    // must hit the same one.
    let mut served = Vec::new();
    for _ in 0..3 {
        let response = call("go-session-pin").await;
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        served.push(body["account"].as_str().unwrap().to_string());
    }
    assert_eq!(
        served[0], served[1],
        "the second turn must stay on the account the session started on: {served:?}"
    );
    assert_eq!(
        served[1], served[2],
        "the third turn must stay too: {served:?}"
    );

    // The pin is a recorded binding, not an artefact of there being one usable account: the
    // session's key maps to the account that served it, and a second conversation records its own
    // binding without disturbing the first.
    let bound = {
        let pool = pool.lock().unwrap();
        pool.binding("go-session-pin")
            .expect("a sticky session must be bound")
            .0
    };
    assert_eq!(
        bound, served[0],
        "the binding must name the account the session has been served by"
    );
    assert!(
        pool.lock().unwrap().binding("go-session-other").is_none(),
        "the second conversation has not run yet"
    );

    let other: serde_json::Value = call("go-session-other").await.json().await.unwrap();
    assert!(other["account"].is_string());
    let after = pool.lock().unwrap();
    assert_eq!(
        after.binding("go-session-pin").unwrap().0,
        bound,
        "a second conversation must not steal the first session's binding"
    );
    assert!(
        after.binding("go-session-other").is_some(),
        "the second conversation gets its own binding"
    );
}

/// @cc [owner:ghuntley,label:pool] opencode-go-unavailable-endpoint-leaves-the-account-serving
/// A Go `402` naming an unavailable endpoint MUST cool only the requested model: Go answers a model
/// it cannot currently serve that way, and cooling the account would take every other model out of
/// rotation for one model's outage.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_go_unavailable_endpoint_leaves_the_account_serving() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let unavailable = model("opencode_go/kimi-k3");
    let other = model("opencode_go/glm-5.3");
    store.set_catalog(BackendId::OpencodeGo, &[unavailable.clone(), other.clone()]);
    store.upsert_account(&go_account("opencode-go-endpoint", "go-endpoint-1"));
    let (base, _logs, pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": unavailable.id, "messages": []}))
        .send()
        .await
        .unwrap();
    // With one Go account, the model-scoped cooldown leaves no eligible account for that model, so
    // the pool fails fast. What matters is that the ACCOUNT is untouched: the next assertion shows
    // its other models still serve.
    assert_eq!(
        response.status(),
        429,
        "the pool fails fast for the cooled model"
    );
    response.text().await.unwrap();

    let account = store.get_account("opencode-go-endpoint").unwrap();
    assert_eq!(
        account.status,
        AccountStatus::Healthy,
        "one model's outage must not cool the whole account"
    );
    assert!(
        pool.lock()
            .unwrap()
            .model_cooling_until("opencode-go-endpoint", "opencode_go/kimi-k3")
            .is_some_and(|until| until > now_ms()),
        "the unavailable model must cool"
    );

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": other.id, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "another model on the same account must still serve"
    );
}

/// Go meters usage per model, so a 429 must cool the requested model on that account and leave the
/// rest of its catalog servable.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_go_rate_limit_cools_only_the_model() {
    let _mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    let limited = model("opencode_go/kimi-k3");
    let other = model("opencode_go/glm-5.3");
    store.set_catalog(BackendId::OpencodeGo, &[limited.clone(), other.clone()]);
    store.upsert_account(&go_account("opencode-go-quota", "go-quota-1"));
    let (base, _logs, pool) = spawn_app(store.clone(), 3_600_000).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": limited.id, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429, "the upstream limit passes through");
    response.text().await.unwrap();

    let account = store.get_account("opencode-go-quota").unwrap();
    assert_eq!(
        account.status,
        AccountStatus::Healthy,
        "a per-model limit must not cool the whole account"
    );
    assert!(
        account.scope_cooling.is_empty(),
        "a per-model limit is not a billing surface: {:?}",
        account.scope_cooling
    );
    let cooling = pool
        .lock()
        .unwrap()
        .model_cooling_until("opencode-go-quota", "opencode_go/kimi-k3");
    assert!(
        cooling.is_some_and(|until| until > now_ms()),
        "the limited model must cool"
    );

    // The same account still serves its other models.
    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": other.id, "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "another model on the same account must still serve"
    );
}

/// The fork's own backends present the client's `User-Agent` upstream, so an operator running a
/// real client is not identified as a proxy; the upstream backends keep their own identity, which
/// this test pins so a future change cannot quietly widen the rule.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_fork_backends_forward_the_client_user_agent() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::Cline, &[model(CLINE_TEST_MODEL)]);
    store.set_catalog(BackendId::OpencodeGo, &[model("opencode_go/kimi-k3")]);
    store.set_catalog(BackendId::Codex, &[model("gpt-5.5")]);
    store.upsert_account(&cline_account("cline-ua", "cline-token-ua"));
    store.upsert_account(&go_account("go-ua", "go-token-ua"));
    store.upsert_account(&codex_account("codex-ua", "tok-ua"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    let post = |path: &str, model: &str| {
        let http = http.clone();
        let url = format!("{base}{path}");
        let model = model.to_string();
        async move {
            http.post(url)
                .header("Authorization", concat!("Bear", "er ", "test-key"))
                .header("User-Agent", "opencode/1.17.0")
                .json(&serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
                .send()
                .await
                .unwrap()
        }
    };

    let cline = post("/v1/chat/completions", CLINE_TEST_MODEL).await;
    assert_eq!(cline.status(), 200);
    assert_eq!(
        upstream_user_agent("cline-token-ua", 0),
        "opencode/1.17.0",
        "Cline must present the client's own user agent"
    );

    let go = post("/v1/chat/completions", "opencode_go/kimi-k3").await;
    assert_eq!(go.status(), 200);
    assert_eq!(
        upstream_user_agent("go-token-ua", 0),
        "opencode/1.17.0",
        "OpenCode Go must present the client's own user agent"
    );

    let codex = post("/v1/responses", "gpt-5.5").await;
    assert_eq!(codex.status(), 200);
    assert_eq!(
        upstream_user_agent("tok-ua", 0),
        concat!(
            "opencode/",
            env!("CARGO_PKG_VERSION"),
            " (underclass-pool-proxy)"
        ),
        "Codex keeps upstream's user agent: the client's must not leak in"
    );
}

/// Without a client user agent the fork's backends present the built-in default, so the change
/// above does not leave the header empty when a client sends none.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_fork_backends_default_user_agent_when_the_client_sends_none() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::OpencodeGo, &[model("opencode_go/kimi-k3")]);
    store.upsert_account(&go_account("go-ua-default", "go-token-default"));
    let (base, _logs, _pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": "opencode_go/kimi-k3", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        upstream_user_agent("go-token-default", 0),
        underclass::user_agent::DEFAULT,
        "with no client value the backend must still identify itself"
    );
}

/// The operator's configured override wins over both the client's user agent and the default, and
/// applies to the fork's backends only.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_configured_user_agent_overrides_the_client_value() {
    mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.set_catalog(BackendId::OpencodeGo, &[model("opencode_go/kimi-k3")]);
    store.upsert_account(&go_account("go-ua-override", "go-token-override"));
    let (base, _logs, _pool) =
        spawn_app_with_user_agent(store.clone(), 400, Some("underclass-fleet/2")).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .header("User-Agent", "opencode/1.17.0")
        .json(&serde_json::json!({"model": "opencode_go/kimi-k3", "messages": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        upstream_user_agent("go-token-override", 0),
        "underclass-fleet/2",
        "the configured override must beat the client's own value"
    );
}

/// The regression this closes: `GET /api/v1/models` lists neither Cline's ClinePass models nor its
/// free models, so a catalog built from that listing alone answered "no subscriptions are configured
/// for this model" for a model the subscription serves. A refresh must publish both from the public
/// recommended-models feed, and the pool must then route them.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_catalog_refresh_publishes_feed_only_cline_models() {
    let mock = mock_upstream();
    let store = Arc::new(Store::in_memory().unwrap());
    store.upsert_account(&cline_account("cline-feed", "key-cline-feed"));
    let (base, _logs, pool) = spawn_app(store.clone(), 400).await;
    let http = reqwest::Client::new();

    let response = http
        .post(format!("{base}/admin/api/catalog/refresh-cline"))
        .header("Authorization", concat!("Bear", "er ", "unused"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["refreshed_accounts"], 1,
        "the refresh must succeed: {body}"
    );

    let catalog = store.catalog(BackendId::Cline);
    let find = |id: &str| catalog.iter().find(|m| m.id == id);
    // The listing's own entries survive the refresh.
    assert!(find("cline/anthropic/claude-sonnet-4-6").is_some());
    assert!(
        !find("cline/openai/gpt-5.4:batch").is_some(),
        "batch-only ids stay out of the catalog"
    );
    // The model this issue is about is routable, and bills against the subscription.
    let pass = find("cline/cline-pass/deepseek-v4.1-flash-1017")
        .expect("a feed-only ClinePass model must be published");
    assert_eq!(pass.scope, "cline-pass");
    let free =
        find("cline/reflection/beam-free").expect("a feed-only free model must be published");
    assert_eq!(
        free.scope, "credits",
        "a free model rides usage billing, not the subscription"
    );
    // A comment in the feed is not a model.
    assert!(find("cline/cline-cloud/deepseek-v4.1-flash").is_none());

    // And the published model is actually routable, not just listed.
    let model = "cline/cline-pass/deepseek-v4.1-flash-1017";
    assert!(
        pool.lock().unwrap().serves(BackendId::Cline, model),
        "the pool must serve a model the catalog publishes"
    );
    let served = http
        .post(format!("{base}/v1/chat/completions"))
        .header("Authorization", concat!("Bear", "er ", "test-key"))
        .json(&serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        served.status(),
        200,
        "a published subscription model must serve: {}",
        served.text().await.unwrap_or_default()
    );
    let attempted = mock.lock().unwrap().models.clone();
    assert!(
        attempted.contains(&"cline-pass/deepseek-v4.1-flash-1017".to_string()),
        "upstream must see the id without the pool namespace, got: {attempted:?}"
    );
}
