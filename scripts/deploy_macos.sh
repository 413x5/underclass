#!/usr/bin/env bash
# deploy_macos.sh — install underclass from source, or update the copy that is already installed,
# in one run. Every change it makes to an installed copy keeps a rollback point behind it.
#
# SUPPORTED OS: macOS, and only macOS. This script drives `launchctl` and writes a launch agent in
# ~/Library/LaunchAgents, so there is nothing here that runs on Linux - a Linux host installs
# through the flake or the NixOS module instead (see README.md, "Nix flake"). The migration it
# calls, `migrate_db.sh`, is OS-neutral: it needs sqlite3, plus lsof for its live-writer guard.
#
# FRESH INSTALL vs UPDATE is detected, not demanded. A missing binary, a missing launch agent
# plist, or a label that is not loaded means fresh install: build, install, write the launch agent
# if there is none, load it, and probe until healthy. A database is NOT required for this. Every
# other shape is an update and takes the full path described below. Both shapes build before the
# service is touched.
#
# SOURCE OF TRUTH: ~/dev/underclass (same directory as ~/Dev/underclass on a case-insensitive
# filesystem). Other worktrees are working copies; this one is the one that gets built.
#
# ORDERING: the build happens BEFORE the service is touched, so a compile error or a network
# failure leaves the running proxy serving its previous binary. The daemon is restarted only
# after a successful build, and only after the rollback point is on disk.
#
# ROLLBACK POINT: before installing, the script writes a release into
# ~/.local/share/underclass/releases/<timestamp>-<rev>/ holding the previous binary, a consistent
# snapshot of pool.db, and a manifest. `--rollback` restores the newest one, or the one recorded
# for a given revision. The snapshot uses SQLite's online-backup API while the daemon is still
# running, so it is transactionally consistent rather than a torn copy of an open file.
#
# REVERT SEMANTICS: the schema only ever grows (migrate() runs `ALTER TABLE ADD COLUMN` behind a
# `PRAGMA table_info` check) and every query names its columns, so an older binary runs fine
# against a newer database. Restoring only the binary is therefore safe, and that is what
# `--keep-db` does. Restoring the database as well is the destructive option: usage recorded
# since the update is discarded with it, which is the right choice when a migration itself is
# suspect.
#
# Usage (from anywhere; the script resolves its own directory):
#   scripts/deploy_macos.sh                        # install if absent, else update; restart either way
#   scripts/deploy_macos.sh --dry-run              # fetch and report; changes nothing
#   scripts/deploy_macos.sh --no-restart           # update the binary, leave the service running
#   scripts/deploy_macos.sh --list                 # list rollback points
#   scripts/deploy_macos.sh --rollback             # restore the newest rollback point, then restart
#   scripts/deploy_macos.sh --rollback REV         # restore the point recorded for that revision
#   scripts/deploy_macos.sh --rollback --keep-db   # restore the binary only, keep the live database
#   scripts/deploy_macos.sh --sandbox              # exercise the whole flow against throwaway state
#   scripts/deploy_macos.sh --sandbox --sandbox-root DIR
#                                                  # same, in a directory that survives the run
#
# RESTART PROOF: a successful `launchctl kickstart` is not evidence that a new binary is running -
# the process can keep its old image after the file is replaced. Every restart therefore captures
# the job's pid and process start time first, then requires a different identity AND a 200 from
# /v1/models before it reports success. An unchanged identity fails loudly with the repair command.
#
# INSTALL ORDER: the binary is staged as `$BIN_DST.new`, chmodded, and moved into place with
# `mv -f`. It is never written in place with `install` or a direct copy, which is what leaves a
# running process on the old image while the file on disk is new.
#
# CONFIG: every run copies the checkout's `config.example.toml` beside the live config, at 0644, so
# the operator can diff their configuration against the reference after an update. `config.toml`
# itself is created only when it is missing or empty, and is never overwritten.
#
# --sandbox is the way to test a change to this script: it builds from a detached worktree,
# installs into a sandbox bin dir, uses a sandbox data dir copied from the live database, and
# targets the `com.underclass.serve.sandbox` label on port 18080. The live daemon, its database,
# its binary and its config are never touched.
#
# The optional database migration is a sibling script, applied from this file's own directory so a
# worktree checkout or a copy elsewhere still finds it. A missing sibling is a hard error when the
# database needs it, because silently skipping it would install a binary that cannot read the rows.
#
# Run from a Terminal window, not from inside a session served by the proxy: the restart
# interrupts in-flight requests, including the agent's own connection.
set -euo pipefail
# Rollback points and migration backups copy `pool.db`, which carries the minted proxy key and
# account data. sqlite3 creates the copy under the process umask, so a permissive one leaves it
# briefly world-readable before the chmod. One umask here closes that window for every copy.
umask 077

# The script's own absolute path, resolved once before anything changes the working directory. The
# update path `cd`s into the source checkout, so a relative `$0` (./scripts/deploy_macos.sh) would
# make every printed hint command - and the sibling lookup below - wrong from that point on.
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

SRC="${SRC:-$HOME/dev/underclass}"
BRANCH="${BRANCH:-main}"
REMOTE="${REMOTE:-origin}"          # our fork; `upstream` is ghuntley's, fetched separately
BIN_DST_EXPLICIT="${BIN_DST:-}"
BIN_DST="${BIN_DST:-$HOME/.local/bin/underclass}"
LABEL="${LABEL:-com.underclass.serve}"
PLIST="${PLIST:-$HOME/Library/LaunchAgents/$LABEL.plist}"
PLIST_WORKDIR="${PLIST_WORKDIR:-$HOME}"
BIND_EXPLICIT="${BIND:-}"
BIND="${BIND:-127.0.0.1:8080}"
PORT="${BIND##*:}"
HEALTH_URL="http://$BIND/v1/models"
DATA_DIR_EXPLICIT="${UNDERCLASS_DATA_DIR:-}"
DATA_DIR="${UNDERCLASS_DATA_DIR:-$HOME/.local/share/underclass}"
DB="$DATA_DIR/pool.db"
RELEASES="$DATA_DIR/releases"
KEEP_RELEASES="${KEEP_RELEASES:-10}"
CONFIG_DIR_EXPLICIT="${UNDERCLASS_CONFIG_DIR:-}"
CONFIG_DIR="${UNDERCLASS_CONFIG_DIR:-$HOME/.config/underclass}"
# Remembered before the sandbox block reassigns DATA_DIR (and repoints PLIST), so the sandbox can
# seed itself from the database this install actually uses. A data dir pinned into the live launch
# agent is read from the LIVE plist here, because by the time the override block runs the sandbox has
# already replaced PLIST with its own.
LIVE_DATA_DIR="$DATA_DIR"
LIVE_PLIST="$PLIST"
if [ -z "$DATA_DIR_EXPLICIT" ] && [ -f "$LIVE_PLIST" ] && command -v plutil >/dev/null 2>&1; then
  LIVE_PLIST_DATA="$(plutil -extract EnvironmentVariables.UNDERCLASS_DATA_DIR raw -o - "$LIVE_PLIST" 2>/dev/null || true)"
  [ -n "$LIVE_PLIST_DATA" ] && LIVE_DATA_DIR="$LIVE_PLIST_DATA"
fi
# The optional database migration is a sibling of this script, resolved from its own directory so
# it works from any working directory and from a worktree checkout (where `.git` is a file).
# MIGRATE overrides it for a copy that lives elsewhere.
MIGRATE="${MIGRATE:-$(dirname "$SELF")/migrate_db.sh}"
PROXY_KEY="${UNDERCLASS_PROXY_KEY:-}"

DRY_RUN=0
RESTART=1
MODE="update"
SANDBOX=0
SANDBOX_ROOT=""
TARGET_REV=""
KEEP_DB=0

while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run)    DRY_RUN=1 ;;
    --no-restart) RESTART=0 ;;
    --rollback)   MODE="rollback" ;;
    --keep-db)    KEEP_DB=1 ;;
    --list)       MODE="list" ;;
    --sandbox)    SANDBOX=1 ;;
    --sandbox-root)
      [ $# -ge 2 ] || { printf 'option --sandbox-root needs a directory\n' >&2; exit 2; }
      SANDBOX_ROOT="$2"; SANDBOX=1; shift ;;
    # Derived rather than a hard-coded line range: the header is the reference for these flags, and
    # a range silently truncates the help as soon as a line is added.
    -h|--help)    awk 'NR>1 && /^#/ {sub(/^# ?/,""); print; next} NR>1 {exit}' "$0"; exit 0 ;;
    -*)           printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
    *)            TARGET_REV="$1" ;;
  esac
  shift
