# Cline Catalog from the Recommended-Models Feed

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md). It supersedes the catalog decision of
[Cline Backend with Responses Translation](cline-backend-responses-translation.md), whose body is
left unedited.

## Context

The Cline backend built its catalog from `GET /api/v1/models` and merged a hand-maintained table of
ClinePass models so the subscription stayed routable. That table is a guess about a list underclass
does not own, and it aged into a defect: Cline published `cline-pass/deepseek-v4.1-flash-1017`, the
listing did not carry it, the table did not name it, and the pool answered `503 no_accounts`
("no subscriptions are configured for this model") for a model the subscription serves.

Measured against the live gateway on 2026-10-24:

- `GET /api/v1/models` is a 474-entry list of `provider/model` ids with no `cline-pass/` or
  `cline-cloud/` entry in it at all.
- `GET /api/v1/ai/cline/recommended-models` is public (no credential; `200`) and returns four
  buckets: `recommended` (6), `free` (5), `clinePass` (15), `clineCloud` (1).
- Of those buckets, `recommended` repeats ids the listing already carries; `clinePass` and `free`
  carry ids it does not. `clineCloud` is excluded: every Cline surface underclass speaks is
  OpenAI-shaped, and no evidence exists that `cline-cloud/` ids serve on it.

Cline's own clients read the same feed, and treat its `clinePass` bucket as the ClinePass catalog
(`sdk/packages/core/src/cloud/models.ts`, `sdk/packages/llms/src/catalog/catalog-cline-recommended.ts`).
There is no `/cline-pass/models` endpoint: it answers `401` and no such route exists in
`cline/cline`.

## Decision

- **The feed is the authority for the models the listing omits.** `refresh_catalog` reads the
  listing as before, then reads the recommended-models feed and merges the `clinePass` and `free`
  buckets. `parse_recommended_models` namespaces each entry under `cline/` and assigns the scope its
  id derives, so a model published after this build is routable without a release.
- **`recommended` is not read.** Its ids are already in the listing, which carries limits the feed
  does not; reading it would add rows with no new information and one more way for a name to differ.
- **A free model bills `credits`.** Free models ride usage billing at $0 rather than the
  subscription quota, so scope follows the id (`quota_scope`) and not the bucket the entry came
  from. A free model therefore never inherits `cline-pass` cooling.
- **The table degrades gracefully instead of being the source.** `default_catalog` keeps a set of
  documented ClinePass models and `subscription_catalog` merges it with the feed, so a feed outage
  leaves previously working subscription models in the catalog, and a model the table already
  documents keeps its human name while a feed-only one is added under its slug.
- **One id in several sources is combined, not overridden.** `build_catalog` publishes the union of
  the listing, the feed and the documented rows. `merge_row` keeps a non-zero limit from whichever
  source reported it, keeps a non-empty scope, and orders names by quality instead of arrival: a
  name that is not the model's slug beats one that is. That matters because no source is complete -
  the listing carries limits but no ClinePass ids, the feed carries those ids but no limits, and the
  table carries human names the other two lack.
- **No merged row may carry an empty scope.** `merge_discovered_models` backfills an empty scope
  from the id for every row it returns, because an empty scope means account-wide quota here and
  would let a credits `402` cool the subscription surface.

## Consequences

- A model Cline starts serving becomes routable at the next refresh (boot, admin refresh, or
  onboarding), with no table edit and no release.
- The documented table is now a fallback and a source of display names, not the routable set of
  ClinePass models. It still has to be maintained for the case where the feed cannot be read.
- The feed is public, so the subscription catalog can be refreshed without a healthy key; the
  refresh itself still requires a key because the listing does, and a refresh that cannot read the
  listing remains a no-op.
- A feed entry outside `clinePass`/`free` is not published, so a new bucket Cline adds later is
  invisible until this decision is revisited - the same trade the listing already made.
- The feed names ClinePass entries by slug, so a feed-only model appears under its slug until
  someone documents a name for it. Name precedence is therefore a property of the merge, not of the
  order the sources happen to be read in.
- A failed feed read is logged (`catalog.cline_feed_unavailable`) rather than silently absorbed: the
  documented table keeps the subscription routable, but a feed-only model would stop being routable,
  which is the symptom this record exists to remove.
