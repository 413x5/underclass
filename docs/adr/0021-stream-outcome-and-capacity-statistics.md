# 0021 - Stream Outcome Accounting and Long-Range Capacity Statistics

## Status

Accepted

## Context

Codex users saw "Selected model is at capacity. Please try a different model." while the proxy logged every request as `200`. Comparing Codex session logs with the journal showed why. Upstream accepts the request, starts an SSE stream, and a few seconds later (median 4.8s) sends `response.failed` with `server_is_overloaded`. Under ADR 0010 the proxy passes 2xx streams through untouched, so the failure appeared only as a usage row without token counts. A row like that could not be told apart from a client disconnect or a truncated stream. It happened across every account and several models, so it is upstream capacity rather than a pool fault, but the proxy had no record of it.

The operator also wants long-range statistics: tokens in and out, concurrency, session counts, capacity problems, and policy refusals such as `cyber_policy`, over up to two years. Usage rows were never pruned, and nothing recorded how long an attempt lasted, so past concurrency could not be calculated.

## Decision

- `UsageTap` also classifies how each body ended. The first terminal event wins: `response.completed` / chat `[DONE]` gives `completed`; `response.failed`, a stream `error` event, or a chat `error` gives `failed`; `response.incomplete` gives `incomplete`. Without one, the outcome comes from how the transfer ended: `truncated` (EOF), `upstream_error`, or `client_closed` (the body was dropped). It records whether any output event (`*.delta`, `response.output_item.added`) arrived first (`saw_output`). Only the error code is captured (`code`, else `type`), and only after `sanitize_code` (ASCII `[A-Za-z0-9_.-]`, at most 64 bytes), so upstream message text never reaches logs or the database. The SSE `event:` name is used when an oversized payload was dropped.
- `usage_records` gains `duration_ms`, `stream_outcome`, `error_code`, and `saw_output`. `ts` is now the attempt start, so `[ts, ts + duration_ms)` is the attempt's lifetime. Every finished body logs `stream.finished`, at warn level for failed, incomplete, truncated, and upstream_error outcomes, and the request ring buffer shows the outcome next to the status.
- A new `upstream_events` table records events of these kinds:
  - `overloaded` (capacity codes);
  - `policy_blocked` (policy/safety codes such as `cyber_policy`);
  - `quota_exhausted`;
  - `retries_exhausted`;
  - `upstream_failed`;
  - `quota_cooling` (an account entered cooldown);
  - `pool_saturated` (the proxy returned 429 because every eligible account was cooling).

  The proxy writes them through as they happen. Past events can be backfilled from Codex rollout logs with `underclass stats import-codex`. It reads `task_complete` errors carrying `codex_error_info` on the client side and posts only timestamp, kind, sanitized code, model, and a `session:ordinal` dedupe key to `POST /admin/api/events/import`. Re-importing inserts nothing.
- `GET /admin/api/stats` (admin token) and `underclass stats` report, per minute, hour, or day bucket and optionally per model or account:
  - requests;
  - attempts;
  - sessions (distinct prompt-cache keys);
  - tokens;
  - unmeasured successes;
  - peak and time-weighted mean concurrency (a sweep over attempt lifetimes in the pure `stats` module);
  - outcome counts;
  - event counts.

  Window totals use exact `COUNT(DISTINCT)` for requests and sessions.
- Data is kept for `stats_retention_days` (default 730; `UNDERCLASS_STATS_RETENTION_DAYS`). A 15-minute maintenance task prunes older usage rows, events, and rollups in batches. It stores a `stats_daily` rollup per finished UTC day for the total, per model, and per account. A day counts as finished 6 hours after it ends, so long streams have landed. Day-bucket queries read rollups for finished days and raw rows for the rest. Minute buckets are limited to 2 days and hour buckets to 31 days.
- Failover is unchanged: still only before the first byte (ADR 0010, `pre-first-byte-failover`). `saw_output` measures how many failures happen before any output, which is the evidence a later decision on failover before output would need.

## Consequences

- Capacity failures, policy refusals, and quota pressure are visible per account, model, and hour in the journal, the database, the admin page, and `underclass stats`, instead of hiding behind HTTP 200.
- At current traffic, two years of raw rows is about 1.4 GB; daily rollups keep long-range queries cheap. Rows from before this change have no duration or outcome. They count as `unknown` outcomes and add nothing to concurrency.
- An attempt that runs longer than 6 hours and lands after its day was rolled up is missing from that day's rollup until the day is recomputed. Importing events into a finished day discards its rollup so it is recomputed.
- Event classification is by error-code substring (`overload`/`capacity`/`slow_down`, `policy`/`safety`/`content_filter`, `usage_limit`/`quota`). New upstream codes fall into `upstream_failed` until they are mapped.
