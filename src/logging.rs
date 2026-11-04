use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    Auto,
    Json,
    Pretty,
}

pub fn init(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let use_json = match format {
        LogFormat::Json => true,
        LogFormat::Pretty => false,
        LogFormat::Auto => !is_tty(),
    };
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if use_json {
        builder.json().init();
    } else {
        builder.pretty().init();
    }
}

fn is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

pub fn log_request_selected(
    request_id: &str,
    model: &str,
    decision: crate::pool::Decision,
    backend: &str,
    account_id: &str,
    label: Option<&str>,
) {
    tracing::info!(
        request_id = %request_id,
        model = %model,
        decision = ?decision,
        backend = %backend,
        account = %label.unwrap_or("unknown"),
        account_id = %truncate(account_id),
        "request.selected"
    );
}

pub fn log_request_completed(request_id: &str, status: u16, duration_ms: u64) {
    tracing::info!(
        request_id = %request_id,
        status = status,
        duration_ms = duration_ms,
        "request.completed"
    );
}

/// One attempt began waiting for the upstream's response headers. This is the start of the clock
/// an attempt that never gets headers would otherwise never have: without it, a request that is
/// selected and then produces nothing leaves one `request.selected` line and no end.
///
/// `attempt` is the attempt's 1-based position in this request's failover chain, and `cache_key`
/// is the client's own session/prompt-cache key - the identity the proxy already uses to keep a
/// session sticky, and the only thing that joins separate requests belonging to one logical call.
#[allow(clippy::too_many_arguments)]
pub fn log_request_awaiting_headers(
    request_id: &str,
    attempt: u32,
    cache_key: &str,
    model: &str,
    upstream_model: &str,
    endpoint: &str,
    backend: &str,
    account_id: &str,
    label: Option<&str>,
) {
    tracing::info!(
        request_id = %request_id,
        attempt = attempt,
        cache_key = %cache_key,
        model = %model,
        upstream_model = %upstream_model,
        endpoint = %endpoint,
        backend = %backend,
        account = %label.unwrap_or("unknown"),
        account_id = %truncate(account_id),
        "request.awaiting_headers"
    );
}

/// The one terminal line for an attempt that did not end by its upstream response. Emitted from a
/// `Drop` guard, so it fires whether the attempt returned, errored, or was abandoned mid-flight.
/// `stage` says where the attempt was, `reason` which of the indistinguishable cases it hit, and
/// `elapsed_ms` how long it had been running. `reason` is a fixed label, never upstream text.
#[allow(clippy::too_many_arguments)]
pub fn log_request_abandoned(
    request_id: &str,
    attempt: u32,
    cache_key: &str,
    model: &str,
    upstream_model: &str,
    endpoint: &str,
    backend: &str,
    account_id: &str,
    label: Option<&str>,
    stage: &str,
    reason: &str,
    elapsed_ms: u64,
) {
    tracing::warn!(
        request_id = %request_id,
        attempt = attempt,
        cache_key = %cache_key,
        model = %model,
        upstream_model = %upstream_model,
        endpoint = %endpoint,
        backend = %backend,
        account = %label.unwrap_or("unknown"),
        account_id = %truncate(account_id),
        stage = %stage,
        reason = %reason,
        elapsed_ms = elapsed_ms,
        "request.abandoned",
    );
}

/// The configured response-header bound expired before the upstream sent its response head. This
/// is the proxy's own decision rather than an observation about a request nobody finished, so it
/// carries its own event name instead of `request.abandoned`.
#[allow(clippy::too_many_arguments)]
pub fn log_upstream_headers_timeout(
    request_id: &str,
    attempt: u32,
    cache_key: &str,
    backend: &str,
    account_id: &str,
    label: Option<&str>,
    endpoint: &str,
    stage: &str,
    elapsed_ms: u64,
    limit_ms: u64,
) {
    tracing::warn!(
        request_id = %request_id,
        attempt = attempt,
        cache_key = %cache_key,
        backend = %backend,
        account = %label.unwrap_or("unknown"),
        account_id = %truncate(account_id),
        endpoint = %endpoint,
        stage = %stage,
        elapsed_ms = elapsed_ms,
        limit_ms = limit_ms,
        "upstream.headers_timeout"
    );
}

pub fn log_account_state(account_id: &str, label: Option<&str>, event: &str, detail: &str) {
    tracing::info!(
        account = %label.unwrap_or("unknown"),
        account_id = %truncate(account_id),
        detail = %detail,
        "account.{event}"
    );
}

