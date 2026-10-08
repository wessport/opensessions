#!/usr/bin/env sh

trim_space() {
  value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

# First 16 hex characters of the SHA-256 of the argument's bytes.
sha256_prefix16() {
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s' "$1" | sha256sum | cut -c1-16
  elif command -v shasum >/dev/null 2>&1; then
    printf '%s' "$1" | shasum -a 256 | cut -c1-16
  else
    printf '%s' "$1" | openssl dgst -sha256 | awk '{print substr($NF, 1, 16)}'
  fi
}

server_key() {
  explicit_key="$(trim_space "${OPENSESSIONS_SERVER_KEY:-}")"
  if [ -n "$explicit_key" ]; then
    printf '%s\n' "$explicit_key"
    return
  fi

  if [ -z "${TMUX:-}" ]; then
    return
  fi

  socket_path="${TMUX%%,*}"
  [ -n "$socket_path" ] || return

  if command -v realpath >/dev/null 2>&1 && canonical_path="$(realpath "$socket_path" 2>/dev/null)"; then
    socket_path="$canonical_path"
  else
    socket_dir="$(dirname "$socket_path")"
    if canonical_dir="$(cd "$socket_dir" 2>/dev/null && pwd -P)"; then
      socket_path="$canonical_dir/$(basename "$socket_path")"
    fi
  fi

  sha256_prefix16 "$socket_path"
}

# Server key -> port offset (0..19999). Keep identical to
# resolve_server_port_with_base in packages/runtime-rs/src/shared.rs and
# portForServerKey in integrations/amp and integrations/pi-extension:
#   1-15 ASCII digits     -> decimal value          (legacy numeric keys)
#   only hex digits       -> first 8 hex digits     (socket-derived SHA keys)
#   anything else         -> first 8 hex digits of SHA-256(key)
server_port_offset() {
  key="$1"
  case "$key" in
    *[!0-9]*) ;;
    *)
      if [ "${#key}" -lt 16 ]; then
        awk -v key="$key" 'BEGIN { printf "%d\n", (key + 0) % 20000 }'
        return
      fi
      ;;
  esac
  case "$key" in
    *[!0-9a-fA-F]*) key="$(sha256_prefix16 "$key")" ;;
  esac
  key="$(printf '%s' "$key" | cut -c1-8)"
  printf '%s\n' "$(( 0x$key % 20000 ))"
}

# Print the value of a tmux global environment variable. `show-environment`
# prints NAME=value for set variables and -NAME for variables marked for
# removal; values may themselves contain '='.
tmux_global_env() {
  env_line="$(tmux show-environment -g "$1" 2>/dev/null)" || return 0
  case "$env_line" in
    "$1="*) printf '%s\n' "${env_line#*=}" ;;
  esac
}

SERVER_KEY="$(server_key)"
PORT_BASE=22000
TMUX_OPENSESSIONS_PORT="$(tmux_global_env OPENSESSIONS_PORT)"
TMUX_OPENSESSIONS_HOST="$(tmux_global_env OPENSESSIONS_HOST)"
TMUX_OPENSESSIONS_PID_FILE="$(tmux_global_env OPENSESSIONS_PID_FILE)"
TMUX_OPENSESSIONS_TOKEN_FILE="$(tmux_global_env OPENSESSIONS_TOKEN_FILE)"

if [ -n "$TMUX_OPENSESSIONS_PORT" ]; then
  PORT="$TMUX_OPENSESSIONS_PORT"
elif [ -n "$SERVER_KEY" ]; then
  PORT=$((PORT_BASE + $(server_port_offset "$SERVER_KEY")))
else
  PORT="7391"
fi
if [ -n "$TMUX_OPENSESSIONS_TOKEN_FILE" ]; then
  TOKEN_FILE="$TMUX_OPENSESSIONS_TOKEN_FILE"
elif [ -n "$SERVER_KEY" ]; then
  TOKEN_FILE="/tmp/opensessions.${SERVER_KEY}.token"