done

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
ok()  { printf '  ok: %s\n' "$*"; }
die() { printf '  FAIL: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- launch agent

# One launch agent template for both the real install and the sandbox. Environment variables are
# written only when this run's values differ from the built-in defaults, because `underclass serve`
# reads none of them from a plist's command line (src/config.rs) and pinning the defaults would
# freeze them if they ever change.
write_plist() {
  mkdir -p "$(dirname "$PLIST")"
  {
    cat <<PLIST_HEAD
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DST</string>
        <string>serve</string>
    </array>
PLIST_HEAD
    # Each key is emitted only when its own value differs from the built-in default, so setting one
    # does not freeze the other two at today's defaults in an agent that updates never rewrite.
    PLIST_ENV_BODY=""
    [ "$BIND" = "127.0.0.1:8080" ] || PLIST_ENV_BODY="$PLIST_ENV_BODY
        <key>UNDERCLASS_BIND</key>
        <string>$BIND</string>"
    [ "$DATA_DIR" = "$HOME/.local/share/underclass" ] \
      || PLIST_ENV_BODY="$PLIST_ENV_BODY
        <key>UNDERCLASS_DATA_DIR</key>
        <string>$DATA_DIR</string>"
    [ "$CONFIG_DIR" = "$HOME/.config/underclass" ] \
      || PLIST_ENV_BODY="$PLIST_ENV_BODY
        <key>UNDERCLASS_CONFIG_DIR</key>
        <string>$CONFIG_DIR</string>"
    if [ -n "$PLIST_ENV_BODY" ]; then
      printf '    <key>EnvironmentVariables</key>\n    <dict>%s\n    </dict>\n' "$PLIST_ENV_BODY"
    fi
    cat <<PLIST_TAIL
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>WorkingDirectory</key>
    <string>$PLIST_WORKDIR</string>
    <key>StandardOutPath</key>
    <string>$DATA_DIR/underclass.log</string>
    <key>StandardErrorPath</key>
    <string>$DATA_DIR/underclass.stderr.log</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
PLIST_TAIL
  } > "$PLIST"
  plutil -lint "$PLIST" >/dev/null 2>&1 || die "generated launch agent is invalid: $PLIST"
}

job_loaded() { launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; }

# The bind address the INSTALLED agent actually serves. An update never rewrites the plist, so the
# agent keeps whatever UNDERCLASS_BIND it was created with while this script's BIND may have fallen
# back to the default - probing 8080 against a service on 9090 would report a healthy install as
# broken. The plist is the source of truth once it exists.
# One key out of the installed plist's EnvironmentVariables, or empty. Always returns 0: a plist
# that does not set the key is normal (`write_plist` only emits the block when a value is
# non-default), and an assignment from a failing substitution would abort under `set -e`.
# The program path recorded in the installed agent. Updates never rewrite the plist, so if this
# disagrees with $BIN_DST the job keeps running the old binary while the proof still sees a new pid
# and a 200 - the stale-image success this script exists to prevent.
plist_program() {
  local value=""
  [ -f "$PLIST" ] || return 0
  command -v plutil >/dev/null 2>&1 || return 0
  value="$(plutil -extract ProgramArguments.0 raw -o - "$PLIST" 2>/dev/null || true)"
  [ -n "$value" ] && printf '%s' "$value"
  return 0
}

plist_env() {
  local key="$1" value=""
  [ -f "$PLIST" ] || return 0
  if command -v plutil >/dev/null 2>&1; then
    value="$(plutil -extract "EnvironmentVariables.$key" raw -o - "$PLIST" 2>/dev/null || true)"
  fi
  [ -n "$value" ] && printf '%s' "$value"
  return 0
}

# "Fresh" is anything that is not a running service with an installed binary: a missing binary, a
# missing launch agent, or a plist whose label is not loaded. The last case matters because this
# script's own repair advice is `launchctl bootout`, so after following it the binary and plist both
# exist while nothing is running - the update path would then fail with "is not loaded" and demand a
# rollback for a service that only needs bootstrapping. A database is deliberately not part of the
# test: a first install has none, and `underclass serve` mints one on first start.
detect_fresh_install() {
  [ -x "$BIN_DST" ] || return 0
  [ -f "$PLIST" ] || return 0
  job_loaded || return 0
  return 1
}

# A sandbox is a throwaway copy of the mutable state: its own binary path, data dir, launch agent
# label and port, and a *copy* of the source checkout. Nothing it does can reach the real service,
# so it is the supported way to exercise install/rollback end to end.
if [ "$SANDBOX" = "1" ]; then
  SANDBOX_AUTOCREATED=0
  if [ -z "$SANDBOX_ROOT" ]; then
    SANDBOX_ROOT="$(mktemp -d -t underclass-sandbox)"
    SANDBOX_AUTOCREATED=1
  fi
  mkdir -p "$SANDBOX_ROOT"
  SANDBOX_ROOT="$(cd "$SANDBOX_ROOT" && pwd)"
  if ! git -C "$SANDBOX_ROOT/src" rev-parse --git-dir >/dev/null 2>&1; then
    # A sandbox root deleted with `rm -rf` leaves its worktree registered in the main checkout,
    # which makes `worktree add` refuse the path. PRUNE_SANDBOXES is the only destructive call,
    # and it only ever touches registrations whose directory is already gone.
    if [ "${PRUNE_SANDBOXES:-0}" = "1" ]; then
      git -C "$SRC" worktree prune >/dev/null 2>&1 || true
    fi
    git -C "$SRC" worktree add --force --detach "$SANDBOX_ROOT/src" HEAD >/dev/null 2>&1 \
      || die "sandbox: could not create a source worktree under $SANDBOX_ROOT (rerun with PRUNE_SANDBOXES=1 to clear a deleted sandbox)"
  fi
  SRC="$SANDBOX_ROOT/src"
  DATA_DIR="$SANDBOX_ROOT/data"
  CONFIG_DIR="$SANDBOX_ROOT/config"
  BIN_DST="$SANDBOX_ROOT/bin/underclass"
  LABEL="com.underclass.serve.sandbox"
  PLIST="$SANDBOX_ROOT/$LABEL.plist"
  BIND="127.0.0.1:${SANDBOX_PORT:-18080}"
  PORT="${BIND##*:}"
  HEALTH_URL="http://$BIND/v1/models"
  DB="$DATA_DIR/pool.db"
  RELEASES="$DATA_DIR/releases"
  mkdir -p "$DATA_DIR" "$CONFIG_DIR" "$(dirname "$BIN_DST")"
  # Copied from the data directory that applied BEFORE this block reassigned it, so an install
  # driven by UNDERCLASS_DATA_DIR seeds the sandbox with the operator's real data instead of
  # silently minting an empty database. A failed copy is reported rather than swallowed: a sandbox
  # that quietly has no accounts is not exercising the thing it claims to exercise.
  LIVE_DB="$LIVE_DATA_DIR/pool.db"
  if [ ! -f "$DB" ] && [ -f "$LIVE_DB" ]; then
    if sqlite3 "file:$LIVE_DB?mode=ro" ".backup '$DB'" >/dev/null 2>&1; then
      chmod 600 "$DB" 2>/dev/null || true
    else
      printf '  note: could not copy %s into the sandbox; starting from an empty database\n' "$LIVE_DB"
    fi
  elif [ ! -f "$DB" ]; then
    printf '  note: no database at %s; the sandbox starts from an empty one\n' "$LIVE_DB"
  fi
  # Loading the agent is deliberately NOT automatic: `launchctl bootstrap` registers a persistent
  # job that keeps serving on SANDBOX_PORT after this script exits, which would be a stray daemon
  # left behind by a mode advertised as throwaway. The fresh-install path loads it when it writes
  # the agent, and SANDBOX_LOAD=1 opts in for an update that has to exercise the restart:
  #   launchctl bootstrap gui/$(id -u) <root>/com.underclass.serve.sandbox.plist
  if [ "${SANDBOX_LOAD:-0}" = "1" ] && ! job_loaded; then
    launchctl bootstrap "gui/$(id -u)" "$PLIST" >/dev/null 2>&1 || true
  fi
  PLIST_WORKDIR="$SANDBOX_ROOT"
  # A root this script created is throwaway and holds a copy of the live database, so it is removed
  # on exit and its worktree registration is dropped. `--sandbox-root` is the documented way to ask
  # for a root that survives the run, so that case is left alone.
  if [ "$SANDBOX_AUTOCREATED" = "1" ]; then
    cleanup_sandbox() {
      launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
      git -C "$SANDBOX_ROOT/src" worktree remove --force "$SANDBOX_ROOT/src" >/dev/null 2>&1 \
        || git -C "$SRC" worktree prune >/dev/null 2>&1 || true
      rm -rf "$SANDBOX_ROOT"
    }
    trap cleanup_sandbox EXIT
  fi
  printf '\033[1mSANDBOX\033[0m %s (src, data, binary, launchd label, port %s are all isolated)\n' \
    "$SANDBOX_ROOT" "$PORT"
fi


# A worktree has a `.git` file pointing at the real gitdir, not a directory, so test for the
# gitdir rather than requiring a directory. Only the install/update path needs a checkout: `--list`
# and `--rollback` restore installed state, and needing a source tree to undo an install would deny
# recovery exactly when the checkout has moved or been deleted.
if [ "$MODE" = "list" ] || [ "$MODE" = "rollback" ]; then
  :
else
  git -C "$SRC" rev-parse --git-dir >/dev/null 2>&1 || die "not a git checkout: $SRC"
  cd "$SRC"
fi



# The installed agent's own environment wins over this run's defaults, so every path here - the
# probe, the database, the rollback snapshot, the migration preflight, the config directory - measures
# and edits the install that is actually there. An explicitly supplied variable still wins, because
# that is the operator saying where they mean. No plist means nothing to read back.
if [ -f "$PLIST" ]; then
  [ -n "${BIND_EXPLICIT:-}" ] || {
    INSTALLED_BIND="$(plist_env UNDERCLASS_BIND)"
    if [ -n "$INSTALLED_BIND" ]; then
      BIND="$INSTALLED_BIND"
      PORT="${BIND##*:}"
      HEALTH_URL="http://$BIND/v1/models"
    fi
  }
  INSTALLED_PROGRAM="$(plist_program)"
  if [ -n "$INSTALLED_PROGRAM" ] && [ "$INSTALLED_PROGRAM" != "$BIN_DST" ]; then
    if [ -n "$BIN_DST_EXPLICIT" ]; then
      # Installing somewhere the agent does not run would leave the old process serving while the
      # pid/health proof still passes - the stale-image success this script exists to prevent.
      die "BIN_DST ($BIN_DST) is not the path the installed launch agent runs ($INSTALLED_PROGRAM).
        Install to that path, or remove the agent first: launchctl bootout gui/$(id -u)/$LABEL"
    fi
    ok "installing to the path the agent runs: $INSTALLED_PROGRAM"
    BIN_DST="$INSTALLED_PROGRAM"
  fi
  # What the agent actually serves: its pinned value, or the built-in default when it pins none.
  INSTALLED_DATA="$(plist_env UNDERCLASS_DATA_DIR)"
  [ -n "$INSTALLED_DATA" ] || INSTALLED_DATA="$HOME/.local/share/underclass"
  if [ -n "$DATA_DIR_EXPLICIT" ] && [ "$INSTALLED_DATA" != "$DATA_DIR" ]; then
    # The service keeps reading the agent's directory, so snapshotting, migrating and probing the
    # requested one would report a successful update that the running service never uses.
    die "UNDERCLASS_DATA_DIR ($DATA_DIR) is not the directory the installed launch agent uses ($INSTALLED_DATA).
        Install with the agent's directory, or remove the agent first: launchctl bootout gui/$(id -u)/$LABEL"
  fi
  if [ -z "$DATA_DIR_EXPLICIT" ]; then
    DATA_DIR="$INSTALLED_DATA"
    DB="$DATA_DIR/pool.db"
    RELEASES="$DATA_DIR/releases"
  fi
  if [ -z "$CONFIG_DIR_EXPLICIT" ]; then
    INSTALLED_CONFIG="$(plist_env UNDERCLASS_CONFIG_DIR)"
    [ -n "$INSTALLED_CONFIG" ] && CONFIG_DIR="$INSTALLED_CONFIG"
  fi
fi

# Fresh or update, decided AFTER the override above: `detect_fresh_install` tests $BIN_DST, and an
# agent that runs a non-default path only reports that path once it has been read back from the
# plist. Computing this earlier would classify such an install as fresh on every later run.
FRESH=0
if detect_fresh_install; then FRESH=1; fi

# Read AFTER the plist override above: keying this off the pre-override default could pick up a
# leftover pool.db from an earlier default install and send a bearer the real service never issued.
# `--rollback` also clears PROXY_KEY when it replaces the database, so the probe re-reads on demand.
if [ -z "$PROXY_KEY" ] && [ -f "$DB" ]; then
  PROXY_KEY="$(sqlite3 "file:$DB?mode=ro" \
    "SELECT value FROM config WHERE key='proxy_key';" 2>/dev/null || true)"
fi

# ---------------------------------------------------------------- helpers

# Rollback points are timestamp-prefixed, so a reverse lexical sort is newest first. The
# `|| true` matters: with no releases directory `ls` exits non-zero and `pipefail` would turn that
# into a script abort instead of an empty list.
# Dot-prefixed entries are rollback points still being written; they are not releases yet.
list_releases() { { ls -1 "$RELEASES" 2>/dev/null || true; } | grep -v '^\.' | sort -r; }

# A point is restorable only when its manifest says so and its binary is present.
restorable() { [ -f "$RELEASES/$1/underclass" ] && grep -qx 'complete=1' "$RELEASES/$1/MANIFEST" 2>/dev/null; }

release_rev() { cat "$RELEASES/$1/REVISION" 2>/dev/null || printf '?'; }

# lsof is the only way this script can tell whether a port is bound or a database is still held. A
# probe that cannot run must not read as "free" or "unheld": both answers gate destructive steps.
require_lsof() {
  command -v lsof >/dev/null 2>&1 \
    || die "lsof is required to prove the service stopped and the database is not held, and it is not installed.
        Install it (macOS ships it in /usr/sbin) and rerun."
}
port_busy() { lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t >/dev/null 2>&1; }

wait_for_port_free() {
  # Arithmetic loop, not `seq`: `seq` is not part of stock macOS, and a missing binary would make
  # the loop body never run.
  local i
  for ((i = 0; i < 100; i++)); do
    port_busy || return 0
    sleep 0.1
  done
  port_busy && return 1 || return 0
}

# A snapshot that is not a readable, undamaged database with the expected account count must never
# be installed over the live file, so every restore is verified before it is staged.
verify_db() {
  local file="$1" expect="$2" integrity got
  [ -s "$file" ] || die "snapshot is empty: $file"
  # Status-checked explicitly: under `set -e` a bare sqlite3 failure would abort with no message at
  # all, and the whole point of this function is to say which check failed.
  integrity="$(sqlite3 "file:$file?mode=ro" 'PRAGMA integrity_check;' 2>&1)" \
    || die "sqlite3 could not read the snapshot: $integrity"
  [ "$integrity" = "ok" ] || die "snapshot failed integrity_check: $integrity"
  got="$(sqlite3 "file:$file?mode=ro" 'SELECT count(*) FROM accounts;' 2>&1)" \
    || die "sqlite3 could not count accounts in the snapshot: $got"
  [ "$got" = "$expect" ] || die "snapshot has $got accounts, expected $expect"
}

# `.backup` uses the online-backup API, which is consistent while the daemon holds the file open.
# The snapshot is taken while the daemon is still writing, so the account count must be read from
# the COPY and checked against the copy again. Comparing it with a later read of the live file would
# fail whenever an account was added or removed in between, aborting the update for no reason.
snapshot_db() {
  local dst="$1" accounts
  [ -f "$DB" ] || die "database not found: $DB"
  sqlite3 "file:$DB?mode=ro" ".backup '$dst'" >/dev/null \
    || die "sqlite3 .backup failed; no snapshot was written to $dst"
  accounts="$(sqlite3 "file:$dst?mode=ro" 'SELECT count(*) FROM accounts;')" \
    || die "snapshot written but unreadable: $dst"
  verify_db "$dst" "$accounts"
  printf '%s' "$accounts"
}

# The proxy key can be unset while this runs on a fresh install: the daemon mints it into the
# database on first start, which is after the key was read at the top of this script. Reading it
# again here is what makes the probe work without the operator exporting anything; /v1/models is
# behind proxy::require_proxy_key, so an empty bearer token would never see a 200.
probe_health() {
  local code
  if [ -z "$PROXY_KEY" ] && [ -f "$DB" ]; then
    PROXY_KEY="$(sqlite3 "file:$DB?mode=ro" \
      "SELECT value FROM config WHERE key='proxy_key';" 2>/dev/null || true)"
  fi
  # The key is fed on stdin, never as an argument: process arguments are visible to other local
  # users through `ps`, and this token gates the loopback API.
  code="$(printf 'Authorization: Bearer %s\n' "$PROXY_KEY" | \
    curl -s --noproxy '*' -o /dev/null -m 5 -w '%{http_code}' "$HEALTH_URL" \
      -H @- 2>/dev/null || true)"
  [ "$code" = "200" ] || { printf '  GET %s -> %s\n' "$HEALTH_URL" "$code" >&2; return 1; }
}

# A running job's identity: pid plus process start time. The pair is what distinguishes a real
# restart from a kickstart that exited 0 while the process kept its previous image, and the start
# time also stops a recycled pid from being mistaken for a new process.
job_identity() {
  local pid lstart
  pid="$(launchctl print "gui/$(id -u)/$LABEL" 2>/dev/null \
    | awk '/^[[:space:]]*pid = /{print $3; exit}')"
  [ -n "$pid" ] || return 1
  lstart="$(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/^ *//;s/ *$//')"
  printf '%s %s' "$pid" "$lstart"
}

# Restart through launchd, then prove it: a new job identity AND /v1/models answering 200. The
# previous identity is captured first, and the same identity afterwards is a failure - it means the
# process was never replaced, whatever `kickstart` returned. Returns non-zero instead of dying so
# callers can report the exact rollback command.
# Poll until the identity differs from $1 and the service answers. Returns 0 on success.
wait_for_restart() {
  local before="$1" after
  local i
  for ((i = 0; i < 300; i++)); do
    after="$(job_identity || true)"
    if [ -n "$after" ] && [ "$after" != "$before" ] && probe_health 2>/dev/null; then
      printf '  pid %s -> %s\n' "${before%% *}" "${after%% *}"
      return 0
    fi
    sleep 0.1
  done
  return 1
}

# Restart through launchd and prove the process actually changed.
#
# `launchctl kickstart` exiting 0 is not evidence: when the file was replaced under a running
# process, the job stays up on the old image and kickstart can report success while nothing is
# replaced (this is how a service sat on a 22-hour-old binary while reporting a clean update). The
# evidence is a different pid AND a different process start time AND /v1/models answering 200.
#
# If the identity is unchanged, escalate once to bootout + bootstrap from the plist, which starts a
# genuinely new process, and re-verify. Only when that also fails is the run aborted, with the
# command that repairs it.
restart_and_probe() {
  local before after
  launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 \
    || { printf '  %s is not loaded\n' "$LABEL" >&2; return 1; }
  before="$(job_identity || true)"
  launchctl kickstart -k "gui/$(id -u)/$LABEL" >/dev/null 2>&1 \
    || { printf '  kickstart failed\n' >&2; return 1; }
  wait_for_restart "$before" && return 0

  after="$(job_identity || true)"
  if [ -n "$before" ] && [ "$after" = "$before" ]; then
    printf '  kickstart returned success but the process is unchanged (pid %s); escalating to bootout + bootstrap\n' \
      "${after%% *}" >&2
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    wait_for_port_free || printf '  warning: port %s is still bound\n' "$PORT" >&2
    if launchctl bootstrap "gui/$(id -u)" "$PLIST" >/dev/null 2>&1; then
      wait_for_restart "$before" && return 0
    else
      printf '  bootstrap from %s failed\n' "$PLIST" >&2
    fi
  fi

  probe_health 2>/dev/null || true
  printf '  the service is not running the newly installed binary\n' >&2
  printf '  repair with:\n' >&2
  printf '    launchctl bootout gui/%s/%s; launchctl bootstrap gui/%s %s\n' \
    "$(id -u)" "$LABEL" "$(id -u)" "$PLIST" >&2
  return 1
}

# A fresh install has no previous identity to differ from, so its proof is the other half of the
# same requirement: a job that is running with a real pid AND /v1/models answering 200.
wait_for_new_job() {
  local ident i
  for ((i = 0; i < 300; i++)); do
    ident="$(job_identity || true)"
    if [ -n "$ident" ] && probe_health 2>/dev/null; then
      printf '  pid %s\n' "${ident%% *}"
      return 0
    fi
    sleep 0.1
  done
  probe_health >/dev/null 2>&1 || printf '  %s never answered 200\n' "$HEALTH_URL" >&2
  return 1
}

reload_job() {
  [ -f "$PLIST" ] || die "launch agent plist not found: $PLIST"
  launchctl bootstrap "gui/$(id -u)" "$PLIST" >/dev/null 2>&1 \
    || die "could not load $LABEL; load it with: launchctl bootstrap gui/$(id -u) $PLIST"
}

# ---------------------------------------------------------------- --list

if [ "$MODE" = "list" ]; then
  say "Rollback points in $RELEASES"
  if [ -z "$(list_releases)" ]; then
    ok "none yet; the next update creates the first one"
  else
    for name in $(list_releases); do
      dir="$RELEASES/$name"
      rev="$(release_rev "$name")"
      prev="$(cat "$dir/PREVIOUS_REVISION" 2>/dev/null || printf '?')"
      db="$(grep '^db_state=' "$dir/MANIFEST" 2>/dev/null | cut -d= -f2 || true)"
      # A release with no saved binary cannot be restored; say so rather than implying it can.
      if restorable "$name"; then bin="saved"; else bin="MISSING (not restorable)"; fi
      printf '  %-28s restores %-9s (after %-9s) binary %-20s db %s\n' \
        "$name" "$prev" "$rev" "$bin" "${db:-none}"
    done
  fi
  exit 0
fi

# ---------------------------------------------------------------- --rollback

if [ "$MODE" = "rollback" ]; then
  require_lsof
  if [ -n "$TARGET_REV" ]; then
    # Matched against the revision the point RESTORES (`PREVIOUS_REVISION`), which is the one an
    # operator names when they want that build back. `REVISION` is accepted too, so a point can also
    # be addressed by the update that created it.
    REL=""
    for name in $(list_releases); do
      # Only restorable points: a release is written with no binary when $BIN_DST was absent at
      # deploy time, and such an entry must not shadow an older restorable point for the revision.
      restorable "$name" || continue
      if [ "$(cat "$RELEASES/$name/PREVIOUS_REVISION" 2>/dev/null || true)" = "$TARGET_REV" ] \
        || [ "$(release_rev "$name")" = "$TARGET_REV" ]; then
        REL="$RELEASES/$name"
        break
      fi
    done
    [ -n "$REL" ] || die "no rollback point for revision $TARGET_REV (see --list)"
  else
    # The newest release is not always restorable: the very first update has no previous binary
    # to save, so its directory holds a database snapshot only. Walk past unusable entries rather
    # than failing on the newest one.
    REL=""
    for name in $(list_releases); do
      if restorable "$name"; then
        REL="$RELEASES/$name"
        break
      fi
    done
    [ -n "$REL" ] || die "no restorable rollback points in $RELEASES (see --list)"
  fi

  # The point records the revision the update INSTALLED (`REVISION`) and the one it replaced
  # (`PREVIOUS_REVISION`). The binary saved in the directory is the replaced one, so that is what a
  # rollback restores and what it must be labelled with - naming the new revision would tell an
  # operator their rollback did not take effect. Matching `--rollback REV` uses the same field, so
  # "restore the point recorded for that revision" means the revision you want back.
  REV="$(cat "$REL/PREVIOUS_REVISION" 2>/dev/null || release_rev "$(basename "$REL")")"
  BIN_SRC="$REL/underclass"
  DB_SRC="$REL/pool.db"
  [ -f "$BIN_SRC" ] || die "rollback point is missing its binary: $BIN_SRC"

  say "Rolling back to $REV ($(basename "$REL"))"
  printf '  binary:   %s (%s)\n' "$BIN_SRC" "$(ls -lh "$BIN_SRC" | awk '{print $5}')"
  if [ "$KEEP_DB" = "1" ]; then
    printf '  database: keeping the live %s\n' "$DB"
  elif [ -f "$DB_SRC" ]; then
    printf '  database: %s (%s)\n' "$DB_SRC" "$(ls -lh "$DB_SRC" | awk '{print $5}')"
    printf '            usage recorded since the update is discarded with it\n'
  else
    die "rollback point has no database snapshot: $DB_SRC"
  fi

  if [ "$DRY_RUN" = "1" ]; then
    say "Dry run: would install $BIN_SRC and restart $LABEL"
    exit 0
  fi

  # Stage, verify, then swap: nothing live is overwritten until every input has been checked.
  # Beside the database, so the final `mv` is a rename on one filesystem. A staging directory in
  # $TMPDIR can be a different volume, where `mv` degrades to copy-then-unlink and writes the live
  # file in place - the torn copy this staging exists to prevent.
  # Beside the BINARY, so `mv` is a rename: the data dir is overridable and may be another volume,
  # where a cross-device `mv` would write the live binary in place - the direct-copy install the
  # header says never happens. The database is staged separately, beside itself, for the same reason.
  mkdir -p "$(dirname "$BIN_DST")" || die "could not create $(dirname "$BIN_DST")"
  mkdir -p "$(dirname "$DB")" || die "could not create $(dirname "$DB")"
  STAGE="$(mktemp -d "$(dirname "$BIN_DST")/.underclass-rollback.XXXXXX")" \
    || die "could not stage beside $BIN_DST"
  DB_STAGE="$(mktemp -d "$(dirname "$DB")/.underclass-rollback-db.XXXXXX")" \
    || die "could not stage beside $DB"
  # Chained, not replaced: an auto-created sandbox root already installed a cleanup trap, and
  # overwriting it would leave that root (with its copy of the live database) and its worktree
  # registration behind.
  if [ "${SANDBOX_AUTOCREATED:-0}" = "1" ]; then
    trap 'cleanup_sandbox; rm -rf "$STAGE" "$DB_STAGE"' EXIT
  else
    trap 'rm -rf "$STAGE" "$DB_STAGE"' EXIT
  fi
  cp "$BIN_SRC" "$STAGE/underclass"
  chmod 755 "$STAGE/underclass"
  EXPECT_SHA="$(grep '^binary_sha256=' "$REL/MANIFEST" 2>/dev/null | cut -d= -f2 || true)"
  GOT_SHA="$(shasum -a 256 "$STAGE/underclass" | awk '{print $1}')"
  if [ -n "$EXPECT_SHA" ] && [ "$EXPECT_SHA" != "none" ]; then
    [ "$GOT_SHA" = "$EXPECT_SHA" ] || die "staged binary does not match the recorded hash; point is corrupt"
  else
    [ "$GOT_SHA" = "$(shasum -a 256 "$BIN_SRC" | awk '{print $1}')" ] || die "staged binary copy is corrupt"
  fi
  if [ "$KEEP_DB" != "1" ]; then
    cp "$DB_SRC" "$DB_STAGE/pool.db" || die "could not stage the database snapshot"
    chmod 600 "$DB_STAGE/pool.db"
    verify_db "$DB_STAGE/pool.db" "$(sqlite3 "file:$DB_SRC?mode=ro" 'SELECT count(*) FROM accounts;')"
  fi

  BOOTED_OUT=0
  if [ "$KEEP_DB" != "1" ]; then
    # Replacing the file under a running daemon would leave it writing to an unlinked inode, so
    # the job is removed from the domain (KeepAlive cannot resurrect it in that state).
    say "Stopping the daemon to restore the database"
    # `bootout` addresses the job by label and needs no plist file, and it is the only stop that
    # sticks: KeepAlive/SuccessfulExit=false resurrects a plain `stop`, which would either keep the
    # port bound or let the database be replaced under a daemon that is reopening it.
    if job_loaded; then
      launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
      BOOTED_OUT=1
    fi
    if ! wait_for_port_free; then
      [ "$BOOTED_OUT" = "1" ] && reload_job
      die "port $PORT still bound; refusing to replace the database under a live daemon"
    fi
    # A job can be unloaded while its process is still alive and holding the file (hung, or bound to
    # another port), so the port alone is not evidence. Same guard the migration runner uses.
    if [ -e "$DB" ]; then
      LSOF_ERR="$(lsof -nP -- "$DB" 2>&1 >/dev/null)" || LSOF_RC=$?
      LSOF_RC="${LSOF_RC:-0}"
      if [ "$LSOF_RC" = "0" ]; then
        [ "$BOOTED_OUT" = "1" ] && reload_job
        die "a process still has $DB open; refusing to replace it"
      elif [ -n "$LSOF_ERR" ]; then
        [ "$BOOTED_OUT" = "1" ] && reload_job
        die "could not confirm no writer holds $DB: $LSOF_ERR"
      fi
    fi
    ok "daemon stopped"
  fi


  if [ "$KEEP_DB" != "1" ]; then
    # Every sidecar of the replaced file: a hot rollback journal left by an unclean stop would be
    # replayed into the restored database.
    rm -f "$DB-wal" "$DB-shm" "$DB-journal"
    mv -f "$DB_STAGE/pool.db" "$DB"
    ok "restored $DB"
    # The restored file may carry a different minted key than the one read at startup, so the cache
    # must be dropped or the health proof would send a bearer that file never issued.
    PROXY_KEY=""
  fi

  # The binary moves into place only after the database restore succeeded: a failed restore must not
  # leave the rolled-back binary paired with the post-update database.
  mv -f "$STAGE/underclass" "$BIN_DST" || die "could not install the restored binary; $DB was already restored"
  # The installed revision changed, so the record must follow it: the next deploy uses it to label a
  # point, and a stale value would make `--rollback <rev>` install a build it does not claim.
  mkdir -p "$RELEASES"
  printf '%s\n' "$REV" > "$RELEASES/.installed-revision"
  printf '%s\n' "$(cat "$REL/PREVIOUS_COMMIT" 2>/dev/null || printf '%s' "$REV")" \
    >> "$RELEASES/.installed-revision"
  ok "installed $BIN_DST ($REV)"

  # Outside the KEEP_DB guard: `--rollback --keep-db` is one of the repair commands this script
  # prints, and it must work in the agent state those messages describe (unloaded). `BOOTED_OUT`
  # alone is not enough for the same reason - an earlier failed restart leaves the job unloaded.
  if [ "$RESTART" = "1" ] && ! job_loaded; then
    reload_job
    ok "loaded $LABEL"
  fi

  if [ "$RESTART" = "0" ]; then
    say "Skipped the restart check (--no-restart)"
    if job_loaded; then
      printf '  restart with: launchctl kickstart -k gui/%s/%s\n' "$(id -u)" "$LABEL"
    else
      printf '  the agent is not loaded; start it with: launchctl bootstrap gui/%s %s\n' \
        "$(id -u)" "$PLIST"
    fi
    exit 0
  fi

  say "Restarting the service"
  restart_and_probe || die "service is not healthy after rollback to $REV; see $DATA_DIR/underclass.log"
  ok "healthy on $BIND"
  if [ "$SANDBOX" = "1" ]; then
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    printf '  sandbox: agent booted out again; nothing is left running\n'
  fi
  printf '\n\033[1mRolled back to %s\033[0m\n' "$REV"
  exit 0
fi

# ---------------------------------------------------------------- 1. update the source
[ "$TARGET_REV" = "" ] || die "unexpected argument: $TARGET_REV (did you mean --rollback $TARGET_REV?)"

say "Fetching $REMOTE"
git fetch "$REMOTE" --prune --quiet
git fetch upstream --quiet 2>/dev/null || true
ok "fetched"

# A working tree with tracked edits builds a binary that matches no commit, while the run reports it
# as $NEW_REV and records that revision in the release manifest - so --list and --rollback would
# reason about a revision that is not what got installed. Checked on every install/update path, not
# just when a branch switch happens to be needed.
if [ "$SANDBOX" != "1" ] && [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  die "tracked changes present in $SRC; commit or stash them before deploying a build"
fi

# The sandbox is a detached worktree of the canonical checkout: it must not check out `main`
# (git refuses, because that branch belongs to the real worktree) and it must not move the branch
# either. It fast-forwards by resetting its detached HEAD to the same commit instead.
if [ "$SANDBOX" = "1" ]; then
  if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    die "sandbox source has tracked changes; recreate it with a fresh --sandbox-root"
  fi
  ok "sandbox source at $(git rev-parse --short HEAD) (detached)"
elif [ "$DRY_RUN" = "1" ] && [ "$(git rev-parse --abbrev-ref HEAD)" != "$BRANCH" ]; then
  ok "dry run: would switch $(git rev-parse --abbrev-ref HEAD) -> $BRANCH"
elif [ "$SANDBOX" != "1" ] && [ "$(git rev-parse --abbrev-ref HEAD)" != "$BRANCH" ]; then
  say "Switching to $BRANCH"
  if git worktree list --porcelain | grep -qx "worktree $SRC" && [ -d .git ]; then
    # This checkout owns the branch; a dirty tree would block the switch.
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
      die "tracked changes present on $(git rev-parse --abbrev-ref HEAD); commit or stash first"
    fi
  fi
  git checkout "$BRANCH" --quiet
  ok "on $BRANCH"
fi

# The checkout originally tracked upstream/main, which does NOT contain our fork's work.
# Point it at our fork so 'latest' means our merged main, not ghuntley's.
if [ "$SANDBOX" != "1" ] && [ "$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)" != "$REMOTE/$BRANCH" ]; then
  if [ "$DRY_RUN" = "1" ]; then
    ok "dry run: would retarget $BRANCH to track $REMOTE/$BRANCH"
  else
    say "Retargeting $BRANCH to track $REMOTE/$BRANCH"
    git branch --set-upstream-to="$REMOTE/$BRANCH" "$BRANCH" >/dev/null
    ok "upstream set to $REMOTE/$BRANCH"
  fi
fi

# Measured against the branch being deployed, not against whatever HEAD currently is: a --dry-run
# from a feature branch deliberately does not switch, and comparing that HEAD with $REMOTE/$BRANCH
# would report the feature's own commits as divergence on $BRANCH.
if [ "$DRY_RUN" = "1" ] && [ "$(git rev-parse --abbrev-ref HEAD)" != "$BRANCH" ]; then
  AHEAD=$(git rev-list --count "$REMOTE/$BRANCH..$BRANCH")
  BEHIND=$(git rev-list --count "$BRANCH..$REMOTE/$BRANCH")
else
  AHEAD=$(git rev-list --count "$REMOTE/$BRANCH..HEAD")
  BEHIND=$(git rev-list --count "HEAD..$REMOTE/$BRANCH")
fi
printf '  %s: %s ahead, %s behind %s/%s\n' "$BRANCH" "$AHEAD" "$BEHIND" "$REMOTE" "$BRANCH"

# The canonical checkout must not build from a divergent tree, because "latest" would then mean
# something only this machine has. The sandbox is the exception by construction: it builds a detached
# worktree of the CURRENT HEAD, which is exactly how a change to this script is exercised before it
# is pushed.
if [ "$AHEAD" != "0" ] && [ "$SANDBOX" != "1" ]; then
  die "$BRANCH has $AHEAD local commit(s) not on $REMOTE/$BRANCH; refusing to rebuild from a divergent tree"
fi
if [ "$AHEAD" != "0" ]; then
  ok "sandbox: building $AHEAD local commit(s) not on $REMOTE/$BRANCH (detached worktree)"
fi

OLD_REV=$(git rev-parse --short HEAD)
OLD_FULL=$(git rev-parse HEAD)
if [ "$BEHIND" != "0" ]; then
  if [ "$DRY_RUN" = "1" ]; then
    ok "dry run: would fast-forward $OLD_REV -> $(git rev-parse --short "$REMOTE/$BRANCH")"
  else
    say "Fast-forwarding $OLD_REV -> $(git rev-parse --short "$REMOTE/$BRANCH")"
    if [ "$SANDBOX" = "1" ] && [ "$AHEAD" != "0" ]; then
      # Both ahead and behind: resetting to the remote branch would discard the very commit under
      # test, and the health verdict would prove nothing about it. Leave the detached worktree where
      # it is and say so.
      ok "sandbox: not fast-forwarding (it has local commits under test)"
    elif [ "$SANDBOX" = "1" ]; then
      git reset --hard --quiet "$REMOTE/$BRANCH"
    else
      git merge --ff-only "$REMOTE/$BRANCH" --quiet
    fi
    ok "now at $(git rev-parse --short HEAD)"
  fi
else
  ok "already up to date at $OLD_REV"
fi

NEW_REV=$(git rev-parse --short HEAD)
NEW_FULL=$(git rev-parse HEAD)

# ---------------------------------------------------------------- 2. build (service untouched)
say "Building release binary"
if [ "$DRY_RUN" = "1" ]; then
  ok "dry run: skipping cargo build"
else
  if ! cargo build --release --locked 2>&1 | tail -5; then
    die "build failed; the running service was not touched"
  fi
  [ -x target/release/underclass ] || die "build produced no binary"
  ok "built $(ls -lh target/release/underclass | awk '{print $5}') from $NEW_REV"
fi

# ---------------------------------------------------------------- 3a. migration preflight
require_lsof
# Before the install, not after: the launch agent has RunAtLoad, so a new binary left on disk starts
# on the next login or reboot even if this run aborted. A database the runner cannot read must
# therefore stop the deploy while the previous binary is still the installed one, not after the
# new build has already replaced it.
MIGRATED=0
MIGRATE_CHECK=0
# Set when this run writes the launch agent, so later advice knows a reload (not a kickstart) is
# required: launchd does not re-read a plist on kickstart.
PLIST_WRITTEN=0
if [ -f "$DB" ]; then
  # A missing sibling is a hard error whenever there is a database. It is NOT gated on the restart
  # mode: the runner is part of this copy of the tooling, and its absence is reported the same way
  # whether or not this particular run would have invoked it.
  if [ ! -x "$MIGRATE" ]; then
    if [ "$DRY_RUN" = "1" ]; then
      # A dry run reports what a real run would refuse; it must not fail on a condition it cannot fix.
      ok "dry run: the migration runner is missing ($MIGRATE); a real run would refuse here"
      MIGRATE_MISSING=1
    else
      die "the migration runner is missing or not executable: $MIGRATE
        It ships beside this script ($(dirname "$SELF")). Restore it, or point MIGRATE at a copy.
        Nothing was installed; the previous binary is still in place."
    fi
  fi
  if [ "${MIGRATE_MISSING:-0}" = "1" ] && [ "$DRY_RUN" = "1" ]; then
    say "Dry run complete: a real run would refuse here and install nothing"
    exit 0
  fi
  if [ "${MIGRATE_MISSING:-0}" = "0" ]; then
    set +e
    "$MIGRATE" --db "$DB" --check
    MIGRATE_CHECK=$?
    set -e
    case "$MIGRATE_CHECK" in
    0) ok "no pending database migrations" ;;
    1) MIGRATED=1 ;;
    *)
      if [ "$DRY_RUN" = "1" ]; then
        say "Dry run complete: the migration check fails (exit $MIGRATE_CHECK); a real run would refuse here and install nothing"
        exit 0
      else
        die "database migration check failed (exit $MIGRATE_CHECK); nothing was installed and the service was NOT stopped.
        Run: $MIGRATE --db $DB --check"
      fi ;;
    esac
  fi
