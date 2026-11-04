# Deployment Tooling Lives in the Repository

## Status

Accepted

## Context

The operator's macOS install and update tooling lived outside the repository, in
`~/Dev/scripts` (`underclass-update.sh` and `underclass-migrate-opencode-go.sh`). That put the
procedure that decides what binary runs the proxy outside version control, where it could not be
reviewed with the code it installs, could drift from the binary's persisted identifiers, and did
not reach anyone who forked the repository.

Two facts shaped the move:

- The scripts drive `launchctl`, write a launch agent, and probe a locally bound port. That is
  macOS-shaped, and underclass's Linux story is already the flake and the NixOS module. A generic
  "deploy" name would have implied Linux coverage the script does not have.
- The database has data rewrites that are not schema migrations. `Store::migrate()` runs on every
  start and adds columns, so a rewrite like "`backend = 'go'` becomes `opencode-go`" cannot live
  there: it would fire forever and would reclassify any future backend that happened to be named
  `go`.

## Decision

Move the tooling into `scripts/` as two files, and name them for their real scope.

**`scripts/deploy_macos.sh`** handles a fresh install and an update, detected rather than demanded
from the operator. A missing binary or a missing launch agent plist is a fresh install: build,
install, write the agent if there is none, load it, and prove health. A database is not part of
that test, because a first install has none and `underclass serve` mints one. Everything else is an
update: record a rollback point, build before touching the service, migrate only when pending,
install, restart, verify.

The install is staged: copy to `$BIN_DST.new`, `chmod 755`, then `mv -f`. Writing over a binary that
a launchd job is running leaves the old process alive on the old image while the path on disk is
new, which is a success report for code that is not running. For the same reason a restart is not
considered successful because `launchctl kickstart` exited 0: the job's pid and process start time
are captured before, and a different identity plus a `200` from `/v1/models` is the evidence. An
unchanged identity fails with the exact repair command.

Every run also installs `config.example.toml` beside the live configuration at `0644`, so the
reference can be diffed against the operator's own file after an update. `config.toml` is created
only when it is absent or empty and is never overwritten; an empty file means nothing was ever
configured, so there is no intent to preserve.

**`scripts/migrate_db.sh`** is the companion runner and is OS-neutral, because it is only
`sqlite3`. It holds an ordered registry of named migrations; adding one is an entry in `MIGRATIONS`, a
`describe` case, and a `pending_<slug>` / `sql_<slug>` pair. No migration is registered at present: the runner is the
extension point, and the first data rewrite it carries will be the one that needs it. Everything
around that empty registry is settled - an online-backup safety copy first, refusal when a live
writer holds the file, one `BEGIN IMMEDIATE` transaction for the whole rewrite, and a per-migration
`pending_<slug>` count that must reach `0` before the rewrite runs.

There is no applied-migrations ledger. Each migration derives its pending count from the data
itself, so it cannot disagree with the data. A ledger can: restoring one of `deploy_macos.sh`'s
rollback snapshots rolls the data back while the ledger inside the restored copy stays ahead, and a
database copied from another host carries a ledger that says nothing about this file. The cost of
this choice is that every registered migration must be naturally idempotent; one that cannot be
must not be registered.

The coupling between the two files is direct: `deploy_macos.sh` calls its sibling `migrate_db.sh`
from its own directory, checked with `--check` before the restart. A missing or non-executable
runner is a hard error whenever a database exists, including under `--no-restart`. Treating it as a
skippable note is what would ship a data rewrite to a database that never received it; the cost of
that is a pool reading identifiers the new binary no longer recognises.

The rollback model is unchanged from the script this replaces: a release directory per update
holding the previous binary, an online-backup `pool.db` snapshot and a manifest, pruned to
`KEEP_RELEASES`; `--rollback` restores the newest restorable one or the one recorded for a
revision, and `--keep-db` restores only the binary. Restoring the binary alone is safe because the
schema only grows and every query names its columns, so an older binary reads a newer database.

## Consequences

- The deployment procedure is reviewed with the code it installs, and a change to a persisted
  identifier can ship its migration in the same commit.
- The fresh-install path is exercised by the sandbox mode rather than being reconstructed by hand,
  so a first install and an update are both covered by one verification story.
- `scripts/deploy_macos.sh` states in its header that it is macOS-only, and the README keeps the
  Nix path as the supported Linux install. A Linux equivalent is not guessed at; it would be a
  separate script with its own service manager, and it does not exist today.
- The migration runner's extensibility is the reason it is not named after a migration: it starts
  with none registered, and the next data rewrite is a registry entry, not a new file.
- Removing the migration runner from the deployment directory is now loud instead of silent. The
  cost is that a stripped-down copy fails rather than degrades, which is the intended trade.
