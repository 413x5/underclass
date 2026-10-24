# Graceful Shutdown Drain on SIGTERM

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

## Context

`underclass serve` previously had no shutdown handling: `axum::serve` ran bare inside a
`tokio::try_join!`, so a SIGTERM (the signal `launchctl kickstart -k` sends during a deploy)
exited the process immediately. Every request in flight at that instant was cut off:

- streaming responses truncated mid-stream, surfacing as a transport error rather than an HTTP status;
- non-streaming responses never got a reply (connection reset);
- `usage_records` are written when the stream ends through `UsageFinish`'s `Drop`, so a killed
  process never recorded the interrupted attempt — it vanished from `underclass stats`;
- in-memory pool state (`inflight`, bindings) reset, which is harmless.

Failover is pre-first-byte only (ADR 0004), so a started stream is deliberately never re-sent to
another account. The only recovery is to make the process exit cleanly enough that `UsageFinish`
runs.

## Decision

- `async_serve` installs a signal handler for SIGINT and SIGTERM (and ctrl-c on non-Unix) in a
  background task that broadcasts a `watch` flag.
- The same flag is passed to both listeners (HTTP and the monitor Unix socket) via
  `with_graceful_shutdown`, so `try_join!` still terminates and no new connections are accepted
  after the signal.
- In-flight requests are given a bounded drain window of 5 seconds; if they have not completed
  by then, the process aborts the remainder. The bound keeps the drain inside launchd's
  10-second `ThrottleInterval` so a restart is not delayed.
- Nothing in this path bypasses `UsageFinish`: an aborted stream drops its guard and records
  `client_closed`, so an interrupted attempt appears in accounting instead of disappearing.

Out of scope: retrying started streams (deliberately excluded by ADR 0004), and any change to
`underclass-update.sh` (it already builds before touching the service).

## Consequences

- Deploys no longer truncate in-flight responses silently; interrupted attempts are recorded as
  `client_closed`.
- A drain longer than 5 seconds is impossible, so a wedged request cannot stall a restart.
- `UsageFinish` remains the single accounting path; shutdown does not introduce a second one.
