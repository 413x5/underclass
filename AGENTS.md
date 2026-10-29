# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository.

## What this is

`underclass` is a local LLM proxy that pools multiple ChatGPT/Codex, GitHub Copilot, and Cline subscriptions behind one OpenAI-compatible endpoint (`/v1/responses`, `/v1/chat/completions`, `/v1/models`). It keeps sessions sticky per subscription via the prompt cache key, takes quota-exhausted subscriptions out of rotation until their window resets, and fails fast with `429` + earliest reset time when the whole pool is exhausted. Architecture decisions live in `docs/adr/` - read `docs/adr/0010-unprefixed-routing-flat-pool.md` first for the core model.

## Commands

```sh
cargo build                 # build
cargo test                  # unit tests + Hegel property tests (both must pass)
cargo run -- serve          # run the proxy (default 127.0.0.1:8080)
cargo run -- connect        # configure opencode to use the pool (global config)
cargo run -- connect --dry-run
cargo run -- connect --remove
cargo run -- reset <email-or-id> [--force]   # redeem one banked Codex reset via the running server's admin API
cargo run -- refresh <email-or-id> | --all     # force a token refresh via the running server's admin API
cargo run -- stats [--since 90d] [--by model|account] [--json]   # usage/capacity/refusal stats via the admin API
cargo run -- stats import-codex [--dry-run]  # backfill capacity/policy errors from Codex session logs
```

## Testing policy (enforced)

