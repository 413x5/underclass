#!/usr/bin/env bash
# migrate_db.sh — apply the one-shot data rewrites that a pool.db needs in order to be read by the
# current build. Registered migrations, applied in order, all inside one transaction.
#
# OS-NEUTRAL, unlike its sibling `deploy_macos.sh`: no service manager is involved. It needs
# `sqlite3`, and a writing mode also needs `lsof` to prove no daemon holds the file; on a host
# without `lsof`, stop the service and set `MIGRATE_ASSUME_NO_WRITER=1` to proceed deliberately.
#
# NO MIGRATIONS ARE REGISTERED. The runner ships empty on purpose: it exists so the next data
# rewrite is one entry in `MIGRATIONS` rather than a new script and a new procedure. `--check`
# therefore reports "up to date" against every database, and `deploy_macos.sh` never stops the
# service for it.
#
# WHY THIS IS NOT A BOOT-TIME MIGRATION: `Store::migrate()` runs on every start and adds columns, so
# it is the right place for a schema change. A data rewrite does not belong there - an
# `UPDATE ... WHERE backend='go'` would fire on every boot and would silently reclassify any future
# backend that happens to be named `go`. Data rewrites are this operator tool, run by
# `scripts/deploy_macos.sh` between the rollback-point snapshot and the restart, with the daemon
# stopped. It is also safe to run by hand while the daemon is stopped.
#
# ADDING A MIGRATION takes three edits and nothing else:
#
#   1. append the id to `MIGRATIONS` below, in the order it must run;
#   2. add a case to `describe` saying what it rewrites;
#   3. write `pending_<slug>` (print how many rows still need it, `0` when already applied) and
#      `sql_<slug>` (print the SQL), then add both to their dispatchers.
#
# A migration must be NATURALLY IDEMPOTENT, because there is no applied-ledger. Re-running it on a
# database that already carries the rewrite must make `pending_<slug>` print `0`, so nothing is
# executed. That choice is deliberate: a ledger can lie. Restoring one of `deploy_macos.sh`'s
# rollback snapshots rolls the data back while a ledger inside the restored copy stays ahead, and a
# database copied from another host carries a ledger that says nothing about this file. A pending
# count derived from the data itself cannot disagree with the data. A migration that cannot be
# written idempotently must not be registered here.
#
# STOP THE SERVER FIRST. underclass writes through on token rotation, health transitions, and every
# request, and `sync_from_store` reloads the account table every 10s, so a rewrite racing the daemon
# can be overwritten or half-applied. Every writing mode refuses to touch a database that has a
# live writer; the read-only modes (`--list`, `--check`, `--dry-run`) do not, because
# `deploy_macos.sh` runs `--check` while the daemon is still healthy to decide whether stopping it
# is necessary at all.
#
# A `sqlite3 .backup` copy is taken before any write, using the online-backup API so it is
# transactionally consistent rather than a torn copy of an open file. The rewrite itself runs in a
# single `BEGIN IMMEDIATE` transaction, so a failure anywhere leaves the file as it was.
#
# EXIT CODES: 0 success / nothing to do; 1 `--check` found pending work (its whole contract);
# 2 anything wrong - a missing or unreadable database, a failed integrity check, a live writer, a
# failed rewrite. 1 and 2 must stay distinct: `deploy_macos.sh` only stops the service on 1, and
# treating a broken database as "pending" would take the service down for a rewrite that cannot run.
#
# Usage:
#   migrate_db.sh                    # apply every pending migration to the live database
#   migrate_db.sh --list             # show registered migrations, and which are pending
#   migrate_db.sh --check            # exit 0 when nothing is pending, 1 when something is
#   migrate_db.sh --dry-run          # report what would change; write nothing
#   migrate_db.sh --db PATH          # target a specific database
#   migrate_db.sh --backup-only      # take the safety copy, rewrite nothing
#   migrate_db.sh --restore          # restore the newest backup this script made
set -euo pipefail
# A `pool.db` copy carries the minted proxy key and account data. sqlite3 creates it under the
# process umask and the scripts chmod 600 afterwards, so a permissive umask leaves a brief window
# where the copy is world-readable. Setting the umask once closes it for every copy made here.
umask 077

