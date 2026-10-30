# Model Aliases: Subscription vs Credits Routing

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md).

## Context

A single account can serve models that draw on different billing surfaces: a Cline API key
serves both ClinePass subscription models (`cline/cline-pass/...`) and credit-based models
(`cline/...`). The operator wants to route a request through only subscription accounts or
only credits accounts for the same model family, using a request id such as
`subscriptions/deepseek-v4.1-flash` or `credits/deepseek-v4.1-flash`.

The existing routing model (ADR 0010) is model-eligibility-driven: a model id is served by
any healthy account whose backend catalog contains it. There is no notion of "billing kind"
in the request id, so clients cannot express "subscription only" vs "credits only".

## Decision

- **No DB schema change.** Aliases are derived at runtime from the existing catalogs plus
  a config-driven classification. The DB stays as close to upstream as possible.
- **Config** (`~/.config/underclass/config.toml`, `[model_aliases]`): maps alias kind →
  list of catalog-id prefixes. Default:
  - `subscriptions = ["cline/cline-pass/", "opencode-go/"]`
  - `credits = ["cline/"]`
  - `default_kind = "subscriptions"`
  Operator overrides merge per-kind over defaults, so a partial config never drops other
  kinds.
- **Cline's `scope` column is authoritative** for Cline accounts: `cline-pass` →
  subscriptions, `credits` → credits. Other backends match configured prefixes (longest
  prefix wins); unmatched → default kind.
- **Base name = last path segment** of the model id. `cline/openai/gpt-5.2` →
  `credits/gpt-5.2`; `opencode-go/deepseek-v4.1-flash` → `subscriptions/deepseek-v4.1-flash`.
- **Alias table** is an in-memory `HashMap<String, Vec<(BackendId, concrete_model_id)>>`
  built on boot and rebuilt when any catalog changes (periodic sync + explicit refresh).
- **PoolCore** gains `aliases` + `alias_config` and alias-aware `serves`, `declared_scope`,
  `concrete_for`. `Selection` gains `concrete_model: String` (the backend-specific id sent
  upstream). Exact concrete matches take precedence over aliases per-backend.
- **Proxy** keeps `requested_model` (alias or concrete) for selection and uses
  `selection.concrete_model` as `upstream_model` for `rewrite_url`, `responses_via_chat`,
  `auto_stream_usage`, body model mutation, and `prepare_body`. Usage records store the
  concrete model id; the alias is shown in the request ring buffer / admin UI.
- **`/v1/models`** includes alias entries (id = alias, limits merged by max,
  `owned_by = "underclass-alias"`), filtered per concrete model to the aliases at least one
  account-backed backend serves. See [Only Account-Backed Models Are Advertised](account-backed-model-advertisement.md).
- **Admin endpoint** `GET /admin/api/aliases` returns the derived alias table.
- Alias validity is validated: `default_kind` must name a configured kind.

## Consequences

- Clients can request `subscriptions/<model>` or `credits/<model>` to restrict routing to
  accounts of that billing kind. Direct concrete ids keep working unchanged.
- No DB migration; aliases live in memory and are rebuilt from catalogs + config.
- Mixed, subscription-only, and credits-only accounts all work without special-casing:
  an account simply never matches an alias whose concrete ids its catalog lacks.
- Catalog refresh paths rebuild the alias table (periodic sync every 10s plus explicit
  rebuild on each refresh) so the table stays consistent.
- Cooldowns are keyed by the requested id (alias or concrete) so they stay consistent
  within one request family; scope resolution uses the concrete id.
