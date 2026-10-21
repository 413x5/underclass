# 0024 - Surface-Partitioned Routing

## Status

Accepted

## Context

ADR 0010 gave underclass a single OpenAI-shaped surface and a flat pool: every backend served
`/v1/responses` and `/v1/chat/completions`, so model eligibility alone decided which backend
could answer a request.

The Claude Code backend (ADR 0023) breaks that assumption. Its upstream is the Anthropic
Messages API: a different request schema (`system` blocks, content blocks, `metadata.user_id`),
a different response schema (SSE events named `message_start` / `content_block_delta` /
`message_delta` / `message_stop`), and a different credential shape. Two consequences follow.

**A Claude account must never answer an OpenAI-shaped request.** The upstream would reject the
body, but only after the request was sent, and the client would see a protocol error instead of
a routing decision. Routing has to happen before the request leaves the proxy.

**An OpenAI-shaped request must never land on a Claude account**, for the same reason in the
other direction.

The pool core had no way to express this. `PoolCore::select` took a model name and consulted
the per-backend catalogs; every backend was assumed to speak every protocol. The unknown-model
fallback was a hardcoded `BackendId::Codex`.

## Decision

- A request carries a **surface**: the wire protocol it speaks. `Surface::OpenAI` covers
  `/v1/responses` and `/v1/chat/completions`; `Surface::Anthropic` covers `/v1/messages` and
  `/v1/messages/count_tokens`.

- Backends declare the surfaces they serve, in `provider::Backend::surfaces`. Codex and Copilot
  declare `OpenAI`; Claude declares `Anthropic`. The declaration lives with the backend, so the
  proxy never names a backend (CONTRACTS `backend-agnostic-core`).

- `PoolCore` learns those declarations at construction (`PoolCore::new(store, surfaces)`) and
  selection takes the surface: only accounts whose backend declares it are eligible, and the
  saturation deadline is the minimum over *that surface's* eligible accounts. A pool whose
  OpenAI accounts are all cooling still serves an Anthropic request from a healthy Claude
  account.

- The unknown-model fallback is now "the first backend that declares the surface", in the
  stable order of `BackendId::all()`. On the OpenAI surface that is still Codex, preserving
  ADR 0007's pass-through behaviour; on the Anthropic surface it is Claude.

- The surface is a property of the route, not of the model. A model that is missing from every
  catalog still routes to the surface's default backend, so a client never receives a
  protocol-shaped answer it did not ask for.

## Consequences

- The pool core gained one parameter and one map. It stays pure and synchronous: the surfaces
  are data, and `now_ms` is still injected (CONTRACTS `pure-sync-core`).
- Adding a fourth backend that speaks a new protocol is a trait method and a registration, not
  a routing change - the same property ADR 0010 claimed for adding a backend.
- The two surfaces share one pool, one sticky map, one health model, and one accounting store.
  A sticky key therefore pins a conversation to an account regardless of which surface it
  arrived on, and the per-surface eligibility keeps the two protocol families apart.
- Property tests cover the partition: an OpenAI-shaped selection never returns a Claude account
  and the other way round.
