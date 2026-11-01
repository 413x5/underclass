# Catalog Refresh Is Operator-Driven

## Status

Accepted. This is a fork-local ADR: it carries no upstream number so a future `git pull` from
`main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

It supersedes in part the boot-refresh statements of upstream [ADR 0007](../0007-config-driven-model-catalog.md)
and [ADR 0011](../0011-github-copilot-backend.md), and of
[Cline Catalog from the Recommended-Models Feed](cline-catalog-from-the-recommended-models-feed.md).
The `cline-catalog-refreshed-on-boot` and `opencode-go-catalog-refreshed-on-boot` declaration
contracts were removed with the functions they annotated; `catalog-refresh-is-operator-driven` in
`CONTRACTS` replaces them.
Those records' bodies are left unedited; each gained a `## Status` pointer here, which is the
convention this directory already uses.

## Context

`underclass serve` refreshed the Copilot, Cline, and OpenCode Go catalogs before
`TcpListener::bind`, once per account of that backend. The Cline refresh reads the whole
`https://models.dev/api.json` document - a multi-megabyte body behind a 30 second timeout, fetched
through a client built for that read alone. All of it ran on the startup critical path.

That made a catalog read look like a failed start. `underclass-update.sh` probes the new process
for `200` on `/v1/models` for about 18 seconds before it prints rollback instructions, so a slow
models.dev read could fail a healthy update. A catalog refresh is also not what makes the proxy
able to serve: the stored catalog already describes every model, and a model the upstream has
stopped serving fails on its own response rather than at startup.

## Decision

- **Startup fetches no catalog for Copilot, Cline, or OpenCode Go.** The stored catalog is what the
  pool advertises until an operator asks for more.
- **The admin refresh endpoints are the only path that changes those three model sets**, alongside
  the two existing non-startup paths: onboarding an account, and editing the catalog in the admin
  UI or through `PUT /admin/api/catalog/{backend}`.
- **Claude keeps its boot refresh.** The dashboard has no Claude refresh button, so removing it
  would leave the Claude catalog unable to change without onboarding a fresh account. Adding such
  a button is a separate change.
- **Alias and identity refresh stay on boot.** Neither is a catalog refresh: aliases are derived
  from the stored catalogs, so without the boot build every alias would be missing until something
  else refreshed, and the identity pass only fills in labels for generically-labelled accounts.

## Consequences

- A restart no longer changes which models the pool advertises, so a model the operator removed by
  hand stays removed and a newly published one waits for a press of the refresh button.
- Readiness no longer depends on a multi-megabyte models.dev read, so the update probe sees the new
  process answer promptly.
- An updated lineup is one click away instead of being picked up silently at the next restart, and
  a `503 no_accounts` for a brand-new model is expected until that click.
- The Claude boot refresh is now the only startup catalog fetch, so any future delay on that path
  is visible rather than averaged with the other backends.