DATA_DIR="${UNDERCLASS_DATA_DIR:-$HOME/.local/share/underclass}"
DB="$DATA_DIR/pool.db"
BACKUP_DIR="${UNDERCLASS_MIGRATION_BACKUP_DIR:-}"
KEEP_BACKUPS="${KEEP_BACKUPS:-5}"

DRY_RUN=0
CHECK=0
LIST=0
BACKUP_ONLY=0
RESTORE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run)     DRY_RUN=1 ;;
    --check)       CHECK=1 ;;
    --list)        LIST=1 ;;
    --backup-only) BACKUP_ONLY=1 ;;
    --restore)     RESTORE=1 ;;
    --db)
      [ $# -ge 2 ] || { printf 'option --db needs a path\n' >&2; exit 2; }
      DB="$2"; shift ;;
    # Prints the header comment block and nothing else, derived rather than hard-coded so adding a
    # line to the header cannot silently truncate the help.
    -h|--help)     awk 'NR>1 && /^#/ {sub(/^# ?/,""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    -*)            printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
    *)             printf 'unexpected argument: %s\n' "$1" >&2; exit 2 ;;
  esac
  shift
done

# The safety copies live beside the database they protect, derived from the database actually
# named by `--db` rather than from the default data directory. Otherwise `--db <sandbox>/pool.db`
# would write its backup into the live `~/.local/share/underclass/migrations` and then prune there,
# deleting live safety copies - a sandbox run reaching real state, which the sandbox mode exists to
# prevent.
if [ -z "$BACKUP_DIR" ]; then
  BACKUP_DIR="$(dirname "$DB")/migrations"
fi

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
ok()  { printf '  ok: %s\n' "$*"; }
# 2, never 1: 1 is `--check`'s "migrations are pending" and nothing else.
die() { printf '  FAIL: %s\n' "$*" >&2; exit 2; }

# ---------------------------------------------------------------- registry

# Ordered, oldest first. Adding a migration to this list is the whole registration.
#
# Kept as a plain array, and every expansion below uses the `${MIGRATIONS[@]+...}` form: macOS ships
# bash 3.2, where expanding an empty array under `set -u` is an "unbound variable" error.
MIGRATIONS=()

# One line per registered migration: id and what it rewrites.
describe() {
  case "$1" in
    *) die "registered migration has no description: $1" ;;
  esac
}

# How many rows still need this migration. `0` means it has already been applied.
pending() {
  case "$1" in
    *) die "registered migration has no pending check: $1" ;;
  esac
}

# SQL that performs the migration. Printed, then run inside the shared transaction.
sql_for() {
  case "$1" in
    *) die "registered migration has no SQL: $1" ;;
  esac
}

total_pending() {
  local id n total=0
  for id in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    n="$(pending "$id")"
    total=$((total + n))
  done
  printf '%s' "$total"
}

# ---------------------------------------------------------------- migrations

# ---------------------------------------------------------------- read helpers

# `--check` reserves 1 for "pending", so a query that fails must never surface sqlite3's own status
# (also 1) as if it were pending work. Every read goes through here and a failure is reported as 2.
count() {
  local out
  if ! out="$(sqlite3 "file:$DB?mode=ro" "$1")"; then
    die "database query failed: $1"
  fi
  printf '%s' "$out"
}

