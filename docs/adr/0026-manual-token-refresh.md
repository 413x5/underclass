# 0026 - Manual Token Refresh

## Status

Accepted

## Context

Codex refresh tokens rotate on a 15-minute scheduled tick once they are a day old (ADR 0019), and a
`401`/`403` on a live request forces one refresh before the account is marked `AuthError`
(ADR 0018). Neither gives the operator a way to refresh a credential on demand.

On 2026-10-28 the underclass egress path was dead for two days (its Tailscale exit node had gone
offline), so every scheduled rotation failed with a network error. After egress was restored the
operator had to wait for the next tick to learn whether the credentials were still good. Doing the
refresh outside the server is unsafe: Codex and Anthropic rotate the refresh token on every grant
(`codex-refresh-rotates`), so a refresh made outside the server can invalidate the token it
holds.

For an `AuthError` account the only recovery levers were **Enable** (readmit blindly) and
**Re-login** (a full device flow), even though a refresh alone often proves the credential is
fine.

## Decision

- `POST /admin/api/accounts/{id}/refresh` calls `TokenManager::force_refresh` once, so a manual
  refresh shares the per-account single-flight lock and write-through persistence with every other
  refresh.
- Backends whose `force_refresh` does not reach an issuer (`refreshes_with_issuer`: Copilot, whose
  GitHub token never expires) are refused with `409 not_refreshable`, because a no-op proves
  nothing about the credential.
- A failed refresh returns `502 refresh_failed` and changes nothing, matching the scheduled-rotation
  rule that a transient issuer or network failure must not remove a working account.
- A successful refresh applies `tokens::status_after_manual_refresh` (pure): an `AuthError` account
  becomes `Healthy`, unless the refreshed token's plan would be immediately disabled by the free-plan
  gate (ADR 0025). `Cooling` and `Disabled` are left untouched; quota deadlines and operator
  disables keep their existing meaning.
- `underclass refresh <email-or-id>` and `underclass refresh --all` drive the endpoint through the
  admin API, like `underclass reset`. The admin UI gets a **Refresh token** button per account.

## Consequences

- After a network or issuer outage the operator can confirm every credential in one command instead
  of waiting up to 15 minutes per tick.
- `AuthError` gains a cheaper recovery path that requires positive evidence (a successful grant)
  rather than an unconditional **Enable**. A real `AuthError` account still needs **Re-login**:
  its refresh fails and nothing changes.
- A manual refresh rotates the stored refresh token and resets `token_refreshed_at`, which pushes
  the next scheduled rotation out by a full interval. That is intended.
