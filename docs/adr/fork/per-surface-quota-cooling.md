# Per-Surface Quota Cooling

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md). It supersedes in part
[Cline Backend with Responses Translation](cline-backend-responses-translation.md): that record's
account-wide `402` cooldown is replaced here, and its body is left as accepted.

## Context

A Cline API key serves two billing surfaces: credit-based models (`cline/anthropic/…`) and the
ClinePass subscription (`cline/cline-pass/…`). The Cline backend classified `402 Payment Required`
as `QuotaExhausted`, and the pool stored quota as one account-wide `Cooling { reset_at }` state.

Because both surfaces share an account, the first `402` from a credits model removed the account
from rotation for every model. A ClinePass request then never reached upstream: selection found no
eligible account and the proxy answered `429` "all subscriptions for this model are out of quota"
with a `Retry-After` derived from the configured Cline cooldown — a limit-reached report for a
surface that was never attempted.

The account-wide assumption was also load-bearing in the monitor: a partially cooled account
rendered as `healthy`, so the dashboard repeated the same misleading story.

## Decision

Make the quota scope a property of the **model catalog**, not a backend special case. The pure
core resolves a model id to an optional scope from injected catalog data and never names a
backend (`CONTRACTS: backend-agnostic-core`).

- `ModelInfo` carries `scope`. Cline assigns `cline-pass` to every `cline-pass/…` upstream id and
  `credits` to everything else. Codex and Copilot leave it empty, which means account-wide.
- `Account` carries `scope_cooling: scope → deadline`. `status`/`reset_at` remain the account-wide
  state, so a genuine account-wide cooldown still blocks everything.
- Selection requires the account to be healthy **and** to have no unexpired deadline for the
  requested model's scope. For saturation, an account's own retry time is the later of its
  account-wide and scoped deadlines (it serves only once both have passed), and the advertised
  `Retry-After` is the earliest such time across eligible accounts.
- `PoolCore::report_for(account_id, model, outcome)` scopes `QuotaExhausted`; `AuthFailed` and
  transient outcomes stay account-wide through the existing `report`. A rejected key is a
  credential problem, not a quota surface.
- Every quota classification is scoped uniformly: `402`, `429`, and `Retry-After`-derived
  deadlines all cool the requested surface only.
- Persistence is write-through (ADR 0005): `catalog.scope` and `accounts.scope_cooling` (a JSON
  object) are new columns with `PRAGMA`-guarded migrations, and `Store::update_account_health`
  writes status, deadline, and scopes in one statement.
- Observability is part of the decision: `utop`, `/admin/api/monitor`, and `/admin/api/state`
  expose per-surface deadlines, and the account's `HEALTHY` count excludes partially cooled
  accounts.

## Consequences

- A credits `402` no longer removes the ClinePass subscription from rotation, and a ClinePass
  quota error does not block credit-based models. Each surface fails fast on its own deadline.
- A rate limit that genuinely applies to the whole key is still recorded against one surface, so
  the next request on the other surface reaches upstream and fails there rather than being refused
  by the pool. That costs one upstream request per exhausted surface to confirm the real state;
  it is the trade the Cline `402` ambiguity requires, because Cline does not say whether a `402`
  came from the credit balance or the subscription.
- Operators can recover per-surface state: an `enable` that clears `Disabled`/`AuthError` also
  clears scoped deadlines, while an upstream account-wide cooldown keeps them.
- A `403` remains non-cooling and per-resource, unchanged from the Cline backend ADR.
- Adding another multi-surface backend only needs catalog scopes; no pool or proxy change.
