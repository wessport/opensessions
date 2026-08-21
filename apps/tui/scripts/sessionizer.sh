#!/usr/bin/env bash
# opensessions sessionizer — fuzzy directory picker for new tmux sessions
# Requires: fzf, find
# Supports colon-separated paths in SESSIONIZER_DIR (e.g. "$HOME/Code:$HOME/.config")
# Search depth is configurable via SESSIONIZER_MAXDEPTH (default: 3)

# Check env first, then tmux global environment, then default
if [ -z "$SESSIONIZER_DIR" ] && command -v tmux &>/dev/null; then
  SESSIONIZER_DIR=$(tmux show-environment -g SESSIONIZER_DIR 2>/dev/null | sed 's/^SESSIONIZER_DIR=//')
fi
SEARCH_DIRS="${SESSIONIZER_DIR:-$HOME/Documents}"

if [ -z "$SESSIONIZER_MAXDEPTH" ] && command -v tmux &>/dev/null; then
  SESSIONIZER_MAXDEPTH=$(tmux show-environment -g SESSIONIZER_MAXDEPTH 2>/dev/null | sed 's/^SESSIONIZER_MAXDEPTH=//')
fi
MAXDEPTH="${SESSIONIZER_MAXDEPTH:-3}"
[[ "$MAXDEPTH" =~ ^[1-9][0-9]*$ ]] || MAXDEPTH=3

if ! command -v fzf &>/dev/null; then
  echo "fzf is required for the sessionizer. Install it: https://github.com/junegunn/fzf"
  exit 1
fi

# Split colon-separated paths and validate each one
IFS=: read -ra dirs <<<"$SEARCH_DIRS"
valid_dirs=()
for dir in "${dirs[@]}"; do
  [ -d "$dir" ] && valid_dirs+=("$dir")
done

if [ ${#valid_dirs[@]} -eq 0 ]; then
  echo "No valid directories found in: $SEARCH_DIRS"
  exit 1
fi

fzf_output=$(find "${valid_dirs[@]}" -mindepth 1 -maxdepth "$MAXDEPTH" -type d -not -path '*/.*' 2>/dev/null | fzf \
  --reverse \
  --print-query \
  --header="Pick a directory (or type a path and press Enter)" \
  --preview=':' \
  --preview-window=hidden \
  --bind='ctrl-c:abort')

# --print-query outputs line 1 as the query and line 2 as the selected match.
query=$(printf '%s\n' "$fzf_output" | sed -n '1p')
match=$(printf '%s\n' "$fzf_output" | sed -n '2p')

# Bash does not expand a quoted tilde. Expand the two useful home-directory
# forms explicitly before validating a typed path.
if [ "$query" = "~" ]; then
  query="$HOME"
elif [[ "$query" == "~/"* ]]; then
  query="$HOME/${query#\~/}"
fi

# fzf can return a highlighted match even when the user entered an explicit
# directory, so prefer a non-empty, valid typed path.
if [ -n "$query" ] && [ -d "$query" ]; then
  selected="$query"
elif [ -n "$match" ]; then
  selected="$match"
else
  exit 0
fi

# Offer the directory basename as a default while allowing the new session to
# have a more descriptive name. An empty response preserves the quick path for
# users who are happy with the derived name.
default_session_name=$(basename "$selected" | tr '.' '_')
printf '\nSession name [%s]: ' "$default_session_name"
IFS= read -r entered_session_name || entered_session_name=""
session_name="${entered_session_name:-$default_session_name}"
session_name=$(printf '%s' "$session_name" | tr '.:' '__')

# If session already exists, just switch to it
if tmux has-session -t "=$session_name" 2>/dev/null; then
  tmux switch-client -t "$session_name"
  exit 0
fi

tmux new-session -d -s "$session_name" -c "$selected"
tmux switch-client -t "$session_name"
