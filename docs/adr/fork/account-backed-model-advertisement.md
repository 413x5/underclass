# Only Account-Backed Models Are Advertised

## Status

Accepted

## Context

The stored model catalog lives in SQLite, one set of rows per backend, and it deliberately outlives
the accounts that produced it: a backend's catalog is seeded at boot and replaced by upstream
listings (`copilot`, `claude`, `cline`, `opencode-go` refresh paths), but removing an account never
prunes its rows. That is the right persistence rule - re-adding an account must restore its models
immediately, and an operator's hand-edited rows must not be thrown away because an account is
temporarily gone.

`GET /v1/models` merged every backend's catalog regardless. After a Copilot login was removed, its
34 rows kept being advertised, including a bare `grok-4.7` that no configured account could serve.
Routing then behaved correctly but unhelpfully: no backend was eligible, so
[unprefixed routing, flat pool](../../0010-unprefixed-routing-flat-pool.md)'s unknown-model fallback
picked the first backend declaring the surface (`codex`), which had no account either, and the
request failed with `503 no_accounts`. The id that actually worked was the alias
`subscriptions/grok-4.7`, whose only concrete model (`opencode-go/grok-4.7`) did have an account.

An advertised model therefore has to mean "some configured account can serve this", not "some
backend once published this".

## Decision

- `GET /v1/models` advertises a backend's catalog rows only when that backend has at least one
  account. "Configured" means an account row exists, regardless of `status`: a cooling, disabled,
  or auth-error account still makes the model a real, routable id that will come back.
- Aliases are built from the same filtered view, per concrete model: an alias appears while at
  least one of its `(backend, concrete)` pairs is servable, and an alias grouping models across
  backends keeps the concretes that still are (`free/beam-free` survives when one of its backends
  has an account). Filtering per alias instead would delete an alias because one of its backends
  is missing.
- Catalog rows are never deleted or rewritten by this filter. It is a read-time view over the
  store, computed in `listing::advertised_models` from the per-backend catalogs and the set of
  account-backed backends, so routing, catalog refresh, and the operator's edits are untouched.
- The same rule governs the two other places that offer models to a client: the opencode snippet in
  the admin UI (`/admin/api/state` now carries only account-backed catalogs, plus
  `catalog_backends` for the note) and `underclass connect`'s provider block.
- The filter is about accounts, not surfaces: `/v1/models` still lists Claude's Anthropic-surface
  rows when a Claude account exists, as it always has. Those two OpenAI-shaped provider blocks drop
  Claude for their own reason - an Anthropic wire protocol does not belong there.
- The **catalog editor keeps showing every stored row.** `GET /admin/api/catalog/{backend}` is how
  an operator inspects and edits what a backend published, including rows for a backend with no
  account right now; hiding them would make an unreachable catalog uneditable and would hide the
  stale rows this record is about. The editor labels such a backend as stored but not advertised.
  `GET /admin/api/aliases` likewise keeps the full derived table, which is the routing view rather
  than the advertised one.

## Consequences

- A stale catalog for a removed backend is retained, not pruned: re-adding the account advertises
  its models immediately, and the operator can still see (and clean up) the rows.
- Model discovery is honest about the pool's current contents. A model missing from `/v1/models`
  is one no account can serve now; the catalogs that hold it are still visible in the UI.
- Aliases whose only concretes live on account-less backends disappear from the listing, which is
  correct: a request for one would have failed the same way.
- `underclass connect` writes a smaller provider block after accounts are removed. Its "catalog is
  empty" fallback now applies only while a Codex account exists; with no account behind any model it
  writes an empty model map and says so, instead of seeding ids nothing can serve.