/// Catalog refresh outcome for one account. Identity is logged as a label plus a truncated id so
/// raw UUIDs never reach the log.
pub fn log_catalog_refresh(
    account: &crate::models::Account,
    models: Option<usize>,
    error: Option<&str>,
) {
    match (models, error) {
        (Some(models), _) => tracing::info!(
            account = %account.label,
            account_id = %truncate(&account.id),
            models = models,
            "catalog.refreshed"
        ),
        (None, Some(error)) => tracing::warn!(
            account = %account.label,
            account_id = %truncate(&account.id),
            error = %error,
            "catalog.refresh_failed"
        ),
        (None, None) => {}
    }
}

/// One line per finished response body. Failed, incomplete, or truncated bodies log at warn so
/// mid-stream upstream failures (e.g. `server_is_overloaded`) are visible despite the 200 status.
pub fn log_stream_finished(
    request_id: &str,
    model: &str,
    label: &str,
    outcome: &str,
    error_code: Option<&str>,
    saw_output: bool,
    duration_ms: i64,
) {
    if matches!(
        outcome,
        "failed" | "incomplete" | "truncated" | "upstream_error"
    ) {
        tracing::warn!(
            request_id = %request_id,
            model = %model,
            account = %label,
            outcome = %outcome,
            error_code = error_code.unwrap_or("none"),
            saw_output = saw_output,
            duration_ms = duration_ms,
            "stream.finished"
        );
    } else {
        tracing::info!(
            request_id = %request_id,
            model = %model,
            account = %label,
            outcome = %outcome,
            saw_output = saw_output,
            duration_ms = duration_ms,
            "stream.finished"
        );
    }
}

/// An upstream stream failed with a capacity code before any output, so the request is retried on
/// another account instead of being passed through.
pub fn log_overload_failover(
    request_id: &str,
    model: &str,
    account_id: &str,
    label: &str,
    error_code: &str,
    held_ms: u64,
) {
    tracing::warn!(
        request_id = %request_id,
        model = %model,
        account = %label,
        account_id = %truncate(account_id),
        error_code = %error_code,
        held_ms = held_ms,
        "request.overload_failover"
    );
}

/// An upstream failure after the response body started. The client is told through a terminal
/// error event; this line is the operator's record of why. `reason` is a fixed label
/// (`connection_failed` or `idle`), never upstream message text.
pub fn log_upstream_body_failure(request_id: &str, backend: &str, reason: &str) {
    tracing::warn!(
        request_id = %request_id,
        backend = %backend,
        reason = %reason,
        "stream.upstream_failure"
    );
}

/// `until_ms` is the absolute epoch-millisecond pool reset; `retry_after_ms` is the relative wait.
pub fn log_saturated(request_id: &str, until_ms: i64, now_ms: i64) {
    tracing::warn!(
        request_id = %request_id,
        retry_after_ms = until_ms.saturating_sub(now_ms).max(0),
        until_ms = until_ms,
        "request.saturated"
    );
}

pub fn log_reset_decision(
    request_id: &str,
    reason: &str,
    cooling_accounts: usize,
    selected_wait_ms: Option<i64>,
) {
    tracing::info!(
        request_id = %request_id,
        reason = %reason,
        cooling_accounts,
        selected_wait_ms,
        "codex.reset.decision"
    );
}

pub fn log_reset_account(
    request_id: &str,
    account_id: &str,
    label: &str,
    outcome: &str,
    natural_wait_ms: i64,
) {
    tracing::info!(
        request_id = %request_id,
        account = %label,
        account_id = %truncate(account_id),
        outcome = %outcome,
        natural_wait_ms,
        "codex.reset.account"
    );
}

pub fn log_reset_fetch(account_id: &str, label: &str, reason: &str) {
    tracing::warn!(
        account = %label,
        account_id = %truncate(account_id),
        reason = %reason,
        "codex.reset.usage_unavailable"
    );
}

pub fn log_token_rotation(account_id: &str, label: &str, event: &str, detail: &str) {
    tracing::info!(
        account = %label,
        account_id = %truncate(account_id),
        detail = %detail,
        "token.{event}"
    );
}

fn truncate(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        id[..8].to_string()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncates_ids_but_not_labels() {
        assert_eq!(super::truncate("abcdefghij"), "abcdefgh");
        assert_eq!(super::truncate("abc"), "abc");
    }
}