fi

# The dry run exits here, AFTER the preflight, so its report covers the checks a real run performs:
# a plan that says "would install" while the real run would refuse on pending migrations or a
# missing runner is worse than no plan.
if [ "$DRY_RUN" = "1" ]; then
  # Report the same refusals a real run would hit, in the same order, so a plan never promises an
  # install and a restart that the real run declines to perform.
  if [ "${MIGRATED:-0}" = "1" ] && [ "$RESTART" = "0" ]; then
    ok "dry run: migrations are PENDING and --no-restart was given; a real run would REFUSE and install nothing"
    printf '  run: %s            # migrate, install and restart in one go\n' "$SELF"
    printf '  or:  %s --db %s   # migrate by hand first\n' "$MIGRATE" "$DB"
    exit 0
  fi
  [ "${MIGRATED:-0}" = "1" ] && ok "dry run: migrations are PENDING; a real run would apply them"
  # The no-op decision cannot be settled without building: the real run rebuilds after the
  # fast-forward, and only then is the produced binary comparable with the installed one. Reporting
  # either answer as fact would misstate the plan, so the dry run states the condition instead.
  if [ "$BEHIND" != "0" ]; then
    say "Dry run complete: would fast-forward, build, record a rollback point, install to $BIN_DST, and restart $LABEL"
  elif [ "${MIGRATED:-0}" = "1" ]; then
    say "Dry run complete: would apply migrations, record a rollback point, install to $BIN_DST, and restart $LABEL"
  else
    say "Dry run complete: would build against $NEW_REV, install to $BIN_DST, and restart $LABEL"
    ok "a rollback point is recorded only if the rebuilt binary differs from the installed one"
    ok "nothing is fetched and nothing is pending, so an identical rebuild records no point"
  fi
  exit 0
