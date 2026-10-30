# Model Catalog from Upstream Listing

## Status

Accepted. Supersedes decisions #1 and #3 of `go-serving-set-and-failure-scopes.md`.

## Context

The Go backend previously treated a hardcoded table as the routable set because Go's
`/v1/models` listing omits servable ids and includes unservable ones, and the wire family
(Chat vs Responses) is not in the listing. That left newly published models - including
free/promotional ones such as "Iceman Contributor Free" and "Big Pickle" - unroutable
until a code change.

`models.dev` now provides limits and an `npm` package field that maps to the wire family
(`@ai-sdk/openai` -> Responses, `@ai-sdk/openai-compatible`/absent -> Chat), so the
in-binary table can be seeded from it for all 37 models.

## Decision

- **Membership is dynamic.** `refresh_catalog` publishes the union of Go's `/v1/models`
  listing and this build's `default_catalog()` floor (Cline-style `merge_discovered_models`),
  so a model Go publishes after this build becomes routable without a restart. An empty
  listing leaves the stored catalog untouched (never-shrink), including the bootstrap case
  where the floor seeds the catalog.
- **Wire family** is taken from the in-binary `WIRE_FAMILY` table for known ids; an id
  this build has never seen defaults to `Responses` (Go translates between its APIs), so a
  newly published model stays routable. Anthropics/Gemini-only ids remain `None` and are
  excluded.
- **Limits** come from the in-binary `catalog_entries()` seed (sourced from models.dev)
  and survive refreshes via `carry_forward_limits`, preserving operator edits.
- **Unservable models** are no longer removed from the table; per-model cooldowns (402 ->
  model, 429 -> model) gate them, and a wrong wire-family guess is corrected by the
  cooldown on failure.
- **Free alias** needs no change: `build_alias_table` already creates `free/<base>` for
  any id/name containing a delimited "free" token.

## Consequences

- A Go subscription surfaces every model Go lists, including free/promotional ones, as
  `free/<base>` aliases automatically.
- A model Go cannot serve flaps on its per-model cooldown instead of being hidden; the
  alternative (guessing permanence) strands working models.
- The in-binary table is a seed, not a gate; it must be refreshed from models.dev when
  Go's lineup changes materially.
