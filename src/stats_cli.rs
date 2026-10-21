//! `underclass stats`: print usage/capacity statistics and backfill events from Codex logs.
use crate::stats::{self, DAY_MS};
use crate::stats_api::{ImportBody, ImportEvent};
use clap::{Args, Subcommand};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Args, Debug)]
pub struct StatsArgs {
    #[command(subcommand)]
    pub command: Option<StatsCommand>,
    #[arg(long, default_value = "24h", help = "window, e.g. 6h, 7d, 90d, 2y")]
    pub since: String,
    #[arg(
        long,
        help = "minute, hour, or day (default: hour up to 2 days, else day)"
    )]
    pub bucket: Option<String>,
    #[arg(long, default_value = "total", help = "total, model, or account")]
    pub by: String,
    #[arg(long, help = "print the raw JSON response")]
    pub json: bool,
    #[arg(
        long,
        global = true,
        help = "server URL (requires UNDERCLASS_UI_TOKEN)"
    )]
    pub url: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum StatsCommand {
    /// backfill capacity, quota, and policy errors recorded in Codex session logs
    ImportCodex(ImportCodexArgs),
}

#[derive(Args, Debug)]
pub struct ImportCodexArgs {
    #[arg(
        long,
        help = "rollout directory (repeatable; default: $CODEX_HOME/sessions and archived_sessions)"
    )]
    pub dir: Vec<PathBuf>,
    #[arg(long, help = "parse and summarize without sending anything")]
    pub dry_run: bool,
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))
}

fn error_message(response: reqwest::blocking::Response) -> String {
    let status = response.status().as_u16();
    let body: Value = response.json().unwrap_or(Value::Null);
    let message = body["message"].as_str().unwrap_or("request failed");
    format!("{message} (HTTP {status})")
}

/// @cc [owner:ghuntley,label:cli] stats-cli-via-admin-api
/// `underclass stats` MUST act only through the running server's authenticated admin API and
/// MUST NOT open the local database; `import-codex` MUST send only timestamps, event kinds,
/// sanitized error codes, model names, and dedupe keys, never prompts or message text.
pub fn run(args: &StatsArgs) -> Result<(), String> {
    match &args.command {
        Some(StatsCommand::ImportCodex(import)) => import_codex(import, args.url.clone()),
        None => show(args),
    }
}

fn show(args: &StatsArgs) -> Result<(), String> {
    let span = stats::parse_since(&args.since).ok_or("--since must look like 24h, 90d, or 2y")?;
    let bucket = args.bucket.clone().unwrap_or_else(|| {
        if span <= 2 * DAY_MS {
            "hour".into()
        } else {
            "day".into()
        }
    });
    let config = crate::config::Config::load();
    let (url, token) = crate::top::admin_http_target(&config, args.url.clone())?;
    let endpoint = reqwest::Url::parse_with_params(
        &format!("{url}/admin/api/stats"),
        [
            ("since", args.since.as_str()),
            ("bucket", bucket.as_str()),
            ("by", args.by.as_str()),
        ],
    )
    .map_err(|e| format!("stats url: {e}"))?;
    let response = client()?
        .get(endpoint)
        .bearer_auth(&token)
        .send()
        .map_err(|e| format!("cannot reach {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(error_message(response));
    }
    let body: Value = response
        .json()
        .map_err(|e| format!("stats response: {e}"))?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
    } else {
        print!("{}", render(&body));
    }
    Ok(())
}

fn human(n: i64) -> String {
    let f = n as f64;
    match n.abs() {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", f / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1}M", f / 1e6),
        _ => format!("{:.2}B", f / 1e9),
    }
}

fn count(map: &Value, key: &str) -> i64 {
    map.get(key).and_then(Value::as_i64).unwrap_or(0)
}

const HEADER: [&str; 13] = [
    "",
    "requests",
    "sessions",
    "tok in",
    "tok out",
    "peak",
    "mean",
    "failed",
    "overload",
    "policy",
    "quota",
    "cooling",
    "saturated",
];

