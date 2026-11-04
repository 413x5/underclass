# Abandoned Requests Are Visible, and the Three Edges of an Attempt Are Bounded

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

## Context

A pr-agent review through the proxy took 2352s. Decomposed from underclass's own log, it contained
five requests that were selected and then produced **no terminal event of any kind**: no
`request.finished`, no `stream.finished`, no error line, and no `usage_records` row, because that
row is only written when an attempt ends.

| selected at | next review call | gap |
|---|---|---|
| 07:21:47 | 07:25:16 | 209s |
| 07:36:04 | 07:36:47 | 43s |
| 07:36:47 | 07:51:04 | **857s** |
| 07:51:04 | 07:51:48 | 44s |
| 07:57:08 | (run ended) | — |

The gaps are 43s, 44s, 209s and 857s — not uniform, so this was neither a fixed timeout nor a fixed
retry delay. pr-agent reported `AI calls: 1`, so these were litellm's internal transport retries
inside one logical call. underclass could not distinguish them from a hang, from an upstream stall,
or from a client that walked away.

The cause of the blindness was structural: `request.completed` is logged only after `attempt_account`
returns, i.e. after upstream response headers arrive. A request that never gets headers logged
`request.selected` and nothing else, forever.

The same report showed the opposite problem at the other edge. Nothing bounded a waiter at all: the
only inference-path bounds were `connect_timeout = 10s` and `stream_idle_timeout_secs = 0`, and the
idle guard applied only *after* the body started. A request whose client had already given up stayed
alive with no record and no end.

## Decision

Two changes, deliberately scoped so neither decides anything the client has not already decided.

### 1. An attempt that never gets upstream headers is never silent

Instrumentation only. No timeout is added by this part, and no retry, failover, selection, or
streaming behaviour changes.

- `request.awaiting_headers` marks the start of the wait for the upstream's response head. It
  carries `request_id`, `attempt`, `cache_key`, `model`, `upstream_model`, `endpoint`, `backend`,
  `account`, and `account_id`. This gives an attempt a clock even when nothing else arrives.
- `AttemptWatch` is a `Drop` guard, the same idiom as `UsageFinish`, `InflightGuard` and
  `AffinityGuard`. Whether the attempt ends by response, error, client disconnect, cancellation,
  or the future being dropped, `request.abandoned` is emitted **exactly once**.
- The terminal line carries `stage` (`token` or `awaiting_headers`), `reason`
  (`upstream_silent`, `client_aborted`, or `header_timeout`), `attempt`, `cache_key`, and
  `elapsed_ms`.
- It is deliberately **not** emitted when the head arrived (existing logging covers the attempt),
  when the attempt failed for a reason it already logged, or when the configured header bound
  expired (`upstream.headers_timeout` covers that). One attempt, one terminal record.
- `attempt` is the attempt's 1-based position in one request's failover chain. `cache_key` is the
  client's own session key — the identity the proxy already uses for stickiness — and is the only
  thing that joins separate requests belonging to one logical call, since transport-level retries
  arrive as new requests. No second correlation concept was introduced.

### 2. Three bounds nest, and none of them caps a healthy response

| bound | default | what it bounds | why |
|---|---|---|---|
| `upstream_connect_timeout_secs` | 10s | TCP/TLS connect only | an unreachable upstream must fail over, not hang |
| `upstream_response_header_timeout_secs` | 1800s | the wait for the response **head** | a leak backstop for a request nobody waits for any more |
| `stream_idle_timeout_secs` | 60s | the gap before the **next chunk** | a stream silent for a minute mid-body has stopped, not slowed down |
| total response time | unbounded | — | a long review must run as long as the upstream keeps producing |

- `upstream_response_header_timeout_secs` keeps preflight-proxy's key name so the two hops read the
  same. It is applied with `tokio::time::timeout` around the send, never as a `reqwest` client
  timeout: the latter bounds the body too and re-creates the premature cut that
  [Stream Failures and Timeouts](stream-failure-and-timeouts.md) removed. On expiry the attempt logs
  `upstream.headers_timeout` with `elapsed_ms` and `limit_ms`, records one usage row carrying
  `upstream_response_timeout`, and fails over like any other attempt that produced no response.
  When no account can produce a response at all, the client receives `504` with that same code in
  the existing error envelope.
- 1800s sits deliberately above a client's own wait — the review bundle sets `ai_timeout=1500` — so
  this bound never races the client's decision. Lowering it toward the client's timeout turns a
  backstop into a policy and brings back the original bug.
- `stream_idle_timeout_secs` wraps one `body.stream.next()` call, so the clock resets on every chunk.
  It therefore bounds the silence *between chunks*, not the body's total duration; a long but
  active stream is never cut. The default is now on at 60s. A healthy stream emits deltas
  continuously, reasoning deltas included, so a minute of complete silence mid-body means the
  upstream has stopped talking.
- `0` still means off for both of these keys, and remains accepted at the file and environment
  levels, so an operator can disable either bound.
- The idle path already ends the body cleanly — one terminal `upstream_stream_error` event and a
  complete frame — and that is unchanged. This record only turns it on by default.

## Consequences

- A request abandoned mid-flight has a terminal line with a duration, so the 43s/44s/209s/857s
  pattern is readable as retries against a silent upstream rather than as a hang.
- The non-streaming path is covered by the header bound, since it has no body to observe before the
  head arrives. Measured TTFB equals total on that path (77.27s vs 77.30s), which is exactly why the
  header bound is a leak backstop at 1800s rather than a lower, "useful" value: any bound tight
  enough to bound generation would kill healthy calls, and one 546s call produced a good review.
- An operator who wants dead streams ended sooner lowers `stream_idle_timeout_secs`; one who wants no
  bound at all sets either key to `0`.
- Accounting gains a row for an attempt that timed out on its head, joined to the client's error by
  the shared `upstream_response_timeout` code.
