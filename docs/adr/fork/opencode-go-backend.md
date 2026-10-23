# OpenCode Go Backend over Go's OpenAI Surface

## Status

Accepted, and corrected in part by
[Go Serving Set and Failure Scopes](go-serving-set-and-failure-scopes.md), which replaces this
record's routable-set and `402` decisions and leaves its body unedited. This is a fork-local ADR: it
deliberately carries no upstream number so a future `git pull` from `main` cannot collide with a
newer upstream `NNNN` record. See [README](README.md).

## Context

OpenCode Go is a $10/month subscription (Go Plus $40) that gives one Console API key access to a
curated model lineup through the OpenCode Console gateway at `https://opencode.ai/inference/go`.
It is the same shape of credential as Cline's: a static API key that never refreshes, pasted by
the operator. The operator asked for it to join the pool.

The gateway is not one API. It fronts four wire protocols under one base:

| API | Path |
|---|---|
| OpenAI Responses | `/openai/v1/responses` |
| OpenAI Chat Completions | `/openai/v1/chat/completions` |
| Anthropic Messages | `/anthropic/v1/messages` |
| Gemini | `/google/v1beta/models/<model>` |

Facts established by probing the live gateway, because they decide the design:

1. **Every request must carry `x-opencode-session`.** Without it Go answers
   `400 {"type":"MissingSessionID"}` before it looks at the credential.
2. **The upstream path must match the model's API family.** The Console model table, the OpenCode
   client's own catalog (models.dev, mirrored into `~/.local/share/opencode/opencode.db`), and the
   published harness pages all agree that each model is served by one family: GPT/Grok/Muse Spark
   over Responses, the open-weight lineup over Chat Completions, Claude/Qwen/MiniMax over Anthropic
   Messages, Gemini over Google. A request for the wrong path is refused upstream.
3. **`/inference/go/v1/models` is public.** It lists 36 ids with `id`/`object`/`created`/`owned_by`
   and nothing else - no context or output limits, and no API family. It needs no key.