fi
# `--no-restart` promises that this run starts nothing. Applying a migration needs the daemon booted
# out and then reloaded, which would start the new binary with no identity or health proof - so the
# work is reported, not performed, and the operator is given the exact sequence.
# `--no-restart` and pending migrations cannot both be honoured safely. Applying a migration needs
# the daemon stopped, and installing the new binary without applying it leaves a RunAtLoad agent that
# starts that binary against un-migrated rows on the next login or reboot. Refusing here keeps the
# previous binary installed and the service untouched; nothing has been written at this point.
if [ "$MIGRATED" = "1" ] && [ "$RESTART" = "0" ]; then
  die "migrations are pending in $DB and --no-restart cannot apply them.
        Nothing was installed and the service is untouched. Either:
          $SELF                          # migrate, install and restart in one run
          $MIGRATE --db $DB              # or migrate by hand first, then rerun with --no-restart"
fi

# ---------------------------------------------------------------- 3b. stage the new binary (before any migration)
# Before the migration, so a copy failure happens while the previous binary still matches the data.
# `mv -f` here would defeat that, so the file stays as `$BIN_DST.new` until step 4.
mkdir -p "$(dirname "$BIN_DST")"
cp target/release/underclass "$BIN_DST.new" || die "could not stage the new binary; the running service and its database were not touched"
chmod 755 "$BIN_DST.new"