else
  TOKEN_FILE="/tmp/opensessions.token"
fi
HOST="${TMUX_OPENSESSIONS_HOST:-127.0.0.1}"
if [ -n "$TMUX_OPENSESSIONS_PID_FILE" ]; then
  PID_FILE="$TMUX_OPENSESSIONS_PID_FILE"
elif [ -n "$SERVER_KEY" ]; then
  PID_FILE="/tmp/opensessions.${SERVER_KEY}.pid"
else
  PID_FILE="/tmp/opensessions.pid"
fi

PLUGIN_DIR="$(tmux_global_env OPENSESSIONS_DIR)"
PLUGIN_DIR="${PLUGIN_DIR:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
SERVER_LOG="/tmp/opensessions.${SERVER_KEY:-default}.server.log"
START_LOCK="/tmp/opensessions.${SERVER_KEY:-default}.start.lock"

RUST_SERVER_BIN=""
if [ -x "$PLUGIN_DIR/bin/opensessions-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/bin/opensessions-server"
elif [ -x "$PLUGIN_DIR/target/release/opensessions-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/target/release/opensessions-server"
elif [ -x "$PLUGIN_DIR/target/debug/opensessions-server" ]; then
  RUST_SERVER_BIN="$PLUGIN_DIR/target/debug/opensessions-server"
fi

show_startup_error() {
  message="$1"
  tmux display-message "$message" >/dev/null 2>&1 || true
  printf '%s\n' "$message" >&2
}

server_alive() {
  expected_identity="opensessions server"
  [ -z "$SERVER_KEY" ] || expected_identity="$expected_identity $SERVER_KEY"
  [ "$(curl -s -m 0.2 "http://${HOST}:${PORT}/" 2>/dev/null)" = "$expected_identity" ]
}

auth_token() {
  cat "$TOKEN_FILE" 2>/dev/null
}

# A cold start answers `server_alive` only after the server has installed its
# hooks and restored recorded sidebars (it reports "initializing" until then),
# which can take several seconds on a busy machine. Launchers and lock waiters
# wait this long while the start is still making progress, instead of giving
# up after a fixed number of polls.
START_TIMEOUT="${OPENSESSIONS_START_TIMEOUT:-30}"

# `date +%s` has whole-second resolution, so `now + N` can expire after as
# little as N-1 seconds. Add one second so every wait lasts at least
# START_TIMEOUT.
start_deadline() {
  printf '%s\n' "$(( $(date +%s) + START_TIMEOUT + 1 ))"
}

# The start lock is a file holding its owner's pid, process start time, and a
# nonce. It is published with `ln`, which atomically refuses an existing
# target, so a lock never exists without its owner recorded. A lock is stale
# when its pid is dead, when the pid now belongs to a process started at a
# different time (pid reuse), or when it names no pid (an older version's lock
# directory left by a launcher that died before writing its pid) for longer
# than START_LOCK_GRACE seconds.
START_LOCK_GRACE="${OPENSESSIONS_START_LOCK_GRACE:-2}"
START_LOCK_TOKEN=""

process_start_time() {
  LC_ALL=C ps -o lstart= -p "$1" 2>/dev/null | awk '{ $1 = $1; print }'
}

start_lock_nonce() {
  nonce="$(od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n')"
  printf '%s\n' "${nonce:-$(date +%s)}"
}

# Prints the lock's owner record, or nothing when it is absent. An older
# version's lock directory is read through its pid file.
read_start_lock() {
  if [ -d "$START_LOCK" ]; then
    printf 'pid=%s\n' "$(cat "$START_LOCK/pid" 2>/dev/null)"
  else
    cat "$START_LOCK" 2>/dev/null
  fi
}

# Status 0 when the record's owner is still running, 1 when it is dead or its
# pid was reused, 2 when the record names no pid.
start_lock_owner_state() {
  owner_pid="$(printf '%s\n' "$1" | sed -n 's/^pid=//p')"
  case "$owner_pid" in
    '' | *[!0-9]*) return 2 ;;
  esac
  kill -0 "$owner_pid" 2>/dev/null || return 1
  owner_start="$(printf '%s\n' "$1" | sed -n 's/^start=//p')"
  [ -n "$owner_start" ] || return 0
  current_start="$(process_start_time "$owner_pid")"
  [ -z "$current_start" ] || [ "$current_start" = "$owner_start" ]
}

# Removes the lock only while it still holds the stale record $1. Waiters that
# judged the same record stale serialize on a breaker directory and re-read
# the lock inside it, so a lock re-acquired by a faster waiter is never
# removed. The lock is renamed aside before deletion so removal is atomic.
# Returns 0 when this call removed the stale lock.
break_stale_start_lock() {
  breaker="$START_LOCK.break"
  if ! mkdir "$breaker" 2>/dev/null; then
    # A breaker that died inside its few-command critical section.
    now="$(date +%s)"
    if [ -z "$breaker_seen_at" ]; then
      breaker_seen_at="$now"
    elif [ $((now - breaker_seen_at)) -ge "$START_LOCK_GRACE" ]; then
      rmdir "$breaker" 2>/dev/null
      breaker_seen_at=""
    fi
    return 1
  fi
  breaker_seen_at=""
  broke=1
  if [ "$(read_start_lock)" = "$1" ]; then
    aside="$START_LOCK.stale.$$.$(start_lock_nonce)"
    if mv "$START_LOCK" "$aside" 2>/dev/null; then
      if [ -f "$aside" ] && [ "$(cat "$aside" 2>/dev/null)" != "$1" ]; then
        # Lost a race after all: hand the live lock back.
        ln "$aside" "$START_LOCK" 2>/dev/null
      else
        broke=0
      fi
      rm -rf "$aside"
    fi
  fi
  rmdir "$breaker" 2>/dev/null
  return "$broke"
}

# Returns 0 with the lock held, 1 on failure, or 2 when a server became
# reachable while waiting. Sets saw_live_launcher=1 when it had to wait on a
# live launcher, and SERVER_START_OBSERVED=1 when that launcher's server came
# up while waiting (a fresh start by someone else).
acquire_start_lock() {
  deadline=$(start_deadline)
  saw_live_launcher=0
  breaker_seen_at=""
  pidless_record=""
  pidless_seen_at=""
  live_record=""
  live_pid=""
  START_LOCK_TOKEN="$(printf 'pid=%s\nstart=%s\nnonce=%s' "$$" "$(process_start_time "$$")" "$(start_lock_nonce)")"
  candidate="$START_LOCK.$$.$(start_lock_nonce)"
  if ! printf '%s\n' "$START_LOCK_TOKEN" >"$candidate" 2>/dev/null; then
    show_startup_error "opensessions: cannot create server start lock $START_LOCK"
    return 1
  fi

  while [ -d "$START_LOCK" ] || ! ln "$candidate" "$START_LOCK" 2>/dev/null; do
    if server_alive; then
      rm -f "$candidate"
      [ "$saw_live_launcher" -eq 0 ] || SERVER_START_OBSERVED=1
      return 2
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      rm -f "$candidate"
      show_startup_error "opensessions: server start lock timed out. Remove $START_LOCK if no launcher is active."
      return 1
    fi

    record="$(read_start_lock)"
    if [ -n "$record" ] || [ -e "$START_LOCK" ]; then
      # Re-verify a known-live owner cheaply; only a new record needs `ps`.
      if [ "$record" = "$live_record" ] && kill -0 "$live_pid" 2>/dev/null; then
        owner_state=0
      else
        start_lock_owner_state "$record"
        owner_state=$?
      fi
      case "$owner_state" in
        0)
          live_record="$record"
          live_pid="$owner_pid"
          saw_live_launcher=1
          ;;
        1)
          break_stale_start_lock "$record" && continue
          ;;
        *)
          now="$(date +%s)"
          if [ -z "$pidless_seen_at" ] || [ "$record" != "$pidless_record" ]; then
            pidless_record="$record"
            pidless_seen_at="$now"
          elif [ $((now - pidless_seen_at)) -ge "$START_LOCK_GRACE" ]; then
            pidless_seen_at=""
            break_stale_start_lock "$record" && continue
          fi
          ;;
      esac
    fi
    sleep 0.1
  done

  rm -f "$candidate"
  return 0
}

