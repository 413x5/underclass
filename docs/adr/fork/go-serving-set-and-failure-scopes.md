# Go Serving Set and Failure Scopes

## Status

Superseded in part by `model-catalog-from-upstream-listing.md`: the routable set is now the
union of Go's `/v1/models` listing and this build's `default_catalog()` floor, and models Go
publishes after this build (including free/promotional ones) become routable without a release.
Decisions #2 (402 scoping) and the per-model cooldown remain in force. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).
It corrects two decisions of [OpenCode Go Backend over Go's OpenAI Surface](opencode-go-backend.md).
That record's body is left unedited; only its `## Status` gained a pointer here, which the fork
convention already uses for [Per-Surface Quota Cooling](per-surface-quota-cooling.md).

## Context

The Go backend shipped with three assumptions that live probing against a subscribed workspace
disproved. Each was cheap to hold and expensive to keep: the first two caused a working account to
be pulled out of rotation, the third silently withheld models the subscription pays for.

1. **Go's `/v1/models` listing was treated as the routable set.** A refresh published only what the
   listing carried. Measured: the listing omits ids Go still serves - `cat-jam-free` answered `200`
   while absent from it - and carries ids Go refuses for this plan. The listing is a catalogue of
   what Go offers somewhere, not of what this workspace can call.

2. **A `402` was read as the workspace balance being empty**, so it cooled the account. Measured:
   `longcat-2.0` answers `402 {"message":"Upstream request failed: Endpoint is unavailable."}`
   consistently, on both OpenAI paths, while every other model on the same account serves normally.
   One model's outage therefore took the whole Go subscription out of rotation.

3. **The table's servable set was never checked against the gateway.** Five ids were offered that
   never serve: `mimo-v2.5-pro` (`400 Unsupported model`), `space-bunny` and `step-5-preview-free`
   (`400 Model is unavailable`), `longcat-2.5-preview-free` (`402`), `minimax-m3.1-flash-free`
   (`410 deprecated`).

Two further gates were found that are workspace settings rather than per-model faults, and neither
is underclass's to work around: a region setting (a model outside it answers `400 "... requires
Global regions"`) and a request-data training consent (`400 "This Go model trains on request data"`).
Both are the operator's to decide, and both are reported as they arrive.

## Decision

- **This module's table is the routable set.** `refresh_catalog` publishes exactly
  `default_catalog()` with the stored name and limits carried forward; Go's listing adds and removes
  nothing. `parse_models_response` stays as the listing parser, describing what Go offers and
  dropping ids this build cannot route - an unclassified id has no upstream path - but it is no
  longer a catalog source. A refresh that reaches upstream therefore leaves the routable set
  unchanged, so a listing failure cannot empty it.
- **The table holds no id measured as unservable.** `mimo-v2.5-pro`, `space-bunny`,
  `step-5-preview-free`, `longcat-2.5-preview-free` and `minimax-m3.1-flash-free` are removed from
  `WIRE_FAMILY` and `catalog_entries()`. `longcat-2.0` stays: its `402` is a live outage the
  per-model cooldown below handles, not a missing model.
- **A `402` naming an unavailable endpoint cools the model, not the account.** `classify` maps it to
  `ModelQuotaExhausted`, the same dimension a `429` uses, so the rest of the catalog keeps serving
  and the model returns on its own when the outage clears. Any other `402` remains account-wide: an
  empty balance is a property of the workspace, not of one model.

## Evidence

Probed on 2026-10-23 against the live gateway with a subscribed workspace, both toggles on and the
region set to Global:

- `longcat-2.0` -> `402 "Endpoint is unavailable"` on `/openai/v1/chat/completions` and
  `/openai/v1/responses`, three consecutive tries, while `glm-5.3-flash`, `kimi-k3` and
  `deepseek-v4.1-flash` served `200` on the same key seconds apart.
- `cat-jam-free` -> `200` while absent from `GET /inference/go/v1/models`; `beam-free` -> `200`
  while present.
- `mimo-v2.5-pro` -> `400 "Unsupported model"`; `space-bunny`, `step-5-preview-free` ->
  `400 "Model is unavailable"`; `minimax-m3.1-flash-free` -> `410 deprecated`; all four on both
  OpenAI paths.
- Every one of the 36 listed ids was probed on `/openai/v1/chat/completions`: 23 served, 13 refused
  - the six Responses-family ids, which serve on `/openai/v1/responses` instead, plus the seven
  above.
- A model outside the workspace's region -> `400 "... requires Global regions. Select Global in your
  workspace's Privacy settings"`, and the same model served once Global was selected.
- A model whose training consent is off -> `400 "This Go model trains on request data. Allow paid
  endpoints that train..."`; Muse Spark 1.3 and 1.2 served once the toggle was on.

## Consequences

- A Go subscription no longer loses every model because one of them is unavailable, and no longer
  offers models that can only fail.
- A model Go cannot serve flaps on the configured cooldown for as long as the outage lasts. That is
  the intended trade: the alternative is guessing whether the `402` is permanent, and a wrong guess
  either strands a working model or removes a working account.
- The operator's region and training-consent settings are surfaced as upstream errors and are not
  second-guessed by the pool. A model refused for either reason is a per-request failure, not an
  account fault, so it neither cools the account nor is hidden from the catalog.
- `longcat-2.0` stays offered while its `402` persists. If it is later confirmed permanently gone
  rather than unavailable, it joins the removed list - a table change, not a new decision.
- This record corrects the earlier one rather than editing it, so the reasoning that shipped first
  stays legible: a reader can see that the listing was once trusted and a `402` once meant a balance.
