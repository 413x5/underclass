# 0007 - Config-Driven Model Catalog with Pass-Through

## Status

Accepted, and corrected in part by
[fork: Catalog Refresh Is Operator-Driven](fork/catalog-refresh-is-operator-driven.md): no backend's
catalog refreshes at boot except Claude's, and the rest refresh only on an operator action. The body
below is left unedited.

## Context

The upstream Codex endpoint's model list changes without notice (new models like `gpt-5.6-sol` or `gpt-6-astra` appear over time), while Copilot exposes a live `/models` endpoint. Hardcoding the catalog either rots or blocks new models.

## Decision

- The proxy stores a per-backend catalog (SQLite `catalog` table, editable via the web UI). Defaults are seeded from the model lists observed in the opencode source; Copilot's catalog is refreshed from its live `/models` endpoint on boot and on account add.
- `/v1/models` serves the **union** of backend catalogs (limits merged by max).
- Routing eligibility is catalog-driven: a model is served by backends whose catalog contains it.
- A model that appears in **no** catalog is treated as pass-through traffic and routed to the Codex backend, which upstream-validates. This makes new OpenAI models work with zero proxy changes.

## Consequences

- No recompile or restart is needed to add models (UI editor or `PUT /admin/api/catalog/{backend}`).
- Pass-through means typos in model names hit the upstream and fail there rather than at the proxy; the proxy's 404 surface is intentionally limited to unknown backends, not unknown models.
- Copilot catalog staleness is bounded by the boot-time and on-demand refresh endpoints.
