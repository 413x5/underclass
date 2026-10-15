//! Long-range statistics: the admin query, upstream-event import, and the retention/rollup job.
use crate::models::now_ms;
use crate::proxy::AppState;
use crate::stats::{self, Bucket, Dim, Series, Totals};
use crate::store::{Store, UpstreamEvent, EVENT_KINDS};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

const MAX_IMPORT_BATCH: usize = 1000;
const MAX_ROLLUPS_PER_PASS: usize = 800;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StatsParams {
    /// Window ending at `to_ms` (default now), e.g. `24h`, `90d`, `2y`. Ignored with `from_ms`.
    pub since: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// `minute`, `hour` (default), or `day`.
    pub bucket: Option<String>,
    /// `total` (default), `model`, or `account`.
    pub by: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SeriesOut {
    pub key: String,
    pub label: String,
    pub totals: Totals,
    pub buckets: Vec<Bucket>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsResponse {
    pub from_ms: i64,
    pub to_ms: i64,
    pub bucket: String,
    pub bucket_ms: i64,
    pub by: Dim,
    pub retention_days: i64,
    pub live_inflight: i64,
    pub totals: Totals,
    pub buckets: Vec<Bucket>,
    /// Per-model or per-account series when `by` is not `total`.
    pub series: Vec<SeriesOut>,
}

/// @cc [owner:ghuntley,label:accounting] stats-query-bounds
/// `query` MUST reject windows longer than 2 days for minute buckets, 31 days for hour buckets,
/// and the retention period plus one day for day buckets; buckets MUST align to their size (day
/// buckets to UTC midnight), and day buckets MUST read stored rollups only for finished days, computing unfinished days from raw rows.
/// Window totals MUST use exact distinct request and session counts.
pub fn query(store: &Store, params: &StatsParams, now: i64, live_inflight: i64, retention_days: i64) -> Result<StatsResponse, String> {
    let bucket = params.bucket.as_deref().unwrap_or("hour");
    let (bucket_ms, max_window) = match bucket {
        "minute" => (stats::MINUTE_MS, 2 * stats::DAY_MS),
        "hour" => (stats::HOUR_MS, 31 * stats::DAY_MS),
        "day" => (stats::DAY_MS, (retention_days + 1) * stats::DAY_MS),
        _ => return Err("bucket must be minute, hour, or day".into()),
    };
    let dim = match params.by.as_deref() {
        None => Dim::Total,
        Some(by) => Dim::parse(by).ok_or("by must be total, model, or account")?,
    };
    let to = params.to_ms.unwrap_or(now).min(now);
    let mut from = match (params.from_ms, params.since.as_deref()) {
        (Some(from), _) => from,
        (None, since) => {
            let span = stats::parse_since(since.unwrap_or("24h")).ok_or("since must look like 24h, 90d, or 2y")?;
            to.saturating_sub(span)
        }
    };
    // Align to the bucket size (UTC minute, hour, or midnight) so rows read like a clock.
    from = from.div_euclid(bucket_ms) * bucket_ms;
    if from >= to {
        return Err("window is empty".into());
    }
    if to - from > max_window + bucket_ms {
        return Err(format!(
            "window too long for {bucket} buckets (max {} days); use a coarser bucket",
            max_window / stats::DAY_MS
        ));
    }
    let total = series(store, from, to, bucket_ms, Dim::Total, now).map_err(|e| e.to_string())?;
    let mut totals_out = window_totals(store, from, to, bucket_ms, Dim::Total, &total).map_err(|e| e.to_string())?;
    let (totals, buckets) = match totals_out.pop() {
        Some(series) => (series.totals, series.buckets),
        None => (Totals::default(), empty_buckets(from, to, bucket_ms)),
    };
    let series = if dim == Dim::Total {
        Vec::new()
    } else {
        let by = series(store, from, to, bucket_ms, dim, now).map_err(|e| e.to_string())?;
        window_totals(store, from, to, bucket_ms, dim, &by).map_err(|e| e.to_string())?
    };
    Ok(StatsResponse {
        from_ms: from,
        to_ms: to,
        bucket: bucket.to_string(),
        bucket_ms,
        by: dim,
        retention_days,
        live_inflight,
        totals,
        buckets,
        series,
    })
}

fn empty_buckets(from: i64, to: i64, bucket_ms: i64) -> Vec<Bucket> {
    (0..stats::bucket_count(from, to, bucket_ms))
        .map(|i| Bucket { start_ms: from + i as i64 * bucket_ms, totals: Totals::default() })
        .collect()
}

fn window_totals(store: &Store, from: i64, to: i64, bucket_ms: i64, dim: Dim, series: &[Series]) -> rusqlite::Result<Vec<SeriesOut>> {
    let distinct: HashMap<String, (i64, i64)> = store
        .stats_distinct(from, to, dim)?
        .into_iter()
        .map(|(key, requests, sessions)| (key, (requests, sessions)))
        .collect();
    Ok(series
        .iter()
        .map(|s| {
            let parts: Vec<(&Totals, i64)> = s
                .buckets
                .iter()
                .map(|b| (&b.totals, (b.start_ms + bucket_ms).min(to) - b.start_ms))
                .collect();
            let mut totals = stats::combine(&parts);
            let (requests, sessions) = distinct.get(&s.key).copied().unwrap_or((0, 0));
            totals.requests = requests;
            totals.sessions = sessions;
            SeriesOut { key: s.key.clone(), label: s.label.clone(), totals, buckets: s.buckets.clone() }
        })
        .collect())
}

/// The last UTC day (exclusive) whose rollup is final at `now`.
fn final_day_end(now: i64) -> i64 {
    (now - stats::MAX_ATTEMPT_MS).div_euclid(stats::DAY_MS)
}

fn series(store: &Store, from: i64, to: i64, bucket_ms: i64, dim: Dim, now: i64) -> rusqlite::Result<Vec<Series>> {
    if bucket_ms != stats::DAY_MS {
        return store.stats_series(from, to, bucket_ms, dim);
    }
    let first_day = from.div_euclid(stats::DAY_MS);
    let end_day = (to + stats::DAY_MS - 1).div_euclid(stats::DAY_MS);
    let final_end = final_day_end(now).clamp(first_day, end_day);
    let rolled = store.stats_rolled_days(first_day, final_end)?;
    for day in first_day..final_end {
        if !rolled.contains(&day) {
            store.rollup_stats_day(day, now)?;
        }
    }
    let count = (end_day - first_day) as usize;
    let mut merged: BTreeMap<String, Series> = BTreeMap::new();
    let mut place = |key: String, label: String, day: i64, totals: Totals| {
        let series = merged.entry(key.clone()).or_insert_with(|| Series {
            key,
            label: label.clone(),
            buckets: empty_buckets(from, to, stats::DAY_MS),
        });
        series.label = label;
        if let Some(bucket) = usize::try_from(day - first_day).ok().filter(|i| *i < count) {
            series.buckets[bucket].totals = totals;
        }
    };
    for (day, key, label, totals) in store.stats_daily(first_day, final_end, dim)? {
        if dim == Dim::Total || totals != Totals::default() {
            place(key, label, day, totals);
        }
    }
    for day in final_end..end_day {
        let day_from = day * stats::DAY_MS;
        let day_to = (day_from + stats::DAY_MS).min(to);
        for live in store.stats_series(day_from, day_to, stats::DAY_MS, dim)? {
            if let Some(bucket) = live.buckets.into_iter().next() {
                place(live.key, live.label, day, bucket.totals);
            }
        }
    }
    Ok(merged.into_values().collect())
}

/// @cc [owner:ghuntley,label:persistence] stats-maintenance
/// `maintain` MUST prune everything older than `retention_days` and then roll up every finished
/// UTC day inside the retention window that has data and no stored rollup (bounded per pass).
pub fn maintain(store: &Store, now: i64, retention_days: i64) -> rusqlite::Result<(usize, usize)> {
    let cutoff = now - retention_days * stats::DAY_MS;
    let pruned = store.prune_stats(cutoff)?;
    let Some(earliest) = store.stats_earliest_ts()? else {
        return Ok((pruned, 0));
    };
    let first_day = earliest.max(cutoff).div_euclid(stats::DAY_MS);
    let end = final_day_end(now);
    if first_day >= end {
        return Ok((pruned, 0));
    }
    let rolled = store.stats_rolled_days(first_day, end)?;
    let mut done = 0;
    for day in (first_day..end).filter(|d| !rolled.contains(d)).take(MAX_ROLLUPS_PER_PASS) {
        store.rollup_stats_day(day, now)?;
        done += 1;
    }
    Ok((pruned, done))
}

fn live_inflight(state: &AppState) -> i64 {
    state.pool.lock().unwrap().accounts.values().map(|a| i64::from(a.inflight)).sum()
}

/// @cc [owner:ghuntley,label:security] stats-admin-only
/// Statistics and event import MUST be served only under the admin token gate, and responses
/// MUST NOT include prompt-cache keys, request bodies, or credentials.
pub async fn stats_handler(State(state): State<Arc<AppState>>, Query(params): Query<StatsParams>) -> Response {
    let inflight = live_inflight(&state);
    let retention = state.stats_retention_days;
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || query(&store, &params, now_ms(), inflight, retention)).await;
    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(message)) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"message": message}))).into_response(),
        Err(error) => {
            tracing::error!(error = %error, "stats.query_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "stats query failed").into_response()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportEvent {
    pub ts: i64,
    pub kind: String,
    pub error_code: Option<String>,
    pub model: Option<String>,
    pub dedupe_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportBody {
    pub events: Vec<ImportEvent>,
}

/// @cc [owner:ghuntley,label:accounting] event-import-validated
/// The import endpoint MUST accept at most 1000 events per call, MUST reject unknown kinds, empty
/// or oversized dedupe keys, and timestamps outside the retention window, MUST sanitize codes and
/// model names, and MUST store every accepted event with `source = codex_import`.
pub fn validate_import(body: ImportBody, now: i64, retention_days: i64) -> Result<Vec<UpstreamEvent>, String> {
    if body.events.len() > MAX_IMPORT_BATCH {
        return Err(format!("at most {MAX_IMPORT_BATCH} events per import"));
    }
    let oldest = now - retention_days * stats::DAY_MS;
    body.events
        .into_iter()
        .map(|event| {
            if !EVENT_KINDS.contains(&event.kind.as_str()) {
                return Err(format!("unknown event kind {:?}", event.kind));
            }
            if event.dedupe_key.is_empty() || event.dedupe_key.len() > 200 {
                return Err("dedupe_key must be 1-200 bytes".into());
            }
            if event.ts < oldest || event.ts > now + stats::DAY_MS {
                return Err("event timestamp outside the retention window".into());
            }
            Ok(UpstreamEvent {
                ts: event.ts,
                source: "codex_import".into(),
                kind: event.kind,
                detail: Some("codex_log".into()),
                error_code: event.error_code.as_deref().and_then(crate::usage::sanitize_code),
                model: event.model.as_deref().and_then(crate::usage::sanitize_code),
                dedupe_key: Some(format!("codex:{}", event.dedupe_key)),
                ..Default::default()
            })
        })
        .collect()
}

pub async fn import_handler(State(state): State<Arc<AppState>>, Json(body): Json<ImportBody>) -> Response {
    let received = body.events.len();
    let events = match validate_import(body, now_ms(), state.stats_retention_days) {
        Ok(events) => events,
        Err(message) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"message": message}))).into_response(),
    };
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || store.insert_upstream_events(&events)).await {
        Ok(Ok(inserted)) => {
            tracing::info!(received, inserted, "stats.events_imported");
            Json(serde_json::json!({"received": received, "inserted": inserted})).into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "import failed").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::UsageRecord;

    fn row(request: &str, ts: i64, duration: i64, model: &str, outcome: Option<&str>) -> UsageRecord {
        UsageRecord {
            request_id: request.into(), ts, endpoint: "/v1/responses".into(), backend: "codex".into(),
            model: model.into(), account_id: "acc-1".into(), account_label: "alice".into(),
            cache_key: Some(format!("session-{request}")), status: 200, input_tokens: Some(10), output_tokens: Some(2),
            duration_ms: Some(duration), stream_outcome: outcome.map(str::to_string), ..Default::default()
        }
    }

    #[test]
    fn day_query_mixes_rollups_and_live_days() {
        let store = Store::in_memory().unwrap();
        let day = stats::DAY_MS;
        store.insert_usage(&row("a", day + 1000, 5000, "m1", Some("completed"))).unwrap();
        store.insert_usage(&row("b", day + 2000, 5000, "m2", Some("failed"))).unwrap();
        store.insert_usage(&row("c", 3 * day + 10, 10, "m1", Some("completed"))).unwrap();
        store.insert_upstream_event(&UpstreamEvent { ts: day + 2500, source: "proxy".into(), kind: "overloaded".into(), model: Some("m2".into()), ..Default::default() });
        let now = 3 * day + 100;
        let params = StatsParams { since: Some("3d".into()), bucket: Some("day".into()), by: Some("model".into()), ..Default::default() };
        let response = query(&store, &params, now, 0, 730).unwrap();
        // `since` is floored to UTC midnight, so the window covers days 0..=3.
        assert_eq!(response.buckets.len(), 4);
        assert_eq!(response.totals.requests, 3);
        assert_eq!(response.totals.sessions, 3);
        assert_eq!(response.totals.input_tokens, 30);
        assert_eq!(response.totals.peak_concurrency, 2);
        assert_eq!(response.totals.events.get("overloaded"), Some(&1));
        assert_eq!(response.totals.outcomes.get("failed"), Some(&1));
        assert_eq!(response.buckets[1].totals.attempts, 2);
        assert_eq!(response.buckets[3].totals.attempts, 1);
        // Day 1 is final and now stored; repeating the query reads the stored rollup.
        assert!(store.stats_rolled_days(1, 2).unwrap().contains(&1));
        let again = query(&store, &params, now, 0, 730).unwrap();
        assert_eq!(again.totals, response.totals);
        let m2 = response.series.iter().find(|s| s.key == "m2").unwrap();
        assert_eq!((m2.totals.attempts, m2.totals.events.get("overloaded")), (1, Some(&1)));
    }

    #[test]
    fn query_rejects_oversized_windows() {
        let store = Store::in_memory().unwrap();
        let params = StatsParams { since: Some("40d".into()), bucket: Some("hour".into()), ..Default::default() };
        assert!(query(&store, &params, 100 * stats::DAY_MS, 0, 730).is_err());
        let params = StatsParams { since: Some("2y".into()), bucket: Some("day".into()), ..Default::default() };
        assert!(query(&store, &params, 1000 * stats::DAY_MS, 0, 730).is_ok());
    }

    #[test]
    fn maintain_prunes_and_rolls_up() {
        let store = Store::in_memory().unwrap();
        let day = stats::DAY_MS;
        store.insert_usage(&row("old", 10, 1, "m", None)).unwrap();
        store.insert_usage(&row("new", 5 * day, 1, "m", None)).unwrap();
        let now = 7 * day;
        let (pruned, rolled) = maintain(&store, now, 3).unwrap();
        // Retention cuts at day 4; day 5 is final, day 6 is still within the grace period.
        assert_eq!(pruned, 1);
        assert_eq!(rolled, 1);
        assert_eq!(maintain(&store, now, 3).unwrap(), (0, 0));
    }

    #[test]
    fn import_is_validated_and_idempotent() {
        let store = Store::in_memory().unwrap();
        let now = 10 * stats::DAY_MS;
        let body = || ImportBody { events: vec![ImportEvent { ts: now - 1000, kind: "overloaded".into(), error_code: Some("server overloaded!".into()), model: Some("gpt-6".into()), dedupe_key: "s1:4".into() }] };
        let events = validate_import(body(), now, 730).unwrap();
        assert_eq!(events[0].error_code.as_deref(), Some("serveroverloaded"));
        assert_eq!(store.insert_upstream_events(&events).unwrap(), 1);
        assert_eq!(store.insert_upstream_events(&validate_import(body(), now, 730).unwrap()).unwrap(), 0);
        let bad = ImportBody { events: vec![ImportEvent { kind: "nope".into(), ..body().events[0].clone() }] };
        assert!(validate_import(bad, now, 730).is_err());
    }
}
