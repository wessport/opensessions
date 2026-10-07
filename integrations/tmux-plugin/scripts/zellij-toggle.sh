#!/usr/bin/env sh
# Toggle opensessions sidebar in Zellij via the server.
# Ensures the server is running first, then calls POST /toggle.
#
# Zellij is not a supported mux: the server only manages tmux sidebars. This
# helper is kept so old keybindings fail gracefully instead of by construction.
# It resolves the server binary, port, PID file, and auth token exactly like
# the tmux scripts (server-common.sh). Outside tmux, set OPENSESSIONS_SERVER_KEY
# (or leave it unset for the default port and /tmp/opensessions.token).
#
# Designed to be called from a zellij keybinding. Add to ~/.config/zellij/config.kdl:
#
#   bind "s" {
#     Run "sh" "${OPENSESSIONS_DIR}/integrations/tmux-plugin/scripts/zellij-toggle.sh" {
#       close_on_exit true
#     };
#     SwitchToMode "Normal";
#   }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
. "$SCRIPT_DIR/server-common.sh"

ensure_server || exit 1

# --- Build context: |session|tabId ---
SESSION_NAME="${ZELLIJ_SESSION_NAME:-}"
# Get active tab ID from JSON (works from inside zellij)
TAB_ID="0"
TAB_JSON=$(zellij action list-tabs --json 2>/dev/null || echo "")
if [ -n "$TAB_JSON" ]; then
    TAB_ID=$(printf '%s' "$TAB_JSON" | python3 -c "
import json,sys
try:
    tabs=json.load(sys.stdin)
    active=[t for t in tabs if t.get('active')]
    print(active[0]['tab_id'] if active else tabs[0]['tab_id'] if tabs else '0')
except: print('0')" 2>/dev/null || echo "0")
fi

CTX="|${SESSION_NAME}|${TAB_ID}"
curl -s -o /dev/null -m 0.2 --connect-timeout 0.1 -H "Authorization: Bearer $(auth_token)" -X POST "http://${HOST}:${PORT}/toggle" -d "$CTX"
