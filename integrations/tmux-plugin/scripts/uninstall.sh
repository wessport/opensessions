#!/usr/bin/env sh
# opensessions uninstall — clean up all tmux hooks, keybindings, sidebar panes, and env vars
# Run this BEFORE removing the plugin files.
#
# Usage:
#   sh /path/to/opensessions/integrations/tmux-plugin/scripts/uninstall.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

echo "opensessions: uninstalling..."

# --- Stop the server ---
# A running server removes its own hooks, bindings, and sidebars on /quit.
curl -s -o /dev/null -m 2 -H "Authorization: Bearer $(auth_token)" -X POST "http://${HOST}:${PORT}/quit" 2>/dev/null || true
echo "  ✓ stopped server (if running)"

# --- Remove global hooks ---
# The server installs every hook in its own array slot (OPENSESSIONS_HOOK_INDEX
# in packages/runtime-rs/src/tmux_provider.rs) so user hooks in other slots
# survive. Unset only that slot, for every hook the server installs
# (TmuxProvider::cleanup_hooks), in case the server was not running.
OPENSESSIONS_HOOK_INDEX=909
for hook in \
  client-session-changed \
  after-select-pane \
  session-created \
  after-new-session \
  session-closed \
  after-select-window \
  after-new-window \
  client-resized \
  after-kill-pane \
  pane-exited \
  pane-died \
  after-resize-pane \
  after-resize-window; do
  tmux set-hook -gu "${hook}[${OPENSESSIONS_HOOK_INDEX}]" 2>/dev/null || true
done
echo "  ✓ removed opensessions global hooks"

# --- Kill sidebar panes ---
# Find all panes titled "opensessions-sidebar" and kill them
sidebar_panes=$(tmux list-panes -a -F '#{pane_id} #{pane_title}' 2>/dev/null | awk '$2 == "opensessions-sidebar" { print $1 }') || true
if [ -n "$sidebar_panes" ]; then
  for pane in $sidebar_panes; do
    tmux kill-pane -t "$pane" 2>/dev/null || true
  done
  echo "  ✓ killed sidebar panes"
fi

# --- Kill stash session ---
tmux kill-session -t "_os_stash" 2>/dev/null || true
echo "  ✓ removed stash session"

# --- Remove keybindings ---
# Command table bindings (opensessions key table)
PREFIX_KEY=$(tmux show-option -gqv "@opensessions-prefix-key" 2>/dev/null)
PREFIX_KEY="${PREFIX_KEY:-o}"
tmux unbind-key "$PREFIX_KEY" 2>/dev/null || true

# Unbind all keys in the opensessions command table
tmux unbind-key -T opensessions s 2>/dev/null || true
tmux unbind-key -T opensessions t 2>/dev/null || true
tmux unbind-key -T opensessions e 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key -T opensessions "$i" 2>/dev/null || true
done
tmux unbind-key -T opensessions Any 2>/dev/null || true

# Direct prefix bindings
tmux unbind-key C-s 2>/dev/null || true
tmux unbind-key C-t 2>/dev/null || true
for i in 1 2 3 4 5 6 7 8 9; do
  tmux unbind-key "M-$i" 2>/dev/null || true
done

# Global keys (if configured)
FOCUS_GLOBAL_KEY=$(tmux show-option -gqv "@opensessions-focus-global-key" 2>/dev/null)
if [ -n "$FOCUS_GLOBAL_KEY" ]; then
  tmux unbind-key -n "$FOCUS_GLOBAL_KEY" 2>/dev/null || true
fi
INDEX_KEYS=$(tmux show-option -gqv "@opensessions-index-keys" 2>/dev/null)
for key in $INDEX_KEYS; do
  tmux unbind-key -n "$key" 2>/dev/null || true
done
echo "  ✓ removed keybindings"

# --- Remove recorded sidebar visibility ---
tmux set-option -gu @opensessions_sidebar_visible 2>/dev/null || true
tmux set-option -gu @opensessions_width 2>/dev/null || true

# --- Remove environment variables ---
tmux set-environment -gu OPENSESSIONS_DIR 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_WIDTH 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_HOST 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_PORT 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_PID_FILE 2>/dev/null || true
tmux set-environment -gu OPENSESSIONS_TOKEN_FILE 2>/dev/null || true
echo "  ✓ removed environment variables"

echo "opensessions: uninstall complete. You can now remove the plugin files."
echo "  If using TPM: remove the line from .tmux.conf and run prefix + alt + u"
