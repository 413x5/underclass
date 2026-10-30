# 0010 - Unprefixed OpenAI-Shaped Routing over Hidden Backends

## Status

Accepted

## Context

The proxy pools multiple subscription sources (backends): **codex** (ChatGPT OAuth) and **copilot** (GitHub OAuth). Clients should not need to know which backend serves a model; they think in terms of *models*, and the same model (e.g. `gpt-5.4`) can be served by more than one backend.

Earlier iterations considered provider-prefixed URLs (`/openai/v1/...`, `/codex/v1/...`) and were rejected: the prefix forces clients to make backend decisions that the pool is better positioned to make.

## Decision

- Single OpenAI-shaped surface: `/v1/responses`, `/v1/chat/completions`, `/v1/models` - indistinguishable from pointing at the OpenAI API. `/v1/models` advertises only models a configured account can serve: the fork filters each backend's stored catalog to the backends that have an account, so it never offers an id the pool cannot route. See [the fork record](fork/account-backed-model-advertisement.md).
- Backends are invisible to clients. Backend selection is **model-eligibility-driven**: the requested model is served by any healthy account whose backend catalog contains it; all such accounts compete in one **flat pool** ordered by least-in-flight.
- Models absent from every catalog pass through to the codex backend (ADR 0007).
- Sticky bindings bind to `(backend, account)`; rebinds prefer the same backend (ADR 0003).
- Saturation and health are evaluated per model across all serving backends.
- Unknown URL paths under other prefixes (admin/UI aside) 404.

## Consequences

- Client config is trivial: `baseURL = http://127.0.0.1:8080/v1`.
- Adding a future backend (z.ai was considered and deferred) means implementing the `Backend` trait and registering it - no routing changes.
- The flat-pool policy spends subscriptions uniformly; operators who want to conserve one pool (e.g. Copilot premium requests) must disable accounts manually. Weighted/priority strategies were considered and deferred.
- Codex and Copilot request shapes differ slightly (responses vs chat/completions); both upstreams accept both paths, so no protocol translation is needed.
