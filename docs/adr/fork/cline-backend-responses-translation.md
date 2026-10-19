# Cline Backend with Responses Translation

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md).

## Context

Cline exposes an OpenAI-compatible Chat Completions API at `https://api.cline.bot/api/v1` authenticated with a static API key (`Authorization: Bearer <key>`), created at `app.cline.bot` → Settings → API Keys. Two billing surfaces share the endpoint:

- Credit-based models named `provider/model` (e.g. `anthropic/claude-sonnet-4-6`, `openai/gpt-5.4`), listed publicly by `GET /api/v1/models`.
- The **ClinePass** subscription, whose open-weight models use the `cline-pass/` namespace (e.g. `cline-pass/kimi-k3`, `cline-pass/deepseek-v4.1-flash`). These are documented but not returned by the public model listing.

Cline has no `/responses` endpoint, and its public model listing carries no context or token limits. `402 Payment Required` signals exhausted credits — account-level, so it behaves like quota exhaustion. `403` is returned for per-resource access (a model outside the account's plan) and must not eject a valid key from rotation.

## Decision

Add Cline as a third backend behind the `Backend` trait.

- **Onboarding** is an API-key paste in the admin UI (or `POST /admin/api/flows` with `{"backend":"cline","api_key":"…"}`) rather than a device flow. The key is verified against `GET /api/v1/users/me` before persistence, stored in the account's `refresh_token` column like the Copilot OAuth token, and never refreshed. The account label defaults to the profile email.
- **Model namespace**: every Cline model is exposed as `cline/<upstream id>`, so `cline-pass/kimi-k3` becomes `cline/cline-pass/kimi-k3` and the credit-based `anthropic/claude-sonnet-4-6` becomes `cline/anthropic/claude-sonnet-4-6`. The prefix keeps Cline IDs out of the pre-existing Codex/Copilot ID space and lets routing target the backend by catalog alone. `Backend::prepare_body` strips the prefix before the upstream call.
- **Catalog**: `parse_models_response` prefixes IDs and drops `:batch` variants. Because the public listing omits ClinePass, the documented subscription models are seeded as the default catalog and merged into every refresh, so a ClinePass subscription stays routable.
- **Responses translation**: `Backend::responses_via_chat` marks Chat-Completions-only backends. For those, a `/v1/responses` request body is converted by `responses::request_to_chat` (instructions → system message, `input` items → messages/tool calls, flattened function tools, `max_output_tokens` → `max_tokens`, `text.format` → `response_format`), and the upstream body is converted back — `completion_to_response` for non-streaming bodies, `ChatStreamTranslator` for SSE — before reaching the client. The translator is chunk-boundary independent and emits `response.created`, item/content deltas, and exactly one terminal `response.completed` / `response.incomplete` / `response.failed`.
- **Failure classification**: `402` cools the account for the configured cooldown; `403` maps to `Ok` so the key stays healthy; `401` follows the shared auth-failure path.

## Consequences

- Cline accounts join the flat pool and sticky map with no pool-core special casing; routing, cooldown readmission, failover limits, and accounting are unchanged.
- Translated requests lose Responses-only affordances (encrypted reasoning, `previous_response_id`, hosted tools). Only `function` tools are forwarded; unsupported tool types are dropped rather than sent upstream to fail.
- Cline's public catalog carries no context limits, so `/v1/models` reports zeros for Cline entries until an operator edits the catalog in the UI.
- Streaming client cancellation ends the upstream stream as before (pre-first-byte failover only); a translated stream that ends without a terminal event surfaces as `response.failed` with `stream_truncated`.
- Future Chat-Completions-only providers reuse the same trait hook and translator.