1. **Unit tests** (`#[test]`, in-module) cover exact behavior: header wiring, URL rewriting, JSONC merge, redaction, retry-after parsing.
2. **Property tests** use [Hegel](https://hegel.dev) (`hegeltest`, pinned) in `tests/properties.rs`. Invariants over the pure pool core: stickiness stability, health state machine, saturation minimums, TTL/cap bounds. New pure-logic modules must ship properties.
3. **No mutation testing.** Considered in ADR 0002 and rejected; do not add cargo-mutants.

## Architecture rules

- **Pure sync core, async at the edges.** `pool::PoolCore` and `health` are synchronous structs with an injected clock (`now_ms` passed in). No tokio, no I/O inside the core - property tests depend on this. Async code lives in axum handlers, reqwest calls, and device-flow pollers.
- **Backends implement the `provider::Backend` trait** (`surfaces`, `rewrite_url`, `inject_headers`, `classify`, `default_cooldown_ms`). `surfaces` is the one required declaration: the pool only ever routes a request to a backend that serves its wire protocol, so an Anthropic-shaped request never lands on an OpenAI-shaped upstream (ADR 0024). Adding a provider = new trait impl + registration; never special-case a backend in the proxy path. Model-catalog eligibility and the flat pool are backend-agnostic.
- **Quota cooling has three dimensions, all applied in one predicate.** Account-wide (`status`/`reset_at`), per billing surface (fork: `Account::scope_cooling`, catalog `scope`), and per model (upstream: `PoolCore::model_cooldowns`). `PoolCore::eligible_at` is the only place that decides eligibility, and `min_reset` advertises the latest applicable deadline per account so a `429` names a time at which an account will really serve (fork ADR).
- **Failover happens before any byte reaches the client.** Streams are held until their first output or terminal event. An overload before output is retried on another account (ADR 0022). Once bytes reach the client, errors pass through.
- **Write-through persistence.** Token rotations, health transitions, new bindings, and catalog edits are persisted to SQLite immediately (ADR 0005).

## Security invariants

- Never log or emit: access/refresh tokens, authorization headers, request bodies, full prompts.
- Account identity in logs and the request ring buffer uses the account **label** (email/username) in full - the operator asked for unredacted labels. Raw UUID account IDs are truncated to 8 chars (`logging.rs` is the only place that formats identity).
- `auth.json` writes use `0600`. Never commit credentials or `pool.db`.
- The proxy API key and admin UI token are minted on first run and stored in the DB; they gate `/v1/*` and `/admin/api/*` respectively.

## Code contracts

Use the `code-contracts` skill for every code change and code review. Follow its contract discovery, writing, and enforcement procedures before submitting commits or pull requests.

- Repo-wide rules live in [`CONTRACTS`](CONTRACTS); declaration contracts use `/// @cc` doc comments on the narrowest relevant function.
- `owner` is the account's GitHub username (`ghuntley`); never add `notify` unless explicitly requested.
- Validate syntax and duplicate IDs with `cc-check format` (npm `@spolu/cc-check`); there is no automated semantic enforcement — verify prose against the implementation for every touched contract.

## ADR policy

- Every non-trivial architectural decision gets a MADR record in `docs/adr/NNNN-title.md` (Status / Context / Decision / Consequences).
- Superseding a decision: mark the old ADR `Superseded by NNNN`, write the new one. Never delete ADRs.
- Fork-local decisions live in [`docs/adr/fork/`](docs/adr/fork/README.md) with title-only filenames and their own index; never allocate an `NNNN` number on the fork. See that README for why.
- Index: 0001 Rust/Axum - 0002 Hegel PBT, no mutation testing - 0003 prompt-cache-key stickiness - 0004 health state machine + fail-fast - 0005 SQLite store - 0006 tracing/correlation IDs - 0007 config-driven catalog - 0008 JSONC-safe merge - 0009 device-code onboarding - 0010 unprefixed routing, flat pool - 0011 GitHub Copilot backend - 0012 cross-proxy request correlation - 0013 automatic Codex banked resets - 0014 per-attempt token accounting - 0015 terminal pool monitor - 0016 local monitor socket - 0017 Codex quota deadline reconciliation - 0018 recoverable credential rejection - 0019 scheduled refresh token rotation - 0020 manual Codex reset redemption - 0021 stream outcome accounting + long-range capacity statistics - 0022 failover on overload before output - 0023 Claude Code subscription backend (Anthropic surface) - 0024 surface-partitioned routing - 0025 free-plan gate - 0026 manual token refresh.

## Review before the pull request (preflight-code)

Every change is reviewed before it becomes a pull request, and again after any change made in response to
that review. The reviewer is a machine-local PR-Agent run over the **working tree** - no GitHub round trip,
and no commit required:

```sh
preflight-code review   # the pending change (HEAD -> working tree) -> .preflight/review.md + .preflight/review.json
preflight-gate          # exit 0 = pass, 1 = key issues, 2 = security concerns, 3 = no review yet
```

`preflight-code` is the review bundle at <https://github.com/413x5/preflight>. It is **not** the
[`preflight`](https://github.com/ghuntley/preflight) secret-inspection proxy that fronts this one (ADR
0012): different repository, different command, and the names no longer collide. Its helpers live in
`~/.local/bin`, are driven by `~/.config/preflight/`, and are not part of this repository.

### Scope

`preflight-code review` reviews **the pending commit**: everything HEAD does not yet contain, tracked and
untracked. Its diff comes from a throwaway snapshot of the working tree, so reviewing never stages,
commits or rewrites anything - the index is untouched and HEAD does not move. Reviewing commit by commit
covers a multi-commit branch completely, because each commit is checked against the tree that was reviewed
for it.

| `PREFLIGHT_SCOPE` | Covers | Enforceable by the hook |
|---|---|---|
| `commit` (default) | HEAD -> working tree, untracked included | yes |
| `worktree` | the whole branch vs the default branch; needs a resolvable base and a fetch | yes |
| `head` | committed content only (base...HEAD) | no |

`PREFLIGHT_BASE` overrides the base for `worktree`.

### Read the review

`preflight-code review` writes `.preflight/review.md`, `.preflight/review.json`, and the sidecar
`.preflight/review.meta.json` that the commit hook reads.

Loop until `preflight-gate` exits 0: fix every entry in `key_issues_to_review[]` and anything under
`security_concerns` in one pass, then re-run `preflight-code review` and `preflight-gate`.

What matters on the summary line is the loop's exit conditions:

- `new=N/M` - N of the M findings were not in the previous review. `new=0` means every finding was already
  raised: judge each one rather than re-fixing blind.
- `coverage=PARTIAL (token budget left files out - see review.md)` - the token budget left files out of the
  review, so a low `issues=` count means "the rest was not looked at", not "the change is clean". Report
  the shortfall to the maintainer; raising `config.max_model_tokens` / `custom_model_max_tokens` is their
  call, not an automatic remedy.
- `model=FALLBACK (the configured model did not answer - see review.md)` - the review came from the fallback
  model, not the configured one. Treat a clean result with more caution than usual.

### Optimising the review process

Every round is a full model call that re-reads the whole diff, so these habits are what keep the loop short:

- **Fix everything material in one pass.** A round that resolves many findings is cheaper than many rounds
  that resolve one each.
- **Answer the class, not the instance.** A finding is one member of a class the code did not exclude, so
  a patch that closes only what the reviewer saw returns next round under a different name. Before editing,
  ask what class this is and what would make every member of it impossible: replace the set with the
  property that makes a set unnecessary (the directory rather than the files in it), and close a finite
  list only by asserting it against its source. Two rounds on one class is the signal; a third means
  change course.
- **A review reports on prose as hard as on code.** A claim the reviewer can check and you have not is a
  finding you bought. Write less commentary, and make sure what you write is true.
- **Do not document behaviour you have not settled.** Write the code, confirm what it does, then describe
  it.
- **A number in prose is a copy nothing checks.** The run, the code and the artifact already own the fact.
  State the shape, or quote the run's output in a code fence. A number that *is* the interface (a timeout,
  an exit code, a port) stays.

