# Contributing

The rules and traps are in [`AGENTS.md`](AGENTS.md); this file is the human on-ramp: setup, the checks a
change must pass, and how to get a change landed.

## Setup

Rust toolchain plus the [devenv](https://devenv.sh) shell (which supplies the Nix build and the NixOS
machine test):

```sh
devenv shell        # or: nix develop
cargo build
```

The proxy needs credentials for a real run; see the quick start in [`README.md`](README.md). Tests need
none.

## The checks

```sh
cargo build
cargo test          # unit tests, Hegel property tests, and the e2e suite
```

That pair is the floor. CI (`.github/workflows/ci.yml`) runs these inside the devenv shell on every pull
request, on pushes to `main`, and on demand (`workflow_dispatch`), and a change is not done until they pass:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
npx --yes @spolu/cc-check@0.2.0 format   # contract syntax and dangling `@cc` ids
```

The NixOS module evaluation and machine test run in a separate job with the runner's unused toolchains
removed first, because the machine test compiles the crate in release mode inside the Nix sandbox and does
not fit beside the test job's artifacts. If you add a check, add it to the workflow.

## Testing expectations

- Exact behavior gets a plain unit test in the module: header wiring, URL rewriting, JSONC merge, redaction,
  retry-after parsing.
- New pure-logic modules over the pool core ship Hegel property tests in `tests/properties.rs`.
- Behavioural stories go in `tests/e2e.rs`, which drives a mock upstream over loopback and an in-memory
  store.
- Mutation testing is deliberately not used (ADR 0002).

## Review before the pull request

Every change is reviewed in the working tree before it becomes a pull request:

```sh
preflight-code review     # -> .preflight/review.md + .preflight/review.json
preflight-gate            # exit 0 = pass, 1 = key issues, 2 = security concerns, 3 = no review yet
```

Loop until the gate exits 0, fixing every reported issue in one pass. The full loop - what a review covers,
how to read the summary line, and the habits that keep the round count down - is in
[`AGENTS.md`](AGENTS.md#review-before-the-pull-request-preflight-code).

This is the [`preflight-code`](https://github.com/413x5/preflight) bundle, which is separate from the
[`preflight`](https://github.com/ghuntley/preflight) secret-inspection proxy. The helpers are machine-local:
install them with that bundle's `install.sh`. They are not part of this repository and CI does not run them.

### Enable the commit hook (once per clone)

The reviewed content is enforced by a committed hook:

```sh
git config core.hooksPath .githooks     # RELATIVE - an absolute path breaks git worktrees
```

`core.hooksPath` is local config, so a fresh clone enforces nothing until you run that. The hook verifies
that what you are committing was reviewed and passed the gate; it never runs a review itself. Deliberate
bypasses: `PREFLIGHT_SKIP=1 git commit` or `git commit --no-verify`. If you bypass it, say so in the pull
request body.

## Proposing a change

1. Branch from `main`.
2. Make the change, update the docs it makes wrong in the same edit, and record any non-trivial decision as
   an ADR in `docs/adr/` (see the policy in [`AGENTS.md`](AGENTS.md)).
3. Run the review loop until `preflight-gate` exits 0.
4. Run the gate (build and tests) and quote what the run printed in the pull request body.
5. Open the pull request. Say what changed, why, and what you ran.