# ---------------------------------------------------------------- 3. record the rollback point
# Written before the install, because the window that matters is the one where the new binary is
# on disk and the old one is gone.
# A no-op run records nothing: an update that fetched nothing and installs the revision already
# installed has no state worth going back to, and recording it anyway would churn the bounded
# `KEEP_RELEASES` window until the release that holds a real pre-update binary was pruned away.
if [ "$OLD_FULL" = "$NEW_FULL" ] && [ -x "$BIN_DST" ] && [ "$MIGRATED" != "1" ] \
   && [ "$(shasum -a 256 "$BIN_DST" | awk '{print $1}')" = "$(shasum -a 256 target/release/underclass | awk '{print $1}')" ]; then
  say "Recording a rollback point"
  ok "skipped: $NEW_REV is already installed and the binary is identical"
  # Nothing was replaced and no rewrite is pending, so the advice below must not promise a point for
  # this run. The newest restorable point was written by an earlier deploy and restores whatever THAT
  # deploy replaced. A run that DOES migrate never takes this branch - it must leave a pre-migration
  # database snapshot behind.
  ROLLBACK_POINT_RECORDED=0
else
say "Recording a rollback point"
ROLLBACK_POINT_RECORDED=1
mkdir -p "$RELEASES"
# Built under a name `list_releases` ignores and renamed into place only when complete, so a run
# interrupted mid-copy never leaves something that looks like a usable release.
REL_NAME="$(date +%Y%m%dT%H%M%S)-$OLD_REV"
REL="$RELEASES/.incomplete-$REL_NAME"
rm -rf "$REL"
mkdir -p "$REL"
# Any failure from here to the rename removes the half-written point rather than leaving it. Hooked
# on EXIT as well as called explicitly: the point holds a copy of the database (with the minted proxy
# key), and a dot-prefixed leftover is invisible to `--list` and therefore never pruned.
REL_COMMITTED=0
cleanup_incomplete_release() {
  [ "${REL_COMMITTED:-0}" = "1" ] || rm -rf "$REL"
}
# `STAGE` belongs to the --rollback path and is unset here, so it is expanded defensively: under
# `set -u` an unbound name in an EXIT trap would turn a successful install into a non-zero exit.
if [ "${SANDBOX_AUTOCREATED:-0}" = "1" ]; then
  trap 'cleanup_incomplete_release; cleanup_sandbox; [ -z "${STAGE:-}" ] || rm -rf "$STAGE"' EXIT