4. **The free tier is client-gated.** The free models answer `403 FreeTierError` ("the free tier can
   only be used from within OpenCode") to anything that is not the real client; the check is not the
   user agent. A pooled proxy cannot serve them.
5. **A key with no Go subscription authenticates and then fails per request** with
   `403 "An active OpenCode Go subscription is required to use Go models."`.

## Decision

Add `go` as a fifth backend, serving **Go's OpenAI surface only**, with the `go/` model namespace.

- **Onboarding is an API-key paste** in the admin UI, like Cline's, rather than a device flow. The
  key is verified against `GET https://opencode.ai/console/api/v2/config` before persistence - the
  same endpoint the OpenCode client itself loads provider config from - and stored in the account's
  `refresh_token` column. `401` is reported as a rejected key; every other status keeps its own
  error, because Console answers `403` when the workspace demands an SSO sign-in and reporting that
  as a bad key would send the operator to re-issue a working one.
- **The label is the workspace id** (`x-opencode-org-id`) that the config response carries for the
  provider. It is the only identity the Console API exposes to a personal key (there is no
  `/api/user` for API keys), and it distinguishes two Go subscriptions on one host.
- **Only the OpenAI surface is served.** Go's `/anthropic` path is a Messages API, and underclass's
  Anthropic surface admits only Claude Code clients (ADR 0023); routing a Go account into it would
  either need a new Responses->Anthropic translator or would relax a security-shaped rule the fork
  has no reason to relax. Models Go serves *only* over `/anthropic` or `/google` are therefore not
  advertised at all: Claude Haiku 5.5, Qwen3.7 Plus, Qwen3.8 Max, Qwen3.8 Flash, MiniMax M2.7,
  MiniMax M3, Gemini 4 Argon Contributor. `wire_for` answers `None` for them, and both the default
  catalog and the refresh drop them, so nothing unroutable is ever offered to a client.
- **The wire family is a table in the backend, not a heuristic.** Neither Go's listing nor its error
  bodies name the family, so `src/opencode_go.rs` records it per model, sourced from the Console/Go
  endpoint tables and the OpenCode client's own catalog. An id the table does not classify is
  unroutable: guessing sends the request to the wrong upstream path and returns an error the client
  cannot act on. A model Go publishes later is added by extending that table.
- **`Backend::rewrite_url` and `Backend::responses_via_chat` take the model.** A backend that serves
  each model over a different upstream API has to choose per request, and the trait previously
  decided per backend. The parameter is additive: every other backend ignores it, and the proxy's
  behaviour for them is unchanged.
- **Sticky sessions become Go's session id.** Go routes and caches by `x-opencode-session`, and the
  proxy's sticky key is already the conversation's identity, so it is sent as that header. A client
  that names no session still gets a fresh id, because the alternative is a `400` before the
  credential is even read.
- **A session stays on one Go account, and therefore on one prompt cache.** The sticky key is the
  pool's ordinary binding key, so the second turn of a conversation reaches the account that served
  the first, and a pool with several Go subscriptions does not spread one conversation across them.
  Go is also the one backend whose session id *is* the sticky key, so the affinity the pool keeps
  and the session Go caches against cannot drift apart. The binding is persisted (ADR 0005) and
  survives a restart for `BINDING_TTL_MS`; it moves only when the bound account stops being eligible
  (cooling, disabled, or cooling for that model), and then it prefers another Go account.
- **A rejected key is final for the request.** A Go account holds a static Console key, so no
  status is redeemed by a refresh: `refresh_on` reports false for every status, and the backend
  trait decides that per backend rather than the proxy matching on a backend name.
- **A `429` cools the model, not the account.** Go meters usage per model ($ allowance enforced as
  20% per 5h, 50% per week, 100% per month), so `classify` reports `ModelQuotaExhausted` - the
  per-model dimension upstream added for Claude - with the `Retry-After` deadline when Go sends one.
  `402` (empty workspace balance) is account-wide. A `403` stays transient: Go uses it for the free
  tier's client gate, a model outside the workspace's plan, and a workspace with no Go subscription
  at all, and none of those is a rejected credential. A key with no subscription is therefore
  discovered on the first request and reported per request rather than by ejecting the account.
- **Limits come from this module, not from Go.** The public listing carries none, so the default
  catalog records context/input/output per model from models.dev and a refresh only adds ids the
  table classifies, copying the stored name and limits for every id it already has. The stored
  catalog is the operator's copy: this module's table seeds it and the UI edits it, and a refresh
  must not revert either.

## Consequences

- A Go account joins the flat pool, the sticky map, and the stats pipeline with no pool-core special
  casing: routing, cooldown readmission, failover limits, and accounting are unchanged.
- A `/v1/responses` request for a Chat-family Go model is translated to Chat Completions by the
  existing Responses translator, so an opencode or Codex client reaches Kimi, GLM, DeepSeek and the
  rest without knowing which family they are. A `/v1/chat/completions` request for a
  Responses-family model is forwarded as Responses and is refused upstream; underclass has no
  Chat->Responses translator and this record does not add one.
- The free models (Beam Free, Cat Jam Free, LongCat 2.5 Preview Free, MiniMax M3.1 Flash Free, Step
  5 Preview Free) are listed because Go lists them, but they answer `403 FreeTierError` to a pooled
  client. They stay transient, so they neither serve nor eject the account.
- Go's per-model allowances are approximate request counts, and the catalog's limits are context
  windows, not quota: an operator who wants a model's allowance to cool it for longer edits the
  catalog, exactly as with Cline.
- A Go key with no subscription authenticates, lists models, and then answers every request with
  `403`; underclass keeps that account healthy and surfaces the upstream message to the client,
  because the Console API reports a workspace rather than a plan and the proxy cannot tell the two
  apart before a request is made.
- Adding a fourth wire family (Gemini) or Go's Anthropic path means a new surface, not a new
  backend: the trait hook added here is per model, not per protocol.

## Evidence

Probed on 2026-10-22 against the live gateway:

- `POST /inference/go/openai/v1/chat/completions` with no `x-opencode-session` ->
  `400 {"type":"error","error":{"type":"MissingSessionID"}}`.
- The same request with the header and no credential -> `401 "Go requires a credential"`.
- `GET /inference/go/v1/models` with no credential -> `200`, 36 ids, no limit fields.
- `POST /inference/openai/v1/chat/completions` with a chat-family free model and no credential ->
  `403 FreeTierError`; the same with `User-Agent: opencode/1.0.0` -> identical `403`, so the gate is
  not the user agent.
- A Responses-family id answered identically on `/openai/v1/responses` and
  `/openai/v1/chat/completions` (`410 ModelDeprecated`), so the gateway resolves the model before
  the path.
- A key with no Go subscription -> `403 "An active OpenCode Go subscription is required to use Go
  models."` on a Go model.
