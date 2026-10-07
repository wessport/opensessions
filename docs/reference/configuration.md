# Configuration Reference

This page documents the configuration inputs that opensessions reads today.

## Config File Location

User config is loaded from:

```text
~/.config/opensessions/config.json
```

If the file does not exist, opensessions falls back to defaults.

## Recommended Config Shape

```json
{
  "mux": "tmux",
  "theme": "tokyo-night",
  "sidebarWidth": 30,
  "sidebarPosition": "right"
}
```

## Config Fields

| Field | Type | Default | Runtime status | Description |
| --- | --- | --- | --- | --- |
| `mux` | `string` | auto-detect | active | Selects the preferred registered mux provider by name |
| `plugins` | `string[]` | `[]` | parsed only | Compatibility field from the old TypeScript plugin loader; the Rust server does not execute plugin packages today |
| `theme` | `string` | `catppuccin-mocha` | active | Built-in theme name persisted by the TUI |
| `sidebarWidth` | `number` | `26` | active | Sidebar width in columns |
| `sidebarPosition` | `"left" | "right"` | `"left"` | active | Sidebar placement |
| `port` | `number` | none | parsed only | Present in the config type; use `OPENSESSIONS_PORT`/tmux-scoped environment for runtime port overrides today |
| `keybinding` | `string` | none | parsed only | Present in the config type, but keybindings are configured outside this file today |
| `autoHibernate` | `{ "enabled"?: boolean, "idleAfterMs"?: number }` | `{ "enabled": true, "idleAfterMs": 21600000 }` | active | Stops idle agent processes to free memory; see below |

## Auto-Hibernate

Idle CLI agents keep hundreds of megabytes resident for as long as their tmux pane stays open. Every 5 minutes the server looks for live agents that have been `idle`, `done`, `error`, `interrupted`, or `stale` for longer than `idleAfterMs` (default 6 hours) and stops only the agent process inside that pane. It sends `SIGTERM`, then `SIGKILL` after one second if the agent ignores `SIGTERM` (Amp does). The pane, its shell, and the tmux session are never killed, and only agent-named descendants of the pane's own process are signalled.

Running, tool-running, and waiting agents are never hibernated, nor are agents in the current session or in a pane that a tmux client is showing. The agent row stays in the sidebar as `hibernated` (`◌`) so the thread can be resumed later, for example with `amp threads continue <thread-id>`.

Built-in hibernation recognizes Amp, Claude Code, Codex, OpenCode, Pi, and Droid processes. Hibernation state is in memory, so it is lost when the server restarts.

To disable auto-hibernation or change the threshold:

```json
{
  "autoHibernate": { "enabled": false }
}
```

```json
{
  "autoHibernate": { "idleAfterMs": 43200000 }
}
```

## Built-In Themes

These theme names resolve in the running app today:

- `catppuccin-mocha`
- `catppuccin-latte`
- `catppuccin-frappe`
- `catppuccin-macchiato`
- `tokyo-night`
- `gruvbox-dark`
- `nord`
- `dracula`
- `github-dark`
- `one-dark`
- `kanagawa`
- `everforest`
- `material`
- `cobalt2`
- `flexoki`
- `ayu`
- `aura`
- `matrix`

## Inline Theme Objects

The core config type and theme resolver also support partial inline theme objects such as:

```json
{
  "theme": {
    "palette": {
      "base": "#000000",
      "text": "#ffffff"
    }
  }
}
```

That shape is valid for the core APIs, but the current server startup path only applies string theme names end-to-end.

## tmux Plugin Options

The tmux integration reads these tmux options instead of `config.json`:

| tmux option | Default | Used by |
| --- | --- | --- |
| `@opensessions-prefix-key` | `o` | Key after the tmux prefix that enters the opensessions key table (`prefix <key>`) |
| `@opensessions-focus-global-key` | unset | Optional no-prefix tmux keybinding that reveals and focuses the sidebar pane |
| `@opensessions-index-keys` | unset | Optional space-separated no-prefix tmux keys mapped in order to visible sessions `1` through `9` |
| `@opensessions-width` | deprecated | Use `sidebarWidth` in config or the in-sidebar width slider instead |

The plugin registers these prefix bindings automatically:

| Binding | Action |
| --- | --- |
| `prefix o → s` | Reveal and focus the sidebar |
| `prefix o → t` | Toggle the sidebar |
| `prefix o → e` | Spread non-sidebar panes in the current window using `even-horizontal` |
| `prefix o → 1` through `prefix o → 9` | Switch to visible session by index |

Minimal install:

If you use TPM, this is enough:

```tmux
set -g @plugin 'Ataraxy-Labs/opensessions'
```

After adding it, reload tmux and ask TPM to install plugins:

```bash
tmux source-file ~/.tmux.conf
~/.tmux/plugins/tpm/bin/install_plugins
```

On first load, the plugin downloads the matching GitHub release bundle into `~/.tmux/plugins/opensessions/bin/`. The bundle contains `opensessions-sidebar`, `opensessions-server`, and the bundled `lazydiff` binary. You do not need Rust/Cargo for the normal TPM install path.