else
  trap 'cleanup_incomplete_release; [ -z "${STAGE:-}" ] || rm -rf "$STAGE"' EXIT
fi
# The revision the INSTALLED binary was built from, which is what a rollback actually restores. The
# checkout's HEAD can be ahead of it - a run that fast-forwards and then fails to build leaves the
# two different - so recording HEAD here would name a revision the point does not contain and make
# `--rollback REV` install a different build than it claims.
INSTALLED_REV="$OLD_REV"
if [ -f "$RELEASES/.installed-revision" ]; then
  INSTALLED_REV="$(head -n1 "$RELEASES/.installed-revision" 2>/dev/null || printf '%s' "$OLD_REV")"
  [ -n "$INSTALLED_REV" ] || INSTALLED_REV="$OLD_REV"
fi
printf '%s' "$INSTALLED_REV" > "$REL/PREVIOUS_REVISION"
printf '%s' "$OLD_FULL" > "$REL/PREVIOUS_COMMIT"
printf '%s' "$NEW_REV" > "$REL/REVISION"
printf '%s' "$NEW_FULL" > "$REL/COMMIT"

BIN_SHA="none"
if [ -f "$BIN_DST" ]; then
  # Hashed from the source, so comparing it with the saved copy detects a truncated copy - hashing
  # the copy would compare the file with itself and pass whatever happened.
  BIN_SHA="$(shasum -a 256 "$BIN_DST" | awk '{print $1}')"
  cp "$BIN_DST" "$REL/underclass" || { cleanup_incomplete_release; die "could not save the previous binary"; }
  chmod 755 "$REL/underclass"
  [ "$(shasum -a 256 "$REL/underclass" | awk '{print $1}')" = "$BIN_SHA" ] \
    || { cleanup_incomplete_release; die "saved binary does not match $BIN_DST"; }
  ok "previous binary saved ($(ls -lh "$REL/underclass" | awk '{print $5}'))"