fn row(name: String, t: &Value) -> Vec<String> {
    let outcomes = &t["outcomes"];
    let events = &t["events"];
    vec![
        name,
        count(t, "requests").to_string(),
        count(t, "sessions").to_string(),
        human(count(t, "input_tokens")),
        human(count(t, "output_tokens")),
        count(t, "peak_concurrency").to_string(),
        format!("{:.1}", count(t, "mean_concurrency_milli") as f64 / 1000.0),
        (count(outcomes, "failed") + count(outcomes, "incomplete") + count(outcomes, "truncated"))
            .to_string(),
        count(events, "overloaded").to_string(),
        count(events, "policy_blocked").to_string(),
        count(events, "quota_exhausted").to_string(),
        count(events, "quota_cooling").to_string(),
        count(events, "pool_saturated").to_string(),
    ]
}

fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..HEADER.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].len())
                .chain([HEADER[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i == 0 {
                    format!("{c:<w$}", w = widths[i])
                } else {
                    format!("{c:>w$}", w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = line(HEADER.to_vec()) + "\n";
    for r in rows {
        out += &(line(r.iter().map(String::as_str).collect()) + "\n");
    }
    out
}

fn label_time(ms: i64, bucket: &str) -> String {
    let Some(t) = chrono::DateTime::from_timestamp_millis(ms) else {
        return ms.to_string();
    };
    match bucket {
        "day" => t.format("%Y-%m-%d").to_string(),
        _ => t.format("%Y-%m-%d %H:%M").to_string(),
    }
}

/// Renders a `/admin/api/stats` response as a plain-text table (UTC times).
pub fn render(body: &Value) -> String {
    let bucket = body["bucket"].as_str().unwrap_or("hour");
    let from = label_time(body["from_ms"].as_i64().unwrap_or(0), "hour");
    let to = label_time(body["to_ms"].as_i64().unwrap_or(0), "hour");
    let mut out = format!(
        "{from} → {to} UTC · {bucket} buckets · {} in flight now · retention {} days\n\n",
        body["live_inflight"].as_i64().unwrap_or(0),
        body["retention_days"].as_i64().unwrap_or(0),
    );
    let series = body["series"].as_array().cloned().unwrap_or_default();
    let mut rows: Vec<Vec<String>> = if series.is_empty() {
        body["buckets"]
            .as_array()
            .map(|buckets| {
                buckets
                    .iter()
                    .map(|b| row(label_time(b["start_ms"].as_i64().unwrap_or(0), bucket), b))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        series
            .iter()
            .map(|s| row(s["label"].as_str().unwrap_or("?").to_string(), &s["totals"]))
            .collect()
    };
    rows.push(row("total".into(), &body["totals"]));
    out += &table(&rows);
    let extra: Vec<String> = body["totals"]["outcomes"]
        .as_object()
        .map(|o| o.iter().map(|(k, v)| format!("{k}={v}")).collect())
        .unwrap_or_default();
    if !extra.is_empty() {
        out += &format!("\noutcomes: {}\n", extra.join(" "));
    }
    let events: Vec<String> = body["totals"]["events"]
        .as_object()
        .map(|o| o.iter().map(|(k, v)| format!("{k}={v}")).collect())
        .unwrap_or_default();
    if !events.is_empty() {
        out += &format!("events: {}\n", events.join(" "));
    }
    out
}

/// Maps a Codex `codex_error_info` value to an upstream event kind and error code. Errors that
/// say nothing about capacity, quota, or policy (sandbox, context-window, ...) are skipped.
fn codex_error_kind(info: &Value) -> Option<(&'static str, String)> {
    let (name, status) = match info {
        Value::String(name) => (name.as_str(), None),
        Value::Object(map) if map.len() == 1 => {
            let (name, inner) = map.iter().next()?;
            (
                name.as_str(),
                inner.get("http_status_code").and_then(Value::as_i64),
            )
        }
        _ => return None,
    };
    let kind = match name {
        "server_overloaded" => "overloaded",
        "usage_limit_exceeded" => "quota_exhausted",
        "response_too_many_failed_attempts" => "retries_exhausted",
        "http_connection_failed"
        | "response_stream_disconnected"
        | "internal_server_error"
        | "other" => "upstream_failed",
        name if name.contains("policy") => "policy_blocked",
        _ => return None,
    };
    let code = match status {
        Some(status) => format!("{name}.http_{status}"),
        None => name.to_string(),
    };
    Some((kind, crate::usage::sanitize_code(&code)?))
}

/// @cc [owner:ghuntley,label:cli] codex-rollout-parse
/// `codex_events_from_rollout` MUST emit one event per `task_complete` (or `error`) record whose
/// error carries a mapped `codex_error_info`, keyed `session:ordinal` (falling back to the line
/// number), with the model of the latest preceding `turn_context`, and MUST read no other fields.
pub fn codex_events_from_rollout(text: &str, fallback_session: &str) -> Vec<ImportEvent> {
    let mut session = fallback_session.to_string();
    let mut model: Option<String> = None;
    let mut out = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if !(line.contains("session_meta")
            || line.contains("turn_context")
            || line.contains("codex_error_info"))
        {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = &record["payload"];
        match record["type"].as_str() {
            Some("session_meta") => {
                if let Some(id) = payload["id"].as_str() {
                    session = id.to_string();
                }
            }
            Some("turn_context") => {
                model = payload["model"].as_str().map(str::to_string);
            }
            Some("event_msg")
                if matches!(payload["type"].as_str(), Some("task_complete" | "error")) =>
            {
                let info = payload["error"]
                    .get("codex_error_info")
                    .or_else(|| payload.get("codex_error_info"));
                let Some((kind, code)) = info.and_then(codex_error_kind) else {
                    continue;
                };
                let Some(ts) = record["timestamp"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.timestamp_millis())
                else {
                    continue;
                };
                let ordinal = record["ordinal"].as_i64().unwrap_or(line_no as i64);
                out.push(ImportEvent {
                    ts,
                    kind: kind.to_string(),
                    error_code: Some(code),
                    model: model.clone(),
                    dedupe_key: format!("{session}:{ordinal}"),
                });
            }
            _ => {}
        }
    }
    out
}

fn rollout_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            rollout_files(&path, out);
        } else if kind.is_file()
            && path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
        {
            out.push(path);
        }
    }
}

fn import_codex(args: &ImportCodexArgs, url: Option<String>) -> Result<(), String> {
    let dirs = if args.dir.is_empty() {
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
            .ok_or("cannot locate the Codex home; pass --dir")?;
        vec![home.join("sessions"), home.join("archived_sessions")]
    } else {
        args.dir.clone()
    };
    let mut files = Vec::new();
    for dir in &dirs {
        rollout_files(dir, &mut files);
    }
    files.sort();
    let mut events = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let fallback = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rollout");
        events.extend(codex_events_from_rollout(&text, fallback));
    }
    let mut by_kind: BTreeMap<(String, String), i64> = BTreeMap::new();
    for event in &events {
        *by_kind
            .entry((
                event.kind.clone(),
                event.error_code.clone().unwrap_or_default(),
            ))
            .or_default() += 1;
    }
    let first = events.iter().map(|e| e.ts).min();
    let last = events.iter().map(|e| e.ts).max();
    println!(
        "{} rollout files, {} events{}",
        files.len(),
        events.len(),
        match (first, last) {
            (Some(a), Some(b)) =>
                format!(" from {} to {}", label_time(a, "day"), label_time(b, "day")),
            _ => String::new(),
        }
    );
    for ((kind, code), n) in &by_kind {
        println!("  {n:>6}  {kind:<18} {code}");
    }
    if args.dry_run || events.is_empty() {
        return Ok(());
    }
    let config = crate::config::Config::load();
    let (url, token) = crate::top::admin_http_target(&config, url)?;
    let client = client()?;
    let mut inserted = 0;
    for chunk in events.chunks(1000) {
        let response = client
            .post(format!("{url}/admin/api/events/import"))
            .bearer_auth(&token)
            .json(&ImportBody {
                events: chunk.to_vec(),
            })
            .send()
            .map_err(|e| format!("cannot reach {url}: {e}"))?;
        if !response.status().is_success() {
            return Err(error_message(response));
        }
        let body: Value = response
            .json()
            .map_err(|e| format!("import response: {e}"))?;
        inserted += body["inserted"].as_i64().unwrap_or(0);
    }
    println!(
        "imported {inserted} new events ({} already present)",
        events.len() as i64 - inserted
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_events_carry_model_session_and_kind() {
        let text = [
            r#"{"timestamp":"2026-10-15T05:45:54Z","type":"session_meta","payload":{"id":"sess-1","base_instructions":"secret prompt"}}"#,
            r#"{"timestamp":"2026-10-15T05:46:00Z","ordinal":3,"type":"turn_context","payload":{"model":"gpt-6-astra"}}"#,
            r#"{"timestamp":"2026-10-15T05:49:31.503Z","ordinal":23,"type":"event_msg","payload":{"type":"task_complete","error":{"message":"Selected model is at capacity.","codex_error_info":"server_overloaded"}}}"#,
            r#"{"timestamp":"2026-10-15T05:50:00Z","ordinal":24,"type":"event_msg","payload":{"type":"task_complete","error":{"message":"blocked","codex_error_info":"cyber_policy"}}}"#,
            r#"{"timestamp":"2026-10-15T05:51:00Z","ordinal":25,"type":"event_msg","payload":{"type":"task_complete","error":{"message":"x","codex_error_info":{"response_too_many_failed_attempts":{"http_status_code":429}}}}}"#,
            r#"{"timestamp":"2026-10-15T05:52:00Z","ordinal":26,"type":"event_msg","payload":{"type":"task_complete","error":{"message":"x","codex_error_info":"sandbox_error"}}}"#,
            r#"{"timestamp":"2026-10-15T05:53:00Z","ordinal":27,"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}"#,
        ]
        .join("\n");
        let events = codex_events_from_rollout(&text, "fallback");
        let summary: Vec<(&str, Option<&str>, Option<&str>, &str)> = events
            .iter()
            .map(|e| {
                (
                    e.kind.as_str(),
                    e.error_code.as_deref(),
                    e.model.as_deref(),
                    e.dedupe_key.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "overloaded",
                    Some("server_overloaded"),
                    Some("gpt-6-astra"),
                    "sess-1:23"
                ),
                (
                    "policy_blocked",
                    Some("cyber_policy"),
                    Some("gpt-6-astra"),
                    "sess-1:24"
                ),
                (
                    "retries_exhausted",
                    Some("response_too_many_failed_attempts.http_429"),
                    Some("gpt-6-astra"),
                    "sess-1:25"
                ),
            ]
        );
        assert_eq!(events[0].ts, 1_792_043_371_503);
        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains("capacity.") && !serialized.contains("secret"));
    }

    #[test]
    fn render_prints_bucket_rows_and_totals() {
        let body = serde_json::json!({
            "from_ms": 0, "to_ms": 7_200_000, "bucket": "hour", "live_inflight": 2, "retention_days": 730,
            "buckets": [{"start_ms": 0, "requests": 3, "sessions": 1, "input_tokens": 12_500, "output_tokens": 40,
                         "peak_concurrency": 2, "mean_concurrency_milli": 1500,
                         "outcomes": {"completed": 2, "failed": 1}, "events": {"overloaded": 1}}],
            "totals": {"requests": 3, "sessions": 1, "input_tokens": 12_500, "output_tokens": 40, "peak_concurrency": 2,
                       "mean_concurrency_milli": 1500, "outcomes": {"completed": 2, "failed": 1}, "events": {"overloaded": 1}},
            "series": []
        });
        let text = render(&body);
        assert!(text.contains("1970-01-01 00:00"), "{text}");
        assert!(text.contains("12.5k"), "{text}");
        assert!(text.contains("events: overloaded=1"), "{text}");
        let total = text.lines().find(|l| l.starts_with("total")).unwrap();
        assert!(
            total
                .split_whitespace()
                .collect::<Vec<_>>()
                .starts_with(&["total", "3", "1", "12.5k", "40", "2", "1.5", "1", "1"]),
            "{total}"
        );
    }
}