### Operating invariants

- **Do not commit to be reviewed.** The loop runs in the working tree; commit once, for the pull request.
- **The review supplements the test/lint gate, it does not replace it.** A clean review says nothing about
  types, lint or the tests.
- **`.preflight/` is gitignored and per worktree.** The exclude lives in `.git/info/exclude`, so review
  output is never committed and two worktrees never collide.

### What the commit hook does

`.githooks/pre-commit` verifies that the content being committed was what `preflight-code review` reviewed,
and that the recorded review passed `preflight-gate`. `.githooks/post-commit` is advisory and cannot block.

- It **never runs a review** and never stages anything; it only reads.
- It is **content-based, not time-based**: a review counts while the content it covered is what is being
  committed.
- The rule is **containment, not equality**: every path being committed must match the reviewed tree, and
  reviewed content that is *not* being committed is ignored - which is what keeps `git commit -a` and
  newly staged files working.
- A deletion, a rename (`git mv`) or a symlink/file-type swap is blocked until a review covers it.
- The **first** commit in a repository with no history is allowed but not enforced.
- `head` scope reviews cannot be enforced; the default `commit` scope and `worktree` can.
- `PREFLIGHT_SKIP=1` (or `git commit --no-verify`) bypasses the check once. A deliberate bypass is
  acceptable; say so in the pull request body rather than leaving it silent.
- The verifier is `$PREFLIGHT_HOOK`, defaulting to `~/.config/preflight/preflight-hook`. If that helper is
  missing the commit is **refused**, not passed.

`core.hooksPath` is local config, not a committed file: a fresh clone enforces nothing until
`git config core.hooksPath .githooks` is run there. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Code review

Facts a reviewer should have, so a deliberate trade-off is not reported as an oversight. Keep this list
current: a stale entry actively misleads.

- The monitor socket is deliberately mode `0666` on a local path, so local users can read the monitor
  snapshot without a token; the admin API still requires the admin token.
- `tests/e2e.rs` drives a mock upstream over loopback and an in-memory SQLite store; no test reads or
  writes real credentials or the operator's `pool.db`.
- Account labels are logged in full **by design** (the operator asked for them); only raw UUID account IDs
  are truncated to 8 characters, and `src/logging.rs` is the only place that formats identity.
- Property-based tests are the chosen strategy over the pure core; mutation testing was considered and
  rejected in ADR 0002, so its absence is deliberate.
- `docs/adr/` records are append-only: a reversal is a new record that supersedes the old one, never an
  edit to it.
## Config files agents may edit

`AGENTS.md`, `README.md`, `CONTRIBUTING.md`, `docs/adr/*`, `Cargo.toml`, `flake.nix`, `.github/*`,
`.githooks/*`, everything under `src/` and `tests/`. Do not touch `devenv.nix`/`devenv.lock` unless the
task explicitly requires it, and never commit `*.bak`, `pool.db`, or `.hegel/` artifacts.