If you run from a local checkout instead, this is enough:

```tmux
source-file /absolute/path/to/opensessions/opensessions.tmux
```

Optional overrides:

```tmux
set -g @opensessions-prefix-key "o"   # default; change to remap the opensessions key table
```

All other tmux options fall back to the defaults shown in the table above.

- Use `@opensessions-focus-global-key` and `@opensessions-index-keys` only when you explicitly want no-prefix tmux bindings and know they do not conflict with your window manager or terminal.

## Environment Variables

| Variable | Used by | Notes |
| --- | --- | --- |
| `OPENCODE_DB_PATH` | OpenCode watcher | Overrides the default SQLite path |
| `OPENSESSIONS_DIR` | tmux helper scripts and server | Helps helper scripts find the repo checkout |
| `OPENSESSIONS_HOST` | server, sidebar, helper shell scripts | Runtime host override; normally `127.0.0.1` |
| `OPENSESSIONS_PORT` | server, sidebar, helper shell scripts | Runtime port override; normally derived from the tmux socket/server key |
| `OPENSESSIONS_SERVER_KEY` | server, sidebar, helper shell scripts, Amp/Pi integrations | Explicit server key replacing the tmux-socket-derived key; selects the port, PID file, and token file. See [Server key and port](#server-key-and-port) |
| `OPENSESSIONS_PID_FILE` | server, helper shell scripts | PID file override; default `/tmp/opensessions.<key>.pid` |
| `OPENSESSIONS_TOKEN_FILE` | server, sidebar, helper shell scripts, Amp/Pi integrations | Bearer token file override; default `/tmp/opensessions.<key>.token` |
| `OPENSESSIONS_URL` | Amp/Pi integrations | Explicit server base URL, tried before derived endpoints |
| `OPENSESSIONS_RELEASE_BASE` | TPM bootstrap, `scripts/postinstall.js` | Release download base for prebuilt binaries (required for fork installs) |
| `OPENSESSIONS_DEBUG_LOG` | server | Append debug lines to this file; unset disables debug logging |
| `OPENSESSIONS_LAZYDIFF` | sidebar | Explicit lazydiff binary path override. By default the sidebar prefers the bundled sibling binary in `bin/`, then `lazydiff` on `PATH` |
| `OPENSESSIONS_SKIP_BINARY_DOWNLOAD` | TPM bootstrap | Set to `1` to skip prebuilt binary downloads and use a local `target/` build |
| `OPENSESSIONS_WIDTH` | ignored | Deprecated stale bootstrap variable; width is controlled by persisted `sidebarWidth` |
| `SESSIONIZER_DIR` | tmux sessionizer popup | Colon-separated directories searched for new-session candidates (e.g. `$HOME/Code:$HOME/.config`). Also checked via `tmux show-environment -g` when the shell variable is unset. Defaults to `$HOME/Documents` |
| `SESSIONIZER_MAXDEPTH` | tmux sessionizer popup | Maximum `find` depth when collecting new-session candidates. Also checked via `tmux show-environment -g` when the shell variable is unset. Defaults to `3` |

## Related Files Written By The Runtime

| Path | Purpose |
| --- | --- |
| `~/.config/opensessions/session-order.json` | Persisted custom session ordering |
| `/tmp/opensessions.<key>.pid` | Server PID file used by bootstrap and integration discovery (`/tmp/opensessions.pid` without a key) |
| `/tmp/opensessions.<key>.token` | Bearer token for authenticated endpoints (`/tmp/opensessions.token` without a key) |
| `/tmp/opensessions.<key>.server.log` | Server output when a helper script starts the server |
| `$OPENSESSIONS_DEBUG_LOG` | Debug log, only when the variable is set |

## Server Key And Port

Each tmux server gets its own opensessions server. The server key is
`OPENSESSIONS_SERVER_KEY` (surrounding whitespace trimmed) when set; otherwise
the first 16 hex characters of the SHA-256 of the canonical tmux socket path
from `$TMUX`. Without either, the key is empty and the defaults are port
`7391`, `/tmp/opensessions.pid`, and `/tmp/opensessions.token`.

An explicit port (`OPENSESSIONS_PORT`, or the tmux global environment for the
helper scripts) always wins. Otherwise the port is `22000 + offset`, where the
offset is computed from the key the same way by the server, sidebar, tmux
scripts, and the Amp and Pi integrations:

| Key | Offset |
| --- | --- |
| 1–15 ASCII digits (legacy numeric key) | decimal value mod 20000 |
| hexadecimal digits only (socket-derived keys) | first 8 hex digits as a number, mod 20000 |
| anything else, e.g. `work` | first 8 hex digits of the key's SHA-256, mod 20000 |

So `123` and `123456` map to ports 22123 and 25456, and `work` maps to 23687.
The key also names the PID and token files, so prefer keys made of letters,
digits, `-`, and `_`.

## Mux Detection Rules

If `mux` is unset, the supported built-in auto-detection path is:

1. `$TMUX` -> provider named `tmux`
2. no supported match -> `null`

tmux is the only supported built-in mux today. Older zellij helper code exists in the repository but is not part of the documented support surface.
