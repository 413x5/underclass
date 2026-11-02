# Vercel AI Gateway Backend

## Status

Accepted.

## Context

Vercel AI Gateway is a managed gateway at `https://ai-gateway.vercel.sh/v1` that fronts
hundreds of models across many providers (OpenAI, Anthropic, Google, Meta, Mistral, ...),
serving them over the OpenAI Chat Completions and Responses APIs. Authentication is a
single AI Gateway API key (credits-based, one balance across every provider). The
operator asked for it to join the pool alongside Cline and OpenCode Go.

Facts established by probing the live gateway:

1. **The listing is public, but a provided key is still validated.** `/v1/models` returns
   the full catalog (498 entries) without a key, so a request that omits the bearer is never
   evidence about the key. A request that carries a bad key is rejected with `401`, so
   `verify_key` sends the bearer and treats `401`/`403` as a rejected key.
2. **Model ids are `provider/model`.** The id Vercel expects upstream is already namespaced by
   provider (`openai/gpt-6-luna`, `anthropic/claude-opus-4.5`), so the pool-facing id keeps
   that shape under a `vercel/` prefix: `vercel/openai/gpt-6-luna`.
3. **Only language models are routable.** The listing also carries image, video, embedding,
   speech, transcription, realtime, reranking and evaluation models; those do not speak the
   OpenAI chat/responses wire protocol the pool routes, so they are dropped at parse time.
4. **Credits are account-wide.** A single API key spends one credit balance across every
   provider, so there is no per-billing-surface cooling and no per-model cooldown beyond
   the upstream's own `429`.
5. **Provider selection is a request-body concern.** Vercel's `providerOptions.gateway.order` /
   `only` / `sort` travel in the request body, which underclass already forwards for
   OpenAI-shaped backends, so per-request provider ordering/filtering works with no extra
   code. The model id itself pins the provider.
6. **Catalog metadata comes from models.dev.** The `vercel` provider entry carries limits,
   cost, modalities and reasoning options for 417 of the models, so the refresh can enrich
   rows the same way Cline's does.

## Decision

Add a sixth backend, `BackendId::Vercel` ("vercel"), implemented in `src/vercel.rs`:

- API-key onboarding via `add_account` / `start_vercel_flow`, mirroring Cline: verify the
  key, store it in `refresh_token`, label from operator input or fall back to "vercel".
  `refresh_on` returns false for every status, because the key is static.
- `surfaces()` = `[OpenAI]`; `rewrite_url` keeps the client's path under
  `/v1/responses` or `/v1/chat/completions`; `prepare_body` strips the `vercel/` prefix so
  the upstream sees `provider/model`.
- Dynamic catalog: `refresh_catalog` reads `/v1/models`, keeps language models only,
  namespaces them `vercel/<provider>/<model>`, carries forward stored limits/status, and
  never shrinks. Operator-driven, like Cline and OpenCode Go.
- Account-wide quota: empty scope on every row, so a `429` cools the whole account.
- Credits alias: `vercel/` joins the `credits/` prefix set, so `credits/<base>` aggregates
  Vercel models with other credits-backed backends.

## Consequences

- A Vercel account adds ~320 language models to the pool, all addressable as
  `vercel/<provider>/<model>` and as `credits/<base>`.
- Vercel models are OpenAI-shaped, so they compete with Codex/Copilot/Cline/Go on the
  flat pool for the same surface.
- No migration is needed: Vercel introduces no persisted-identifier rename.
- The user agent knob (`vercel_user_agent` / `UNDERCLASS_VERCEL_USER_AGENT`) is honored on
  the module's own requests and on inference, matching Cline and OpenCode Go.