release_start_lock() {
  if [ "$(cat "$START_LOCK" 2>/dev/null)" = "$START_LOCK_TOKEN" ]; then
    rm -f "$START_LOCK"
  fi
}

# Set to 1 when this ensure_server call launched the server generation that is
# now answering. A fresh server restores the user's last recorded sidebar
# visibility during startup.
SERVER_STARTED=0
# Set to 1 when this call waited on another launcher's in-progress start and
# that start brought the server up, so it has just restored sidebars too.
SERVER_START_OBSERVED=0

# True when the answering server is the process this invocation launched. The
# server publishes its pid file only after binding the port.
launched_server_answering() {
  kill -0 "$1" 2>/dev/null && [ "$(cat "$PID_FILE" 2>/dev/null)" = "$1" ] && server_alive
}

# True when a fresh server generation, started by this invocation or by the
# launcher it waited on, is now serving.
server_freshly_started() {
  [ "$SERVER_STARTED" = 1 ] || [ "$SERVER_START_OBSERVED" = 1 ]
}

ensure_server() {
  unset OPENSESSIONS_WIDTH

  if server_alive; then
    return 0
  fi

  acquire_start_lock
  lock_status=$?
  if [ "$lock_status" -eq 2 ]; then
    return 0
  fi
  if [ "$lock_status" -ne 0 ]; then
    return 1
  fi

  if server_alive; then
    # The launcher we waited on released the lock after its server came up.
    [ "$saw_live_launcher" -eq 0 ] || SERVER_START_OBSERVED=1
    release_start_lock
    return 0
  fi

  if [ -z "$RUST_SERVER_BIN" ]; then
    show_startup_error "opensessions: server binary not found. Reinstall/update opensessions, or build locally with: cd $PLUGIN_DIR && cargo build --release -p opensessions-server"
    release_start_lock
    return 1
  fi

  OPENSESSIONS_SERVER_KEY="$SERVER_KEY" \
  OPENSESSIONS_HOST="$HOST" \
  OPENSESSIONS_PORT="$PORT" \
  OPENSESSIONS_PID_FILE="$PID_FILE" \
  OPENSESSIONS_TOKEN_FILE="$TOKEN_FILE" \
  OPENSESSIONS_DIR="$PLUGIN_DIR" \
    "$RUST_SERVER_BIN" >"$SERVER_LOG" 2>&1 &
  server_pid=$!

  # Keep waiting while the launched server is still running; stop early only
  # when it exits (for example, another server already owns the port).
  deadline=$(start_deadline)
  while kill -0 "$server_pid" 2>/dev/null && [ "$(date +%s)" -lt "$deadline" ]; do
    sleep 0.1
    if launched_server_answering "$server_pid"; then
      SERVER_STARTED=1
      release_start_lock
      return 0
    fi
  done

  # Our server exited (or is still starting at the deadline) but an endpoint
  # answers: a server was already running, e.g. one too slow for the 200ms
  # liveness probe. Use it, but it did not just restore sidebars, so leave
  # SERVER_STARTED=0. Never surface an "address already in use" startup failure
  # when the server is actually up.
  if launched_server_answering "$server_pid"; then
    SERVER_STARTED=1
    release_start_lock
    return 0
  fi
  if server_alive; then
    release_start_lock
    return 0
  fi

  show_startup_error "opensessions: server failed to start. See $SERVER_LOG"
  release_start_lock
  return 1
}