# A live daemon holds the file open. Reading is fine; rewriting under it is not, because it writes
# through on every request and `sync_from_store` reloads the account table every 10s.
#
# The read-only modes MUST NOT be refused on this basis: `deploy_macos.sh` runs `--check` while the
# daemon is still healthy to decide whether stopping it is necessary at all, so failing here would
# turn "nothing to migrate" into an aborted deploy. The guard is enforced for every writing mode,
# `--restore` included.
assert_no_live_writer() {
  local err rc=0
  # Nothing can hold a file that does not exist, and lsof reports a missing path as its own error -
  # which would refuse `--restore` in exactly the case it exists for (the live file was deleted or
  # replaced). Check existence first.
  [ -e "$DB" ] || return 0
  # A guard that cannot be evaluated is not a guard. Without `lsof` there is no way to tell whether
  # a daemon is writing, so a writing mode refuses instead of rewriting a live file. The escape
  # hatch exists for a host that genuinely has no lsof and an operator who has confirmed the
  # service is stopped.
  if ! command -v lsof >/dev/null 2>&1; then
    if [ "${MIGRATE_ASSUME_NO_WRITER:-0}" = "1" ]; then
      printf '  note: lsof is unavailable; proceeding on MIGRATE_ASSUME_NO_WRITER=1\n' >&2
      return 0
    fi
    die "cannot check for a live writer: lsof is not installed.
        Stop the service and confirm nothing holds $DB, then rerun with MIGRATE_ASSUME_NO_WRITER=1"
  fi
  # Exit 1 is lsof's "no matches" - the only answer that means "not held". Anything else is the
  # guard failing to run, which must not be read as a pass: that is the case that would let a
  # rewrite run against a database the daemon still holds.
  # `|| rc=$?` is required: the assignment inherits lsof's status, and under `set -e` a non-zero one
  # would abort the script before the code could be inspected.
  err="$(lsof -nP -- "$DB" 2>&1 >/dev/null)" || rc=$?
  case "$rc" in
    0) die "a process has $DB open; stop the underclass service first" ;;
    1)
      # lsof exits 1 for "no matches" AND for its own errors, so the code alone is not the answer.
      # A genuine no-match is silent; anything on stderr means the guard did not run, and treating
      # that as "not held" is how a rewrite would land on a live database.
      if [ -z "$err" ]; then
        return 0
      fi
      if [ "${MIGRATE_ASSUME_NO_WRITER:-0}" = "1" ]; then
        printf '  note: lsof failed; proceeding on MIGRATE_ASSUME_NO_WRITER=1\n' >&2
        return 0
      fi
      die "could not check for a live writer (lsof failed):
        $err
        Stop the service, confirm nothing holds $DB, then rerun with MIGRATE_ASSUME_NO_WRITER=1" ;;
    *)
      if [ "${MIGRATE_ASSUME_NO_WRITER:-0}" = "1" ]; then
        printf '  note: lsof failed; proceeding on MIGRATE_ASSUME_NO_WRITER=1\n' >&2
        return 0
      fi
      die "could not check for a live writer (lsof failed); stop the service, then rerun with MIGRATE_ASSUME_NO_WRITER=1" ;;
  esac
}

