#!/usr/bin/env sh
# Spread non-sidebar panes in the current tmux window using even-horizontal,
# while preserving the opensessions sidebar pane if present.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/sidebar-common.sh"
. "$SCRIPT_DIR/even-horizontal-common.sh"

pane_rows_for_window() {
  window_id="$1"
  tmux list-panes -t "$window_id" -F "#{pane_id}${PANE_FIELD_SEP}#{pane_title}${PANE_FIELD_SEP}#{pane_width}${PANE_FIELD_SEP}#{pane_left}${PANE_FIELD_SEP}#{pane_right}${PANE_FIELD_SEP}#{pane_active}" 2>/dev/null
}

active_pane_in_window() {
  tmux list-panes -t "$1" -F "#{pane_id}${PANE_FIELD_SEP}#{pane_active}" 2>/dev/null | awk -F "$PANE_FIELD_SEP" '$2 == "1" { print $1; exit }'
}

restore_focus() {
  preferred_pane_id="$1"
  sidebar_pane_id="$2"
  sidebar_active="$3"

  if [ "$sidebar_active" = "1" ] && [ -n "$sidebar_pane_id" ]; then
    tmux select-pane -t "$sidebar_pane_id" >/dev/null 2>&1 || true
    return 0
  fi

  if [ -n "$preferred_pane_id" ]; then
    tmux select-pane -t "$preferred_pane_id" >/dev/null 2>&1 || true
  fi
}

pane_session() {
  tmux display-message -p -t "$1" '#{session_name}' 2>/dev/null || true
}

# Put a stashed sidebar back into the window. If the full-height join at its
# edge fails, retry a plain join next to the edge pane; as a last resort kill
# the stashed pane and ask the server for a fresh sidebar so it is never
# stranded in the stash.
unstash_sidebar_pane() {
  restore_sidebar_pane "$SIDEBAR_PANE_ID" "$WINDOW_ID" "$SIDEBAR_SIDE" "$SIDEBAR_WIDTH" && return 0
  [ "$(pane_session "$SIDEBAR_PANE_ID")" = "$STASH_SESSION" ] || return 0

  fallback_pane_id="$(window_edge_pane "$WINDOW_ID" "$SIDEBAR_SIDE")"
  fallback_flag="-h"
  [ "$SIDEBAR_SIDE" != "left" ] || fallback_flag="-hb"
  if [ -n "$fallback_pane_id" ] &&
    tmux join-pane "$fallback_flag" -d -l "$SIDEBAR_WIDTH" -s "$SIDEBAR_PANE_ID" -t "$fallback_pane_id" >/dev/null 2>&1; then
    return 0
  fi

  tmux kill-pane -t "$SIDEBAR_PANE_ID" >/dev/null 2>&1 || true
  sh "$SCRIPT_DIR/ensure-sidebar.sh" >/dev/null 2>&1 || true
  return 1
}

WINDOW_ID="${1:-$(current_window_id)}"
[ -n "$WINDOW_ID" ] || exit 0

CURRENT_PANE_ID="${2:-$(current_pane_id)}"
CURRENT_PANE_ID="${CURRENT_PANE_ID:-$(active_pane_in_window "$WINDOW_ID")}"

PANE_ROWS="$(pane_rows_for_window "$WINDOW_ID")"
[ -n "$PANE_ROWS" ] || exit 0

SIDEBAR_COUNT="$(count_sidebar_panes "$PANE_ROWS" "$SIDEBAR_PANE_TITLE")"
NON_SIDEBAR_COUNT="$(count_non_sidebar_panes "$PANE_ROWS" "$SIDEBAR_PANE_TITLE")"

# Ambiguous sidebar state: do nothing rather than guess.
if [ "$SIDEBAR_COUNT" -gt 1 ]; then
  tmux switch-client -T root >/dev/null 2>&1 || true
  exit 0
fi

# Need at least two non-sidebar panes for even-horizontal to have an effect.
if [ "$NON_SIDEBAR_COUNT" -lt 2 ]; then
  tmux switch-client -T root >/dev/null 2>&1 || true
  exit 0
fi

if [ "$SIDEBAR_COUNT" -eq 0 ]; then
  tmux select-layout -t "$WINDOW_ID" even-horizontal >/dev/null 2>&1 || exit 0
  restore_focus "$CURRENT_PANE_ID" "" "0"
  tmux switch-client -T root >/dev/null 2>&1 || true
  exit 0
fi

SIDEBAR_INFO="$(extract_sidebar_info "$PANE_ROWS" "$SIDEBAR_PANE_TITLE")"
SIDEBAR_PANE_ID="$(printf '%s' "$SIDEBAR_INFO" | awk -F "$PANE_FIELD_SEP" '{ print $1 }')"
SIDEBAR_WIDTH="$(printf '%s' "$SIDEBAR_INFO" | awk -F "$PANE_FIELD_SEP" '{ print $2 }')"
SIDEBAR_LEFT="$(printf '%s' "$SIDEBAR_INFO" | awk -F "$PANE_FIELD_SEP" '{ print $3 }')"
SIDEBAR_RIGHT="$(printf '%s' "$SIDEBAR_INFO" | awk -F "$PANE_FIELD_SEP" '{ print $4 }')"
SIDEBAR_ACTIVE="$(printf '%s' "$SIDEBAR_INFO" | awk -F "$PANE_FIELD_SEP" '{ print $5 }')"
SIDEBAR_SIDE="$(detect_sidebar_side "$PANE_ROWS" "$SIDEBAR_LEFT" "$SIDEBAR_RIGHT" "$SIDEBAR_PANE_TITLE")"

[ -n "$SIDEBAR_PANE_ID" ] || exit 0
[ -n "$SIDEBAR_WIDTH" ] || exit 0
[ -n "$SIDEBAR_SIDE" ] || exit 0

# Move the sidebar out of the window so even-horizontal only spreads the other
# panes. If stashing fails the sidebar is still in place; leave the layout as is.
if ! stash_sidebar_pane "$SIDEBAR_PANE_ID"; then
  tmux switch-client -T root >/dev/null 2>&1 || true
  exit 0
fi

# From here on the sidebar must always be restored, even if the layout fails.
tmux select-layout -t "$WINDOW_ID" even-horizontal >/dev/null 2>&1 || true
if unstash_sidebar_pane; then
  restore_focus "$CURRENT_PANE_ID" "$SIDEBAR_PANE_ID" "$SIDEBAR_ACTIVE"
else
  restore_focus "$CURRENT_PANE_ID" "" "0"
fi

tmux switch-client -T root >/dev/null 2>&1 || true