else
  ok "no previous binary to save (first install)"
fi

DB_STATE="none"
if [ -f "$DB" ]; then
  DB_ACCOUNTS="$(snapshot_db "$REL/pool.db")"
  chmod 600 "$REL/pool.db"
  DB_STATE="$DB_ACCOUNTS accounts, integrity ok, $(du -h "$REL/pool.db" | cut -f1)"
  ok "database snapshotted ($DB_STATE)"
else
  ok "no database to snapshot"
fi

cat > "$REL/MANIFEST" <<MANIFEST_EOF
previous_revision=$INSTALLED_REV
new_revision=$NEW_REV
binary_sha256=$BIN_SHA
db_state=$DB_STATE
created_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
source=$SRC
MANIFEST_EOF
printf 'complete=1\n' >> "$REL/MANIFEST"
# Flagged AFTER the rename: set before it, a failed `mv` would leave the half-written directory (with
# its copy of pool.db) behind forever, since a dot-prefixed entry is invisible to --list and pruning.
mv "$REL" "$RELEASES/$REL_NAME" || { rm -rf "$REL"; die "could not finalise the rollback point"; }
REL_COMMITTED=1
REL="$RELEASES/$REL_NAME"
ok "release $(basename "$REL")"

# A stack of full database copies is not free; keep the newest KEEP_RELEASES.
STALE="$(list_releases | tail -n +$((KEEP_RELEASES + 1)))"
if [ -n "$STALE" ]; then
  printf '%s\n' "$STALE" | while IFS= read -r old; do
    if [ -n "$old" ]; then rm -rf "${RELEASES:?}/$old"; fi
  done
  ok "pruned old rollback points (keeping $KEEP_RELEASES)"
fi
fi

# ---------------------------------------------------------------- 3c. data migration
# Runs BEFORE the new binary is installed. Renames that touch persisted identifiers must be applied
# while the previous binary is still the installed one, because the launch agent has RunAtLoad: a new
# binary on disk with un-migrated data would start on the next login or reboot even though this run
# aborted. Applying it earlier shrinks that window to zero - any failure here leaves the previous
# binary and an untouched database.
#
# The daemon must be stopped: underclass writes through on every request and `sync_from_store`
# reloads the account table every 10s, so a rewrite racing it can be half-applied. The runner refuses
# a database with a live writer, which is why the job is booted out first - the same mechanism the
# rollback path uses, because KeepAlive:SuccessfulExit=false resurrects a plain `launchctl stop`.
#
# The decision itself was made in 3a, so a database that is already current costs no restart. The
# rollback point above holds a consistent pre-migration snapshot and the runner takes its own backup,
# so there are two independent ways back.
if [ "$MIGRATED" = "1" ]; then
  say "Migrating database"
  BOOTED_OUT=0
  if job_loaded; then
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    BOOTED_OUT=1
  fi
  if ! wait_for_port_free; then
    # Nothing has been installed yet, so leaving the job down is harmless and keeps the unmigrated
    # database away from a binary that would misread it.
    die "port $PORT still bound; the database was not touched and nothing was installed.
        Stop whatever holds the port, then rerun."
  fi
  ok "daemon stopped"

  set +e
  "$MIGRATE" --db "$DB"
  MIGRATE_RUN=$?
  set -e
  if [ "$MIGRATE_RUN" != "0" ]; then
    # The runner cannot report which half failed after the fact, and neither answer is safe to
    # assume: its rewrite is one transaction that either committed and then failed verification, or
    # rolled back entirely. Say exactly that instead of claiming the database is untouched.
    die "database migration failed (runner exit $MIGRATE_RUN).
        It either committed and then failed its own verification, or rolled back and left $DB unchanged.
        Inspect $DATA_DIR/migrations. Nothing was installed; restore a consistent pair with:
          $SELF --rollback"
  fi

  # Deliberately left DOWN until the new binary is installed and verified. Reloading the previous
  # binary here would let it write rows in the pre-migration format it was built for, which the
  # runner's own accounting cannot detect afterwards. The restart at step 5 starts the new binary.
  [ "$BOOTED_OUT" = "1" ] && ok "the service stays stopped until the new binary is installed"
fi

# ---------------------------------------------------------------- 4. install
# `write_plist` points StandardOutPath/StandardErrorPath at $DATA_DIR, and launchd opens those paths
# when it spawns the job, so the directory must exist before the agent is loaded - on a clean host
# nothing has created it yet.
mkdir -p "$DATA_DIR"

# Staged then moved: `install` or a direct copy over a binary that a launchd job is running leaves
# the old process alive on the old image while the path on disk is new. The restart proof below is
# what catches that if it ever happens again.
#
# The copy is staged (step 3b) BEFORE the migration and only moved into place afterwards, so a
# failure to copy cannot leave rewritten data under the previous binary - which, for a rename-style
# migration, may no longer understand it.
say "Installing binary"
[ -f "$BIN_DST.new" ] || die "the staged binary is missing; the migration was not run"
mv -f "$BIN_DST.new" "$BIN_DST"
mkdir -p "$RELEASES"
printf '%s\n' "$NEW_REV" > "$RELEASES/.installed-revision"
printf '%s\n' "$NEW_FULL" >> "$RELEASES/.installed-revision"
ok "$BIN_DST ($NEW_REV)"

# ---------------------------------------------------------------- 4a. config
# `config.example.toml` is the canonical every-key-at-its-default reference (it has a drift test
# against the file config loader), so a copy is installed beside the live config on every run and
# the operator can diff their configuration against it after an update.
#
# `config.toml` is created only when it is absent OR EMPTY, and never overwritten. An empty file
# means the operator never configured anything, so it carries no intent to preserve and leaving it
# would keep the service on defaults anyway; a file with any content in it is left alone verbatim.
#
# A missing example must never abort the run: by this point the new binary is already installed, so
# `set -e` on a bare `cp` would leave the update half-done with no restart and no message. One
# guard covers every branch below.
say "Installing configuration"
# Nothing here is fatal. This step runs AFTER the new binary is installed, so a `set -e` abort on an
# unwritable config dir would leave the update half-done: new binary on disk, migration unapplied,
# service never restarted. Every failure is reported and the run continues to the migration/restart.
if [ ! -f "$SRC/config.example.toml" ]; then
  printf '  note: %s/config.example.toml not found; configuration untouched\n' "$SRC"
elif ! mkdir -p "$CONFIG_DIR"; then
  printf '  note: could not create %s; configuration untouched\n' "$CONFIG_DIR"
elif ! cp "$SRC/config.example.toml" "$CONFIG_DIR/config.example.toml"; then
  printf '  note: could not write %s/config.example.toml; configuration untouched\n' "$CONFIG_DIR"