# ---------------------------------------------------------------- --restore
# A writing mode, so it is subject to the guard above: replacing the file under a running daemon
# leaves that daemon writing to an unlinked inode and loses everything it writes afterwards. The
# sidecars belong to the file being replaced - left beside a restored database they replay a WAL
# that does not belong to it, which corrupts the restore.
if [ "$RESTORE" = "1" ]; then
  # Combined with a read-only mode, `--restore` must not write: `--dry-run` is documented as
  # "changes nothing", and `--list`/`--check` are read-only probes. Refusing is clearer than
  # silently copying over the live database for a caller who asked to preview.
  if [ "$LIST" = "1" ] || [ "$CHECK" = "1" ] || [ "$BACKUP_ONLY" = "1" ]; then
    die "--restore cannot be combined with --list, --check or --backup-only; run them separately"
  fi
  say "Restoring the newest migration backup"
  [ -d "$BACKUP_DIR" ] || die "no backups in $BACKUP_DIR"
  # A run interrupted between `mkdir` and a completed copy leaves an empty newest directory, so the
  # newest entry is not always usable. Walk past the unusable ones rather than failing on the first:
  # an undo that refuses to run because of a partial backup is an undo that does not exist.
  newest=""
  for candidate in $(ls -1 "$BACKUP_DIR" 2>/dev/null | sort -r); do
    candidate_db="$BACKUP_DIR/$candidate/pool.db"
    [ -s "$candidate_db" ] || continue
    # `set -e` would abort on a non-zero sqlite3 exit (a truncated copy exits 1) before the `continue`
    # below could skip it, and it would also exit 1 - the code reserved for "pending". A failure here
    # means "this entry is unusable, try the next".
    if ! integrity="$(sqlite3 "file:$candidate_db?mode=ro" 'PRAGMA integrity_check;' 2>&1)"; then
      continue
    fi
    [ "$integrity" = "ok" ] || continue
    newest="$candidate"
    break
  done
  [ -n "$newest" ] || die "no usable backup in $BACKUP_DIR (each entry is missing, empty, or fails integrity_check)"
  src="$BACKUP_DIR/$newest/pool.db"
  # The dry run writes nothing, so it is a read-only mode and is not refused on this basis - the
  # guard belongs to the write that follows.
  if [ "$DRY_RUN" = "1" ]; then
    say "Dry run: would restore $DB from $newest and leave the file untouched"
    exit 0
  fi
  assert_no_live_writer
  if [ "$DRY_RUN" = "0" ]; then
    # Staged, verified, then moved into place: `cp` truncates the destination on open, so an
    # interrupted copy would leave the live database unreadable with no message. The old file is
    # only replaced once a complete, integrity-checked copy exists.
    # Staged BESIDE the database so the final move is a rename on the same filesystem. A staging
    # directory in $TMPDIR (tmpfs on many Linux hosts) would make `mv` copy-then-unlink, which writes
    # the live file in place and brings back exactly the torn-copy risk this staging exists to avoid.
    STAGE="$(mktemp -d "$(dirname "$DB")/.underclass-restore.XXXXXX")"
    trap 'rm -rf "$STAGE"' EXIT
    if ! cp "$src" "$STAGE/pool.db"; then
      die "could not stage a copy of $src; $DB was not touched"
    fi
    chmod 600 "$STAGE/pool.db"
    staged_integrity="$(sqlite3 "file:$STAGE/pool.db?mode=ro" 'PRAGMA integrity_check;' 2>&1)" \
      || die "sqlite3 could not read the staged copy; $DB was not touched: $staged_integrity"
    [ "$staged_integrity" = "ok" ] || die "staged restore failed integrity_check; $DB was not touched"
    staged_accounts="$(sqlite3 "file:$STAGE/pool.db?mode=ro" 'SELECT count(*) FROM accounts;' 2>&1)" \
      || die "could not count accounts in the staged copy; $DB was not touched"
    expect_accounts="$(sqlite3 "file:$src?mode=ro" 'SELECT count(*) FROM accounts;' 2>&1)" \
      || die "could not count accounts in $src; $DB was not touched"
    [ "$staged_accounts" = "$expect_accounts" ] || die "staged copy has $staged_accounts accounts, expected $expect_accounts; $DB was not touched"
    # Every sidecar of the replaced file: a hot rollback journal left by an unclean stop would be
    # replayed into the restored database and corrupt it.
    # Restoring the wrong point must itself be undoable, so the file being replaced is copied into the
    # backup directory first - the same safety copy every other writing mode takes. Without it a
    # mistaken `--restore` silently discards everything written since the chosen snapshot.
    if [ -f "$DB" ]; then
      # Nested, deliberately: `--restore` scans "$BACKUP_DIR"/*/pool.db for the newest usable backup,
      # and a pre-restore copy sitting directly there would be selected as that backup - making the
      # next restore a no-op that reverts nothing, or worse, reinstating a corrupt file.
      # Same-second collision suffix as the backup path: two restores in one second must not share a
      # directory, or the second overwrites the undo copy the first just made.
      PRE_RESTORE="$BACKUP_DIR/pre-restore/$(date -u +%Y%m%dT%H%M%SZ)"
      pre_suffix=0
      while [ -e "$PRE_RESTORE" ]; do
        pre_suffix=$((pre_suffix + 1))
        PRE_RESTORE="$BACKUP_DIR/pre-restore/$(date -u +%Y%m%dT%H%M%SZ)-$pre_suffix"
      done
      mkdir -p "$PRE_RESTORE" || die "could not create $PRE_RESTORE; $DB was not touched"
      # `.backup` when the file is readable, because it captures committed WAL frames and yields a
      # consistent database rather than loose files. The byte copy is the fallback for exactly the
      # case that usually motivates a restore: a missing, truncated or corrupt file that sqlite3
      # cannot read at all - where something is better than nothing, and the unreadable original is
      # what the operator wants preserved.
      if ! sqlite3 "file:$DB?mode=ro" ".backup '$PRE_RESTORE/pool.db'" >/dev/null 2>&1; then
        if ! cp "$DB" "$PRE_RESTORE/pool.db" 2>/dev/null; then
          die "could not copy the database being replaced into $PRE_RESTORE; $DB was not touched"
        fi
        # Sidecars carry committed frames after an unclean stop, so the byte copy takes them too.
        for sidecar in "$DB-wal" "$DB-shm" "$DB-journal"; do
          [ -f "$sidecar" ] && cp "$sidecar" "$PRE_RESTORE/$(basename "$sidecar")" 2>/dev/null || true
        done
        printf '  note: the database was not readable; saved a byte copy instead\n' >&2
      fi
      chmod 600 "$PRE_RESTORE"/* 2>/dev/null || true
      ok "previous database saved to $PRE_RESTORE"
    fi
    rm -f "$DB-wal" "$DB-shm" "$DB-journal"
    mv -f "$STAGE/pool.db" "$DB"
    ok "restored $DB from $newest"

  # `--restore` exits here, so the pruning in the backup/rewrite path never runs for it: the undo
  # copies it just made must be bounded here, or repeated restores accumulate full-size databases.
  if [ -d "$BACKUP_DIR/pre-restore" ]; then
    STALE_UNDO="$(ls -1 "$BACKUP_DIR/pre-restore" 2>/dev/null | sort -r | tail -n +$((KEEP_BACKUPS + 1)))"
    if [ -n "$STALE_UNDO" ]; then
      printf '%s\n' "$STALE_UNDO" | while IFS= read -r old; do
        if [ -n "$old" ]; then rm -rf "${BACKUP_DIR:?}/pre-restore/$old"; fi
      done
      ok "pruned old pre-restore copies (keeping $KEEP_BACKUPS)"
    fi
  fi
  fi
  printf '  start the service again once you have restored the database it should serve\n' 
  exit 0
fi

# ---------------------------------------------------------------- preflight
# Deliberately AFTER `--restore`: the reason a backup is needed is usually that the live database is
# missing or unreadable, so requiring it to be present and valid would refuse to undo the very case
# the undo exists for. The restore validates the BACKUP instead.
[ -f "$DB" ] || die "database not found: $DB"
sqlite3 "file:$DB?mode=ro" 'PRAGMA integrity_check;' 2>&1 | grep -qx ok \
  || die "database failed integrity_check: $DB"

# `accounts` exists in every build this script could meet. Without it the file is not a pool.db.
# Probed with sqlite3 directly rather than through `count`, which reports every failure as a generic
# query error and would hide the reason that matters here.
if ! sqlite3 "file:$DB?mode=ro" "SELECT count(*) FROM accounts;" >/dev/null 2>&1; then
  die "not a pool.db (no accounts table): $DB"
fi

# ---------------------------------------------------------------- --list

if [ "$LIST" = "1" ]; then
  say "Registered migrations in $DB"
  if [ "${#MIGRATIONS[@]}" = "0" ]; then
    ok "none registered yet; this runner exists for the next data rewrite"
    exit 0
  fi
  for id in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    n="$(pending "$id")"
    if [ "$n" = "0" ]; then
      printf '  %-20s applied   %s\n' "$id" "$(describe "$id")"
    else
      printf '  %-20s PENDING   %s (%s row(s))\n' "$id" "$(describe "$id")" "$n"
    fi
  done
  exit 0
fi

# ---------------------------------------------------------------- --check

if [ "$CHECK" = "1" ]; then
  NEEDS="$(total_pending)"
  if [ "$NEEDS" = "0" ]; then
    printf 'up to date: %s has no pending migrations\n' "$DB"
    exit 0
  fi
  printf 'needs migration: %s has %s row(s) across %s registered migration(s)\n' \
    "$DB" "$NEEDS" "${#MIGRATIONS[@]}"
  for id in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    n="$(pending "$id")"
    [ "$n" = "0" ] || printf '  pending: %s (%s row(s))\n' "$id" "$n"
  done
  exit 1
fi

# Everything past this point writes (a backup, then the rewrite), so the daemon must be gone.
# `--backup-only` is deliberately NOT short-circuited by an empty migration list: taking a copy
# before a manual change is most useful when nothing is pending, and `--restore` needs one to exist.
NEEDS="$(total_pending)"
if [ "$NEEDS" = "0" ] && [ "$BACKUP_ONLY" != "1" ]; then
  say "Nothing to do"
  ok "$DB has no pending migrations"
  exit 0
fi

if [ "$DRY_RUN" = "1" ]; then
  say "Dry run: would rewrite $NEEDS row(s) and leave $DB otherwise untouched"
  for id in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    n="$(pending "$id")"
    [ "$n" = "0" ] || printf '  would apply %s (%s row(s))\n' "$id" "$n"
  done
  exit 0
fi

assert_no_live_writer

# ---------------------------------------------------------------- backup

# Timestamped to the second, with a numeric suffix so two runs in the same second cannot collide.
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DEST="$BACKUP_DIR/$STAMP"
suffix=0
while [ -e "$DEST" ]; do
  suffix=$((suffix + 1))
  DEST="$BACKUP_DIR/$STAMP-$suffix"
done
mkdir -p "$DEST" || die "could not create the backup directory: $DEST"
say "Backing up to $DEST"
sqlite3 "file:$DB?mode=ro" ".backup '$DEST/pool.db'" >/dev/null \
  || die "sqlite3 .backup failed; no safety copy was written and $DB was not modified"
chmod 600 "$DEST/pool.db" || die "could not restrict permissions on $DEST/pool.db"
BEFORE_ACCOUNTS="$(count 'SELECT count(*) FROM accounts;')"
verify_integrity="$(sqlite3 "file:$DEST/pool.db?mode=ro" 'PRAGMA integrity_check;' 2>&1)" \
  || die "sqlite3 could not read the safety copy: $verify_integrity"
[ "$verify_integrity" = "ok" ] || die "backup failed integrity_check: $verify_integrity"
GOT="$(sqlite3 "file:$DEST/pool.db?mode=ro" 'SELECT count(*) FROM accounts;')" \
  || die "could not count accounts in the safety copy"
[ "$GOT" = "$BEFORE_ACCOUNTS" ] || die "backup has $GOT accounts, expected $BEFORE_ACCOUNTS"
printf '%s\n' "$NEEDS" > "$DEST/ROWS_REWRITTEN"
printf '%s\n' "$BEFORE_ACCOUNTS" > "$DEST/ACCOUNTS_BEFORE"
# Only when there is something to list: `printf` with no arguments still applies the format once,
# which would leave a stray blank line in the record.
if [ "${#MIGRATIONS[@]}" != "0" ]; then
  printf '%s\n' ${MIGRATIONS[@]+"${MIGRATIONS[@]}"} > "$DEST/MIGRATIONS"
fi
ok "consistent copy ($(du -h "$DEST/pool.db" | cut -f1), $BEFORE_ACCOUNTS accounts)"

# Only real backups count toward KEEP_BACKUPS: `pre-restore/` holds the undo copies taken by
# `--restore`, is pruned separately, and would otherwise consume a slot forever (and, sorting after
# digits, never be enumerated by this list).
STALE="$(ls -1 "$BACKUP_DIR" 2>/dev/null | grep -vx 'pre-restore' | sort -r | tail -n +$((KEEP_BACKUPS + 1)))"
if [ -n "$STALE" ]; then
  printf '%s\n' "$STALE" | while IFS= read -r old; do
    if [ -n "$old" ]; then rm -rf "${BACKUP_DIR:?}/$old"; fi
  done
  ok "pruned old backups (keeping $KEEP_BACKUPS)"
fi

# The undo copies are full-size too, so they get the same bounded window.
if [ -d "$BACKUP_DIR/pre-restore" ]; then
  STALE_UNDO="$(ls -1 "$BACKUP_DIR/pre-restore" 2>/dev/null | sort -r | tail -n +$((KEEP_BACKUPS + 1)))"
  if [ -n "$STALE_UNDO" ]; then
    printf '%s\n' "$STALE_UNDO" | while IFS= read -r old; do
      if [ -n "$old" ]; then rm -rf "${BACKUP_DIR:?}/pre-restore/$old"; fi
    done
    ok "pruned old pre-restore copies (keeping $KEEP_BACKUPS)"
  fi
fi

if [ "$BACKUP_ONLY" = "1" ]; then
  say "Backup only: $DB was not modified ($NEEDS pending row(s))"
  exit 0
fi

# ---------------------------------------------------------------- rewrite

say "Rewriting $NEEDS row(s)"

# Every registered migration's SQL is emitted unconditionally, inside one transaction passed through
# as a whole. Deciding per migration with `pending "<id>"` would read each count on a separate
# read-only connection, which cannot see the uncommitted work of the transaction being built: a
# later migration whose need only arises from an earlier one's rewrite would report 0 and be
# skipped, and the run would then abort on the post-commit check with the database already changed.
# Each migration's SQL must therefore be safe to run when its rewrite is already present - which the
# idempotency rule above requires anyway - and `pending` stays the *reporting* mechanism.
{
  printf '.bail on\n.timeout 5000\nBEGIN IMMEDIATE;\n'
  for id in ${MIGRATIONS[@]+"${MIGRATIONS[@]}"}; do
    sql_for "$id"
  done
  printf 'COMMIT;\n'
} | sqlite3 "$DB" || die "the database rewrite failed.
        The transaction either rolled back and left $DB unchanged, or committed before the failure was
        observed; this script cannot tell which from here. Verify before relying on either state
        (a pre-rewrite snapshot is in $BACKUP_DIR), then rerun --check."

ok "rewrite committed"

# ---------------------------------------------------------------- verify

say "Verifying"
AFTER_ACCOUNTS="$(count 'SELECT count(*) FROM accounts;')"
[ "$AFTER_ACCOUNTS" = "$BEFORE_ACCOUNTS" ] \
  || die "account count changed ($BEFORE_ACCOUNTS -> $AFTER_ACCOUNTS)"

RESIDUAL="$(total_pending)"
[ "$RESIDUAL" = "0" ] || die "$RESIDUAL row(s) still pending; restore with: $0 --restore"

integrity="$(count 'PRAGMA integrity_check;')"
[ "$integrity" = "ok" ] || die "integrity_check after rewrite: $integrity"

ok "no registered migration has work left"

printf '\n\033[1mMigrated %s\033[0m\n' "$DB"
printf '  backup:  %s\n' "$DEST/pool.db"
printf '  undo:    %s --restore\n' "$0"
