# Model Metadata: Sources, Precedence, and What We Refuse to Copy

## Status

Accepted. This is a fork-local ADR: it carries no upstream number so a future `git pull` from
`main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

It extends two earlier records without editing either, because `docs/adr/` is append-only:

- [Only Account-Backed Models Are Advertised](account-backed-model-advertisement.md) still governs
  which backends' rows are listed. This record adds a second read-time filter over those rows: one
  recorded as `deprecated` is stored but never advertised, and never offered by the UI snippet or
  `underclass connect`.
- [Cline Catalog from the Recommended-Models Feed](cline-catalog-from-the-recommended-models-feed.md)
  still makes the feed the set authority. This record adds that the feed is also what decides
  retirement, that specs come from models.dev `cline-pass`, and that a retirement is recorded on the
  row rather than expressed by dropping it from a table.

## Context

`GET /v1/models` advertised seven fields per entry — `id`, `object`, `name`, `context_window`,
`max_input_tokens`, `max_output_tokens`, `owned_by` — and nothing else, while both pooled upstreams
publish far more. Measured against a live pool on 2026-10-31:

| source | models | fields per entry |
|---|---|---|
| `opencode.ai/inference/go/v1/models` | 28 | 18 |
| `api.cline.bot/api/v1/models` | 435 | 4 |
| `api.cline.bot/api/v1/ai/cline/models` | 435 | 18, but `capabilities: null` on all 435 |
| `api.cline.bot/api/v1/ai/cline/recommended-models` | 26 across four buckets | 4 |
| models.dev `cline-pass` | 18 | 20+ |

Three consequences were reported in issue #25 and are closed here:

1. **Unknown limits read as zero.** 410 `cline/*` rows and the 8 `opencode-go/*` rows whose source
   omits `limit.input` advertised `0`, so a client sizing a request from `max_input_tokens`
   computed a zero budget.
2. **No capability metadata reached a client**, so every consumer kept a hand-maintained duplicate
   catalog that drifted from the backends.
3. **Two retired ClinePass models stayed routable.** `cline-pass/mimo-v2.5` and
   `cline-pass/mimo-v2.5-pro` both answer `404 model not found` from
   `POST /api/v1/chat/completions`, and both were named by `default_catalog()`.

## Decision

### Unknown is not zero

`context_window`, `max_input_tokens`, and `max_output_tokens` are present on every entry and are
JSON `null` when no source published the value; the stored `0` means "unpublished" and never
reaches the wire. The keys are never dropped, because a client that destructures them would break.
An alias takes each limit as the widest value among its servable concretes, and is `null` only when
none of them published one — an unknown sibling neither wins a comparison nor drags a known one
down. Aliases advertise the union of their concretes' reasoning ladder and tool support, and their
`scope` when every servable concrete agrees on one surface; cost, modalities, knowledge, and wire
family are omitted for an alias, because no single value would be true of every concrete.

### `input = context` when the source omits `limit.input`

Go's feed publishes `limit.input` on four of its 28 models. Absence means "input equals context",
so ingest records `input = context` and the row emits a real number. `0` therefore means only "no
source has ever reported this value", which is a state no current backend produces for a model it
lists with a context window.

### One `metadata` column, projected into a typed listing shape

`ModelInfo` carries a `metadata` block: `reasoning` (supported plus the per-model effort ladder),
`tools`, `structured_output`, `modalities { input, output }`, `cost`, `prompt_cache`, `knowledge`,
`wire`, `reasoning_field`, and `status` (`available` / `deprecated`). Every member is
`serde(default)`, so a row written by an older build deserializes unchanged.

Storage is one `catalog.metadata TEXT NOT NULL DEFAULT '{}'` column, added through the existing
`PRAGMA table_info` + `ALTER TABLE ADD COLUMN` migration. The block is a list-bearing structure and
one ALTER is one decision; a column per attribute would make every future capability another
migration. Serialization is serde on a struct, so identical rows are byte-stable and a refresh does
not churn the store.

`src/listing.rs` projects the stored block into an explicit, typed response shape rather than
passing the JSON through, so the wire contract cannot drift when an internal field is added or
renamed. The seven original keys keep their names and meanings; `scope` (`credits`, `cline-pass`, or
absent for an account-wide quota) and `metadata` are additive.

### Per-backend precedence

**OpenCode Go.** `GET {base}/v1/models` is authoritative for the set, the limits, the
capabilities, the modalities, the cost, and the surface. `capabilities.reasoning.effort` becomes the
ladder, `capabilities.reasoning.supported` the boolean, `modalities` the modality lists,
`cost.text` the base price and `cost.text.input.cache` the prompt-cache price, `awareness`-style
`knowledge` the knowledge date, and `api` the surface. `WIRE_FAMILY` stays the source of routing:
the `api` member agrees with it on every id present in both, so it is a cross-check and never a
replacement. A model that reports `reasoning.supported: true` with no `effort` array is
toggle-only: the boolean is recorded and the ladder stays empty.

Go's listing is authoritative for every limit it publishes, so it replaces a stored number; a key
the listing omits keeps the stored value or the seed. That is the opposite of what Cline needs,
because Go's listing does carry limits.

**Cline.** The **set** is the recommended-models `clinePass` bucket: it is public, it matched live
availability on every probed id, and it already carries the ids models.dev lacks. The **specs** are
models.dev `cline-pass`, used only where a row does not already know a value.

`api.cline.bot` is **never** used for cline-pass specs. Its listings contain zero `cline-pass/`
ids, and `/ai/cline/models` describes the upstream provider rather than the ClinePass quota:
`943718` max output for a model whose ClinePass reference is `384000`.

**models.dev** is a new outbound read and therefore subject to the seed-floor rule: a successful
read enriches the catalog, and any failure (network, non-2xx, malformed body, or a document without
the provider read) leaves the stored catalog untouched and falls back to the embedded seed. Nothing
depends on models.dev being reachable, and no row regresses because a third-party feed was down.

### models.dev caveats, as measured

Do not inherit these; they are the reason models.dev is advisory rather than authoritative.

| Issue | Models |
|---|---|
| Marked `status = "deprecated"` **and** correctly `404` | `deepseek-v4-flash`, `glm-5.2`, `kimi-k2.6`, `kimi-k2.7-code` |
| **`404` but not marked deprecated** | `mimo-v2.5`, `mimo-v2.5-pro` |
| **Live but absent from models.dev** | `deepseek-v4.1-flash-1017`, `kimi-k3.1-pro`, `minimax-m3.1-flash` |

Two further inaccuracies, both verified on the wire:

- `glm-5.3.toml` declares `compatibility = { reasoning_field = "reasoning_content" }`, but Cline
  streams reasoning in `delta.reasoning` plus `delta.reasoning_details`. `delta.reasoning_content`
  never appears. (`src/responses.rs` already reads both, so nothing breaks today; copying the value
  would still be wrong.)
- models.dev carries no `limit.input` for any `cline-pass` model, so it cannot fill our `input`
  field — which is why `input = context` is an ingest rule, not a listing one.

### Retirement is recorded, not hard-coded

A retired model is a stored row with `status = deprecated`, and the listing filter drops it: it is
never advertised, never keeps an alias alive, and never contributes to an alias's limits. The two
other surfaces that offer models to a client — the opencode snippet and `underclass connect`'s
provider block — apply the same rule, so a retired id is not handed to a client in any form. The
operator's view is `GET /admin/api/catalog/{backend}`, which still shows every stored row including
a retired one; that is where a retirement is inspected.

The ClinePass set authority is the feed, so `retire_absent` retires a `clinePass` row exactly when
the feed was read and does not name it. This catches the next retirement with no code change, which
is why `mimo-v2.5` and `mimo-v2.5-pro` are simply removed from `default_catalog()` rather than kept
behind a deny-list.

Two rules keep a retirement from being lost again, and both are easy to break:

- **A read that told us nothing changes nothing.** An unread feed, or one whose `clinePass` bucket
  came back empty because it was renamed or reshaped, leaves every stored status alone. If it
  cleared them instead, one transient outage would re-advertise an id the provider answers `404`
  for — the exact failure this catalog exists to prevent.
- **The refresh never deletes a subscription row.** `set_catalog` clears the backend's table, so a
  row this refresh did not produce is re-added with the status it already had *before* the feed
  decides statuses. Otherwise a feed-only model disappears from the operator's catalog, and a
  documented row is re-created as `available`, silently undoing an earlier retirement.

A retirement therefore only ever moves in one direction per read: a feed that names the model
un-retires it, and nothing else does.

The ~10 `opencode-go/*` ids that Go's listing does not name are deliberately **not** removed:
absence from a listing is not evidence of retirement, and several are free/preview specials that
may be legitimately unlisted. They remain stored and advertised.

Live probing is not part of the refresh path: the `404` evidence came from real completions, which
cost tokens and quota. The recorded status is maintained at refresh time from a feed that is free to
read.

## Consequences

- A client can size a request from `max_input_tokens` without special-casing zero, tell "we do not
  know" from "no capacity", read a model's effort ladder before sending `reasoning_effort`, and tell
  a subscription model from a pay-per-token one.
- A capability a backend did not publish stays absent. No value is synthesized: an empty ladder
  means toggle-only, `None` means unpublished, and an unknown surface name is not mapped onto a
  known one.
- The catalog schema only grows, so a new binary reads a `pool.db` written by an older one and vice
  versa; an older binary ignores a `metadata` column it does not know about.
- models.dev being unreachable degrades capability coverage, not the catalog: limits still come from
  the seed table, Go's listing, and Cline's feed.
- Capability metadata describes the model before the first byte of a response. Per-request
  capability negotiation, routing by capability, and `WIRE_FAMILY` changes remain out of scope.
- The stored `status` is only as fresh as the last refresh. A backend that retires a model between
  refreshes leaves it advertised until the next one, which is the same freshness the limits have.
- Retirement is a property of the routable catalog as well as the listing: `PoolCore::set_catalog`
  and `build_alias_table` both drop a `deprecated` row, so keeping it stored for the operator never
  keeps it reachable.
