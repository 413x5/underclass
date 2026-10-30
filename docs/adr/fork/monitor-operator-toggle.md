# Monitor Operator Toggle

## Status

Accepted

## Context

`underclass top` was read-only: it read the monitor socket (or authenticated HTTP) and could only
navigate. Disabling an account required the browser admin page. The operator, already looking at the
dashboard and often reacting to a cooling or failing account, had to leave it, find the same account in
the admin UI, and click there.

Two facts constrain the change. The monitor socket is deliberately unauthenticated for local users and
serves only the snapshot (`src/main.rs` registers `/monitor` on it and nothing else), so it has no path
that could carry a write. And enable/disable already exists behind the admin token at
`POST /admin/api/accounts/{id}/enable|disable`, with the invariants in
[`enable_account`](../../../src/ui.rs): recovery clears scoped cooling deadlines only for accounts the
operator explicitly re-enabled.

## Decision

`top` gains one write, and only one: `d` asks to disable the selected account, `e` asks to re-enable a
`disabled` or `auth error` one, and neither contacts the server until `y` confirms. `n`/`Esc` cancels
locally.

- The request goes to the running server's authenticated admin API. It never uses the monitor socket,
  and the dashboard never opens the SQLite store for writing; the socket keeps its read-only shape.
- The action is derived from the status already on screen, so a keypress cannot do something invisible:
  `e` on an enabled account and `d` on a disabled one are refused with a notice rather than forwarded.
  A `cooling` account counts as enabled, because its deadline comes from upstream quota and enabling it
  would clear a deadline the operator did not ask to clear.
- The prompt names the account and the verb, so a stray `y` cannot act on a different row than the one
  the operator was reading.
- The call runs on its own thread and reports through the existing update channel, so a slow or wedged
  server cannot freeze the dashboard; failures surface as a footer notice.
- The dashboard stays useful without an admin token: the snapshot still arrives over the socket, and
  only the toggle reports the missing token.

## Consequences

- The dashboard is no longer strictly read-only, which is why the `top-observes-only` contract now
  states read-only for *data* and the narrower `top-operator-toggle` contract carries the single
  permitted write. The read-only monitor contract in `src/monitor.rs` still holds: the socket and the
  snapshot handler are untouched.
- A local `top` on the same host resolves the admin token from config or the database, the same way
  `underclass reset` does; a remote `top --url` needs `UNDERCLASS_UI_TOKEN`.
- The dashboard does not optimistically mutate its snapshot. The change appears on the next poll, one
  second later, so what is drawn always came from the server.
- The TUI is still not a general admin surface. Delete, re-login, and reset stay in the browser UI and
  their CLI commands; adding another write here needs its own decision, because each one widens what a
  single keypress can do.