else
  chmod 644 "$CONFIG_DIR/config.example.toml" 2>/dev/null || true
  ok "$CONFIG_DIR/config.example.toml (reference copy)"
  if [ ! -e "$CONFIG_DIR/config.toml" ]; then
    if cp "$SRC/config.example.toml" "$CONFIG_DIR/config.toml"; then
      chmod 644 "$CONFIG_DIR/config.toml" 2>/dev/null || true
      ok "$CONFIG_DIR/config.toml created from the example (it did not exist)"
    else
      printf '  note: could not create %s/config.toml\n' "$CONFIG_DIR"
    fi
  elif [ ! -s "$CONFIG_DIR/config.toml" ]; then
    if cp "$SRC/config.example.toml" "$CONFIG_DIR/config.toml"; then
      chmod 644 "$CONFIG_DIR/config.toml" 2>/dev/null || true
      ok "$CONFIG_DIR/config.toml was empty and is now the example (nothing was configured)"
    else
      printf '  note: %s/config.toml is empty and could not be replaced\n' "$CONFIG_DIR"
    fi
  else
    ok "$CONFIG_DIR/config.toml left untouched ($(wc -c < "$CONFIG_DIR/config.toml" | tr -d ' ') bytes of local configuration)"
  fi
fi

# ---------------------------------------------------------------- 5. restart
if [ "$RESTART" = "0" ]; then
  say "Skipped restart (--no-restart)"
  if [ "$FRESH" = "1" ]; then
    # A fresh install has no running service, so "it is still on the previous binary" would be a
    # lie, and leaving without the launch agent would leave the install incomplete. Write the agent,
    # leave it unloaded, and say exactly how to start it.
    if [ ! -f "$PLIST" ]; then
      say "Creating launch agent"
      write_plist
      PLIST_WRITTEN=1
      ok "$PLIST"
    fi
    # A loaded job means a process is already serving an older image (fresh detection also fires when
    # the binary or plist was removed while the label stayed up). `bootstrap` would fail on a
    # bootstrapped label, so the honest instruction differs by state.
    if job_loaded && [ "$PLIST_WRITTEN" = "1" ]; then
      # launchd does not re-read a plist on kickstart, so a rewritten agent needs a full reload - the
      # same bootout/bootstrap the restart path prescribes for this state.
      printf '  nothing was restarted. The launch agent was rewritten, and kickstart would keep the old\n'
      printf '  definition, so reload it:\n'
      printf '    launchctl bootout gui/%s/%s\n' "$(id -u)" "$LABEL"
      printf '    launchctl bootstrap gui/%s %s\n' "$(id -u)" "$PLIST"
    elif job_loaded; then
      printf '  nothing was restarted. The job is still running the previous binary; restart it with:\n'
      printf '    launchctl kickstart -k gui/%s/%s\n' "$(id -u)" "$LABEL"
    else
      printf '  nothing was started. Start it with:\n'
      printf '    launchctl bootstrap gui/%s %s\n' "$(id -u)" "$PLIST"
    fi
  else
    printf '  the service is still running the previous binary; restart with:\n'
    printf '    launchctl kickstart -k gui/%s/%s\n' "$(id -u)" "$LABEL"
  fi
  exit 0
fi

if [ "$FRESH" = "1" ]; then
  # "Fresh" covers three shapes and only two of them are stopped: the binary or the plist may have
  # been removed while the job kept serving. A job that is already running is therefore restarted
  # through the identity proof, not accepted as-is - otherwise a fresh run would install a new
  # binary, leave the old process in place, and still print success, which is the stale-image
  # failure the update path exists to catch.
  if [ ! -f "$PLIST" ]; then
    say "Creating launch agent"
    write_plist
    PLIST_WRITTEN=1
    ok "$PLIST"
  fi
  if job_loaded; then
    # The job is registered but its plist was just (re)written or was missing, and launchd does not
    # re-read a plist on kickstart - it would keep the previous ProgramArguments, environment and log
    # paths while the identity proof still passes. Boot out and bootstrap so the job runs what is on
    # disk now, then prove health.
    say "Restarting the running service"
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    wait_for_port_free || true
    reload_job
    if ! restart_and_probe; then
      printf '\n\033[1mThe install did not replace the running process.\033[0m\n' >&2
      printf '  repair with:\n    launchctl bootout gui/%s/%s; launchctl bootstrap gui/%s %s\n' \
        "$(id -u)" "$LABEL" "$(id -u)" "$PLIST" >&2
      if [ "${ROLLBACK_POINT_RECORDED:-0}" = "1" ] && [ -n "${REL:-}" ] && restorable "$(basename "$REL")"; then
        printf '  or restore the previous binary and database with: %s --rollback\n' "$SELF" >&2
      else
        printf '  this install saved no previous binary to restore; see: %s --list\n' "$SELF" >&2
      fi
      exit 1
    fi
  else
    say "Loading $LABEL"
    reload_job
    ok "loaded"
    say "Starting the service"
    if ! wait_for_new_job; then
      printf '\n\033[1mThe first install did not come up healthy.\033[0m\n' >&2
      printf '  see %s/underclass.stderr.log\n' "$DATA_DIR" >&2
      printf '  remove the launch agent with: launchctl bootout gui/%s/%s\n' "$(id -u)" "$LABEL" >&2
      exit 1
    fi
  fi
  if [ "$SANDBOX" = "1" ]; then
    # A sandbox is advertised as throwaway, and a bootstrapped job outlives this process. Proving
    # health needed a real running job, so it is stopped again here rather than left behind.
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    printf '  sandbox: agent booted out again; nothing is left running\n'
  fi
else
  # An update in a sandbox must not leave a daemon behind on a throwaway port, so the loaded-agent
  # check gates the restart rather than being skipped by a mode that advertises itself as throwaway.
  # `BOOTED_OUT` distinguishes "this run stopped it for a migration" from "it was never loaded",
  # so a sandbox run that migrated still gets its restart proof.
  if [ "$SANDBOX" = "1" ] && ! job_loaded && [ "${BOOTED_OUT:-0}" != "1" ]; then
    say "Skipped restart (sandbox agent is not loaded)"
    printf '  the sandbox binary and database are in place; nothing was started and nothing was left running\n'
    printf '  to exercise the update path too, load the agent first and rerun with SANDBOX_LOAD=1:\n'
    printf '    launchctl bootstrap gui/%s %s\n' "$(id -u)" "$PLIST"
    exit 0
  fi
  # A migration in step 3b left the job booted out, so the label is not registered and
  # `restart_and_probe` would fail on its first check. Load it first; this is the same reload the
  # rollback path performs for the same state.
  if ! job_loaded; then
    say "Loading $LABEL"
    reload_job
    ok "loaded"
  fi
  say "Restarting the service"
  if ! restart_and_probe; then
    printf '\n\033[1mThe new binary did not come up healthy.\033[0m\n' >&2
    if [ "$ROLLBACK_POINT_RECORDED" = "1" ]; then
      # INSTALLED_REV, not OLD_REV: they differ when an earlier run advanced the checkout without
      # installing, and the point restores the installed binary's revision.
      printf '  roll back to %s with:\n    %s --rollback\n' "${INSTALLED_REV:-$OLD_REV}" "$SELF" >&2
    else
      printf '  this run replaced nothing, so the newest restorable point is from an earlier deploy:\n' >&2
      printf '    %s --list\n    %s --rollback\n' "$SELF" "$SELF" >&2
    fi
    if [ "${MIGRATED:-0}" = "1" ]; then
      # A migration ran in this deploy, so the database may already carry rewritten identifiers.
      # Restoring only the binary would pair the old build with rewritten rows - the mismatch this
      # whole path exists to prevent - so `--keep-db` is deliberately not offered here.
      printf '\n  a data migration ran in this deploy, so the database may not match the previous binary.\n' >&2
      printf '  restore both with: %s --rollback   (do NOT add --keep-db)\n' "$SELF" >&2
    else
      printf '  keep the database with:\n    %s --rollback --keep-db\n' "$SELF" >&2
    fi
    exit 1
  fi
  # A sandbox daemon outlives this script and would keep serving on the sandbox port from a root
  # that may differ from the next run's. The proof needed a real running job, so it is stopped again
  # here - the same discipline the fresh path already applies.
  if [ "$SANDBOX" = "1" ]; then
    launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
    printf '  sandbox: agent booted out again; nothing is left running\n'
  fi
fi
ok "healthy on $BIND"

if [ "$FRESH" = "1" ]; then
  printf '\n\033[1mInstalled %s\033[0m\n' "$NEW_REV"
  printf '  source:   %s\n  binary:   %s\n' "$SRC" "$BIN_DST"
  printf '  agent:    %s\n' "$PLIST"
  printf '  update later with: %s\n' "$SELF"
  exit 0
fi

printf '\n\033[1mUpdated %s -> %s\033[0m\n' "$OLD_REV" "$NEW_REV"
printf '  source:   %s\n  binary:   %s\n' "$SRC" "$BIN_DST"
if [ "$ROLLBACK_POINT_RECORDED" = "1" ]; then
  printf '  rollback: %s --rollback   (restores %s)\n' "$SELF" "${INSTALLED_REV:-$OLD_REV}"
else
  printf '  rollback: nothing was replaced this run; %s --list shows the existing points\n' "$SELF"
fi
