# Client User Agent Forwarding and the `opencode_go` Backend Name

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See
[README](README.md).

## Context

The fork's own backends identified themselves to their upstreams with a hardcoded user agent.
Both strings discard the identity of the client that actually made the request, even when a real opencode client is the
caller.

The upstream backends are a different matter, and are deliberately out of scope:

- **Claude** must forward the client's `User-Agent`: Anthropic validates the `claude-cli/` value
  (ADR 0023).
- **Codex** and **Copilot** are upstream's implementations. Their user agent, and Codex's
  `originator: opencode`, are upstream decisions this fork does not revise.

A second, unrelated problem surfaced in the same review: the Go backend was named `go` in every
identifier it owns - `BackendId::Go`, `GoBackend`, `go_cooldown_secs`, `refresh_go_catalog`, the
`/admin/api/catalog/refresh-go` route, the UI's `<option value="go">`, and the `go/` model
namespace. "Go" collides with the language and reads ambiguously next to `opencode`; the module has
always been `opencode_go`, so the identifiers now match the module.

## Decision

**1. The fork's backends forward the client's user agent.**

Cline and OpenCode Go resolve the outbound `User-Agent` through one pure helper,
`user_agent::resolve`, with this precedence:

1. the operator's configured override, when set and usable as a header value;
2. the client's own `User-Agent`, when the client sent a non-empty one;
3. `user_agent::DEFAULT`, `underclass/<version>`.

The override is configured as `user_agent` in `~/.config/underclass/config.toml` (shared default)
or per backend as `cline_user_agent` / `opencode_go_user_agent`, with the matching
`UNDERCLASS_*` environment variables. It is injected into each backend at construction, so no
global state is shared and tests stay isolated.

Resolution happens inside each backend's `inject_headers`, which already receives the client's
headers. The proxy path is unchanged: no backend name is consulted outside its own implementation
(CONTRACTS `backend-agnostic-core`), and no client header other than the user agent starts flowing
to these two upstreams.

The override also covers the requests those two modules make themselves - catalog refresh, identity
lookup, and key onboarding - so an operator who sets an identity sets it everywhere that module
speaks to its upstream.

**2. Go is renamed `opencode_go` everywhere it is underclass's to name.**

`BackendId::OpencodeGo`, `OpencodeGoBackend`, `opencode_go_cooldown_secs` /
`opencode_go_cooldown_ms`, `refresh_opencode_go_catalog`, `/admin/api/catalog/refresh-opencode-go`,
the UI's `<option value="opencode_go">`, the `opencode_go/` model namespace, and the contract ids
whose prefix was `go-` (`opencode-go-credential-swap` and its siblings).

Two things are deliberately **not** renamed:

- The Go gateway's own names — its hostname (`https://opencode.ai/inference/go`), its Console path,
  and the `x-opencode-session` header it requires — are the upstream's, not ours.
- `docs/adr/` records and their filenames. The ADR policy is append-only, so
  `go-serving-set-and-failure-scopes.md` and `opencode-go-backend.md` keep their paths and titles;
  renaming them would edit a record and break links to it. This record is the note that reconciles
  the naming, not a rewrite of theirs.

## Consequences

- **The `opencode_go/` namespace is a breaking rename for clients and catalogs.** `underclass
  connect` writes these ids into the operator's opencode config, and catalog rows are keyed by
  `model_id`, so a `go/…` entry becomes unroutable. This record accepts that break rather than
  carrying a migration or a second accepted prefix.
- **No store migration ships for the backend string.** `BackendId::as_str()` is persisted in
  `accounts.backend`, `catalog.backend`, `bindings.backend`, `usage_records.backend`,
  `upstream_events.backend`, and stats keys, and read back through `BackendId::parse()`, which
  falls back to `BackendId::Codex` for an unknown value. A database that still holds `go` rows is
  therefore **not** read-compatible: those accounts would be reclassified as Codex. The deployment
  this change is for starts from a database that predates the Go backend and re-seeds the catalog
  on boot, so a migration is unnecessary here. Any other deployment holding Go rows must re-key
  those accounts before upgrading.
- The upstream sees the traffic as the client it actually is. A client that sends none still gets `underclass/<version>`, and an operator who wants a specific identity sets one.
- The operator override is not authenticated and is applied verbatim; a wrong value degrades those
  two backends and nothing else. It does not reach Codex, Copilot, or Claude.
- A client can no longer be told apart from underclass by user agent alone on these two backends.
  Request accounting, sticky routing, and quota cooling are unaffected: none of them reads the
  user agent.
