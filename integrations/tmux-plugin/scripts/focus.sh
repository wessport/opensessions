#!/usr/bin/env sh

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

SIDEBAR_PANE_TITLE="opensessions-sidebar"

is_shell_command_pattern='^(zsh|bash|fish|sh|ksh|dash)$'

find_sidebar_pane() {
  tmux list-panes -t "$1" -F '#{pane_id}|#{pane_title}|#{pane_current_command}' 2>/dev/null |
    awk -F '|' -v title="$SIDEBAR_PANE_TITLE" -v shell_pattern="$is_shell_command_pattern" \
      '$2 == title && $3 !~ shell_pattern { print $1; exit }'
}

kill_stale_sidebar_panes() {
  tmux list-panes -t "$1" -F '#{pane_id}|#{pane_title}|#{pane_current_command}' 2>/dev/null |
    awk -F '|' -v title="$SIDEBAR_PANE_TITLE" -v shell_pattern="$is_shell_command_pattern" \
      '$2 == title && $3 ~ shell_pattern { print $1 }' |
    while IFS= read -r pane_id; do
      [ -n "$pane_id" ] || continue
      tmux kill-pane -t "$pane_id" >/dev/null 2>&1 || true
    done
}

WINDOW_ID="$(tmux display-message -p '#{window_id}' 2>/dev/null)"
[ -n "$WINDOW_ID" ] || exit 0

PANE_ID="$(find_sidebar_pane "$WINDOW_ID")"
if [ -n "$PANE_ID" ]; then
  tmux select-pane -t "$PANE_ID" >/dev/null 2>&1
  tmux switch-client -T root >/dev/null 2>&1
  exit 0
fi

kill_stale_sidebar_panes "$WINDOW_ID"

ensure_server || exit 0

CTX="$(tmux display-message -p '#{client_tty}|#{session_name}|#{window_id}' 2>/dev/null)"

auth_post "/ensure-sidebar?reveal=1" -d "$CTX"

attempt=0
while [ "$attempt" -lt 20 ]; do
  PANE_ID="$(find_sidebar_pane "$WINDOW_ID")"
  if [ -n "$PANE_ID" ]; then
    tmux select-pane -t "$PANE_ID" >/dev/null 2>&1
    tmux switch-client -T root >/dev/null 2>&1
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 0.05
done

tmux switch-client -T root >/dev/null 2>&1
