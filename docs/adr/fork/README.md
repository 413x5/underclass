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
