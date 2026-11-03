# Stream Failures End the Body Cleanly, and No Timeout Cuts a Long Response

## Status

Accepted. This is a fork-local ADR: it deliberately carries no upstream number so a future
`git pull` from `main` cannot collide with a newer upstream `NNNN` record. See [README](README.md).

## Context

A multi-minute pr-agent review through underclass failed with

```
litellm.APIConnectionError: OpenAIException - Response payload is not completed:
<TransferEncodingError: 400, message='Not enough data to satisfy transfer length header.'>
```

underclass had answered `200` immediately and then streamed for 105 seconds, recording
`stream.finished outcome=client_closed`. Neither preflight nor underclass timed the request out: the
upstream stalled, and the client aborted.

Two separate defects were behind that report.

**A truncated frame.** When the upstream transfer failed after a 2xx body had started,
`src/proxy.rs` returned `Err(io::ErrorKind::Interrupted)` from the body stream for an untranslated
response. Hyper honoured that by dropping the connection rather than writing the chunked
terminator, so the client's HTTP layer failed with a framing error and the model client raised a
connection error. The client learned nothing about what happened. The translated path already
closed with a single `response.failed`, so the two dialects behaved differently for the same event.

**Unbounded waiting as the only option.** The inference path has no request or read deadline, which
is what a long review needs, but it also had no way for an operator to end a genuinely dead stream
other than killing the process.

## Decision

### An upstream failure after the response has started ends the body cleanly

- The proxy never aborts the client's body once it has begun. It emits exactly one terminal error
  event and then ends the body normally, so a `Content-Length` or a chunked terminator always
  arrives.
- The code is stable and shared by every dialect and by the usage row:
  `upstream_stream_error`. The message names the backend and says the stream ended early; it never
  carries upstream message text.
- The event is written in the dialect the client is reading. Chat Completions and the OpenAI
  Responses stream get a `data:` frame carrying an `error` object (the Responses stream prefixed
  with `event: error`), and the Anthropic surface gets `{"type":"error","error":{...}}`.
- A translated `/v1/responses` stream keeps its single translator terminal event
  (`response.failed`, produced by `ChatStreamTranslator::finish_on_error`) and gains no second one.
  A body the upstream already terminated gets nothing added.
- A buffered 2xx body (`response.bytes()`) keeps its `502 upstream_error`, now worded so a client
  can act on it.
- Accounting is unchanged in shape: `UsageFinish` still records exactly one row per attempt with
  the tap's own outcome and `saw_output`, and records `upstream_stream_error` when that is the code
  the client was told. Failover is untouched: post-output failures are never retried
  ([pre-output-failover](../../../CONTRACTS)).

### Nothing on the inference path has a wall-clock deadline

| Bound | Value | What it bounds | On expiry |
|---|---|---|---|
| upstream `connect_timeout` (`upstream_connect_timeout_secs`) | 10s | TCP/TLS connect only | attempt fails over |
| pre-output hold (`pre_output_hold_secs`, `pre_output_hold_bytes`) | 60s, 1 MiB | how long the first output is held back so a capacity failure can fail over | holding stops; streaming continues |
| stream idle timeout (`stream_idle_timeout_secs`) | `0` = off | how long an established stream may produce nothing | the body ends with the terminal error event above |
| graceful-shutdown drain | 5s | shutdown only | remaining requests are aborted and recorded `client_closed` |

- The pre-output hold (`pre_output_hold_secs` / `pre_output_hold_bytes`) never fails a request. Its
  expiry only means the response is no longer held, so a slow first token is delivered rather than
  cut.
- No inference request carries a `reqwest` per-request timeout. The other `.timeout(...)` calls in
  the tree bound non-inference work: catalog reads, the models.dev corpus, Codex reset and usage
  calls, and the CLI clients.
- Every bound above is a key in `config.example.toml` with its default, so the whole timeout
  surface is visible in one file (`upstream_connect_timeout_secs`, `pre_output_hold_secs`,
  `pre_output_hold_bytes`, `stream_idle_timeout_secs`). The idle timeout is off by default.
- The idle timeout is opt-in (`stream_idle_timeout_secs` / `UNDERCLASS_STREAM_IDLE_TIMEOUT_SECS`)
  and ends the body per the section above rather than dropping the connection.

## Consequences

- A client that reads a response to the end always gets a completely framed body, and the last
  thing it reads on an upstream failure is a terminal error event naming the condition.
- A client with its own read timeout still aborts a stalled stream on its side; underclass can no
  longer make that abort look like a protocol error.
- A long review runs as long as the upstream keeps producing. An operator who wants dead streams
  ended sets an idle timeout generous enough for the longest legitimate silence.
- The recorded `error_code` for a synthesized failure matches what the client saw, so a usage row
  and a client's error report can be joined.
