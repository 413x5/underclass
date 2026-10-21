# Quota Dimensions Compose in Selection

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

## Context

The fork and upstream independently added a way to cool *part* of an account rather than all of
it, and the merge had to carry both.

[Per-Surface Quota Cooling](per-surface-quota-cooling.md) cools a **billing surface**: the catalog
declares a `scope` per model (`credits`, `cline-pass`), and `Account::scope_cooling` holds one
deadline per scope. A `402` on a Cline credit balance therefore leaves the ClinePass subscription
on the same key routable.

Upstream's ADR 0023 cools a **model**: `Outcome::ModelQuotaExhausted` records an in-memory
`model_cooldowns[account][model]` deadline, so an Anthropic weekly limit scoped to one model family
leaves the account serving its other models.

Each is correct, and neither subsumes the other. A billing surface is a property of the *catalog
entry* and is shared by every model that draws on it; a model cooldown is a property of one model
on one account. A refresh token that pays for two billing surfaces still has one credential-level
quota, so a surface is not a model either.

The pool core had one eligibility predicate and one saturation deadline. Both had to become the
conjunction, and a merge that resolved the textual conflict by taking either side would have
silently dropped one dimension - the failure mode this record exists to prevent.

## Decision

- Selection requires **all three** dimensions to admit the account: the account-wide
  `status`/`reset_at`, the model's billing scope for that account, and the per-model cooldown.
  `PoolCore::eligible_at` is the single predicate; `select` and `min_reset` both call it, so the
  two cannot disagree about which accounts count.
- The advertised saturation deadline is, per account, the **later** of its account-wide, scoped,
  and per-model deadlines - an account serves only once every applicable deadline has passed - and
  across accounts the **earliest**. `Disabled` and `AuthError` accounts contribute nothing, because
  the value is advertised as `Retry-After` and those accounts would not serve at that time.
- `report_for` keeps routing `QuotaExhausted` to a billing scope and delegates
  `ModelQuotaExhausted` to `report_model_quota`, which touches no account status. A model cooldown
  is in-memory only: it is derived from a live upstream response and expires on its own, so it is
  neither persisted nor resurrected by `sync_from_store`.
- `sweep` clears expired deadlines from both `scope_cooling` and `model_cooldowns`.
- The catalog keeps the fork's `HashMap<BackendId, HashMap<String, String>>` (model id to scope)
  rather than upstream's `HashSet<String>`, because the set cannot express a scope.

## Consequences

- A credits `402` and an Anthropic model-scoped limit compose: whichever dimension is exhausted
  blocks only its own slice, and a request that both would block reports the later deadline.
- Two accounts of the same backend with different quota state stay independently routable, which
  is what the fail-fast `429` promises: saturation is only reported when no eligible account could
  serve the model on that surface at any point.
- Adding a fourth quota dimension means extending `eligible_at` and `min_reset`; the property tests
  in `tests/properties.rs` assert one invariant per dimension plus the surface partition, and the
  saturation deadline is asserted against the deadline actually reported.
- The per-surface record's claim that "adding another multi-surface backend only needs catalog
  scopes" still holds, but scopes are no longer the only way to scope a cooldown.
- **Divergence from upstream ADR 0023's 429-scope rule.** That record names "overage" and an
  overage-disabled reason as model-scoped signals. They are not: its own captured header sample
  shows `anthropic-ratelimit-unified-overage-status: rejected` and
  `...overage-disabled-reason: org_level_disabled` on ordinary successful responses, so treating
  them as a signal would cool a model on every request-scoped 429. The classifier therefore reads
  only the shared five-hour and seven-day window statuses and the weekly other-models window, and
  the request-scoped body phrases are checked before any header. ADR 0023 is left unedited
  (append-only); this paragraph is the record of the divergence.
