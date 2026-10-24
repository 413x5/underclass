# Fork-local ADRs

This directory holds architecture decisions made on this fork that have no counterpart upstream.

## Why it exists

Upstream owns `docs/adr/NNNN-title.md` and numbers sequentially. A fork that also numbers
sequentially will eventually pick the same number as a newer upstream ADR. Git reports no conflict
for that case — both files simply exist — so the duplicate is silent. Editing the `- Index:` line in
`AGENTS.md` also collides on every upstream ADR addition, because that index is a single line.

Keeping fork decisions in `docs/adr/fork/` with title-only filenames avoids both problems:

- Upstream never creates this directory, so upstream ADRs and fork ADRs never share a path.
- Fork ADRs carry no number, so none can be reused.
- The fork index lives here, so `AGENTS.md`'s index line is left to upstream.

## Convention

- Filename: `kebab-case-title.md`, no numeric prefix.
- Same structure as upstream MADRs: title, `## Status`, `## Context`, `## Decision`,
  `## Consequences`.
- Add new records to the list below.

## Fork decisions

- [Cline Catalog from the Recommended-Models Feed](cline-catalog-from-the-recommended-models-feed.md)
  — the catalog's ClinePass and free models come from Cline's public recommended-models feed
  (`clinePass` and `free` buckets), which supersedes the hand-maintained-table decision in the
  record below; the table degrades to a fallback and a source of display names.
- [Cline Backend with Responses Translation](cline-backend-responses-translation.md) — third
  backend (API-key onboarding, `cline/` model namespace, ClinePass subscription models) plus
  Responses-to-Chat-Completions translation for Chat-Completions-only upstreams.
- [Per-Surface Quota Cooling](per-surface-quota-cooling.md) — catalog-declared quota scopes so a
  `402` on Cline credits does not remove the ClinePass subscription from rotation. It supersedes
  the account-wide `402` cooldown in [Cline Backend with Responses Translation](cline-backend-responses-translation.md),
  whose record is left as accepted and unedited.
- [Prompt Cache Scope Across Subscriptions](prompt-cache-scope-across-subscriptions.md) — measured
  finding that Cline's prompt cache is shared across API keys, so cross-subscription reuse already
  happens and no derived sticky key is needed.
- [Quota Dimensions Compose in Selection](quota-dimensions-compose-in-selection.md) — how the
  fork's per-billing-surface cooling and upstream's per-model cooling coexist in one eligibility
  predicate, and why neither subsumes the other.
- [Go Serving Set and Failure Scopes](go-serving-set-and-failure-scopes.md) — corrects two decisions
  of the Go backend record: the module's table, not Go's listing, decides what is routable, and a
  `402` naming an unavailable endpoint cools the model rather than the account.
- [OpenCode Go Backend over Go's OpenAI Surface](opencode-go-backend.md) — fifth backend (API-key
  onboarding, `opencode_go/` model namespace, sticky session header) and the per-model wire-family
  choice that keeps Go's Anthropic- and Gemini-only models out of the catalog.
- [Client User Agent Forwarding and the `opencode_go` Backend Name](client-user-agent-forwarding.md)
  — Cline and OpenCode Go present the client's own user agent (with an operator override, and the
  upstream backends' behaviour untouched), and Go is renamed `opencode_go` in every identifier
  underclass owns, including the model namespace.

- [Graceful Shutdown Drain on SIGTERM](graceful-shutdown-drain.md) — `underclass serve` reacts to
  SIGINT/SIGTERM, drains in-flight requests for a bounded 5 seconds via `with_graceful_shutdown`
  on both listeners, and lets `UsageFinish` record aborted streams as `client_closed` instead of
  dropping them.
