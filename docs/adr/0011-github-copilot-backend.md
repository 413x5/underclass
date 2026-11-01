# 0011 - GitHub Copilot Backend

## Status

Accepted, and corrected in part by
[fork: Catalog Refresh Is Operator-Driven](fork/catalog-refresh-is-operator-driven.md): the Copilot
catalog is no longer fetched on boot. The body below is left unedited.

## Context

GitHub Copilot subscriptions (personal and enterprise) can serve OpenAI-family models and are a common second subscription source alongside ChatGPT/Codex. The flow is specified by opencode's `plugin/github-copilot` implementation, which we verified: device flow with client ID `Ov23li8tweQw6odWQebz`, long-lived OAuth token used directly as the bearer token (no exchange, no refresh), API base `https://api.githubcopilot.com` (or `https://copilot-api.<ghe-domain>` for enterprise), required headers (`X-GitHub-Api-Version: 2026-06-01`, `x-initiator`, `Openai-Intent`, `Copilot-Vision-Request` when the body carries images, `X-Interaction-Id` per session), and a live `/models` catalog filtered by usability (`policy.state != "disabled"`, prompt/output limits present).

## Decision

Implement Copilot as a second backend behind the `Backend` trait:

- Onboarding via the GitHub device flow; per-account optional enterprise domain captured at add time.
- The GitHub OAuth token is stored as the account's static credential; the token manager's refresh step is a no-op for this backend.
- Required Copilot headers are injected per request; image detection walks the JSON body for `image_url`/`input_image` parts to set `Copilot-Vision-Request`.
- Catalog fetched from `/models` on boot and on account add, stored per ADR 0007.
- 429 -> cooling (ADR 0004); 401/403 -> `AuthError` (re-login).

z.ai support was explored (its coding plan is API-key based: `api.z.ai/api/coding/paas/v4`, `ZHIPU_API_KEY`) and **deferred**: the `Backend` trait is the seam, and the keys-based flow is simpler than either implemented backend.

## Consequences

- Copilot accounts participate in the same flat pool and sticky map as codex accounts.
- `x-initiator` is always `user` from the proxy; opencode's own plugin sets `agent` for subagent sessions, a distinction the proxy cannot see through the OpenAI-compatible surface. Accepted as a limitation.
- Enterprise and personal Copilot accounts can be mixed in one pool; the enterprise domain lives on the account.
