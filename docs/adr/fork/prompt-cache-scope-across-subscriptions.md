# Prompt Cache Scope Across Subscriptions

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md).

## Context

Stickiness (ADR 0003) exists so a conversation keeps landing on one subscription. The stated reason
is that a per-account prompt cache would go cold if the pool round-robined a session across
accounts. That reasoning is sound for Codex and Copilot, where the cache belongs to the upstream
account.

Cline is different: model access is proxied by a single gateway (`api.cline.bot`) that fronts many
providers. The question was whether the gateway's prompt cache is keyed per Cline API key or shared
across all of them. If it is shared, a session spread across Cline subscriptions still hits cache,
and the stickiness guarantee matters only for provider-side session affinity and for keeping one
conversation on one billing surface — not for cache warmth.

Two further observations frame the decision:

- The admin UI's per-request `cache key` column is empty unless the *client* supplies
  `prompt_cache_key` / `promptCacheKey` (or `session-id`). `extract_sticky_key` has no derivation
  fallback. An empty cell therefore means "this client sent no key", not "caching is off".
- `ClineBackend::prepare_body` strips `prompt_cache_key` before the upstream call because Cline's
  routers reject unknown request controls. The key is a *proxy-side routing* input for Cline; it is
  never forwarded.

## Decision

Record the measured behaviour and change nothing in the code.

The Cline gateway's prompt cache is **shared across Cline API keys**, so cross-subscription reuse
already happens. Measured directly against `https://api.cline.bot/api/v1/chat/completions` with two
different account keys and an identical ~8k-token system prefix:

| call | account | prompt tokens | cached tokens |
| --- | --- | --- | --- |
| cold | A | 8040 | 0 |
| warm | A | 8040 | 7808 |
| same prefix | B | 8040 | 7808 |

The third row is the result that matters: account B had never sent that prefix, yet it scored a
cache hit on the prefix warmed by account A. The same pattern reproduced through the proxy,
including concurrent requests forced onto different accounts. Caching at Cline is content-addressed
and gateway-wide, so the pool's account selection does not affect cache hit rate.

Consequently no derived sticky key is introduced. Deriving one from the conversation prefix would
pin sessions more aggressively but would not increase cache reuse, and it would add a pure-core
routing rule whose only benefit is billing-surface affinity.

## Consequences

- Cache hit rate is independent of which Cline subscription serves a request, so failover across
  Cline accounts costs no cache warmth.
- The empty `cache key` column in the admin UI for Cline traffic is expected when the client omits
  the key. `opencode` configured by `underclass connect` sets `setCacheKey: true`, so sessions
  started through it do send one.
- `usage_records` does not persist `cached_tokens`, so cache effectiveness remains unobservable from
  the UI. Deliberately out of scope here; revisit if cache hit rate becomes a tuning input.
- This finding is specific to Cline. Do not generalize it to Codex or Copilot, whose caches are
  account-scoped and still rely on stickiness per ADR 0003.
- Upstream `api.cline.bot` intermittently answers `500 {"error":"empty response content"}` on
  reasoning models, independent of cache state and reproducible on both keys. It is a transient
  upstream flake, not a proxy fault.
