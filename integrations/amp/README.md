# opensessions plugin for Amp

Real-time agent status for Amp threads, driven by the Amp plugin API instead
of by polling Amp's cloud API.

## Why

The server's built-in Amp watcher reads Amp's local files
(`~/.local/share/amp/threads/*.json` and `~/.cache/amp/logs/threads/`), so it
only sees a status change once Amp has written it and the next scan runs.
This plugin reports lifecycle events as they happen. A tracked row that is
newer than a thread's log snapshot keeps winning until the log catches up, so
plugin events are not overwritten by stale watcher data. Without the plugin,
the watcher alone still tracks Amp threads.

To show thread titles, the plugin fetches `GET <amp url>/api/threads/<id>`
once per thread using the API key in `~/.local/share/amp/secrets.json`.

## Install

Copy (or symlink) `opensessions.ts` into `~/.config/amp/plugins/`:

```sh
mkdir -p ~/.config/amp/plugins
cp integrations/amp/opensessions.ts ~/.config/amp/plugins/opensessions.ts
```

Restart Amp. A recent Amp build that exposes `ctx.thread.id` on `session.start`
is required.

## Server discovery

Endpoints are resolved again for every event, so the plugin follows
opensessions restarts and tmux environment changes. Candidates, in order:

1. `OPENSESSIONS_URL`, if set (a trailing `/` is ignored).
2. `http://127.0.0.1:$OPENSESSIONS_PORT`, if set.
3. The port derived from `OPENSESSIONS_SERVER_KEY`, if set.
4. The port derived from the tmux socket in `$TMUX` (the same per-socket key
   the tmux scripts and server use; ports fall in 22000–41999).
5. Every live server found through `/tmp/opensessions.<key>.pid`.
6. `http://127.0.0.1:7391`, only when nothing above produced a candidate.

Each event goes to the first candidate that accepts it (the last successful
endpoint is tried first); it is not broadcast to every server. Requests carry
the bearer token from `OPENSESSIONS_TOKEN_FILE` when set, otherwise from
`/tmp/opensessions.<key>.token` for key-derived candidates, or
`/tmp/opensessions.token`. Candidates without a readable token are skipped.

Failed events are retried with backoff for up to 30 seconds. A newer event for
the same thread replaces any queued older one, and events are sent one at a
time, so an older status can never overwrite a newer one.

| Variable | Purpose |
| --- | --- |
| `OPENSESSIONS_URL` | Explicit server base URL, tried first |
| `OPENSESSIONS_PORT` | Explicit server port on `127.0.0.1` |
| `OPENSESSIONS_SERVER_KEY` | Explicit server key; see the key rule in the [configuration reference](../../docs/reference/configuration.md#server-key-and-port) |
| `OPENSESSIONS_TOKEN_FILE` | Bearer token file used for every candidate |
| `OPENSESSIONS_AMP_PLUGIN_LOG` | Plugin log path (default `/tmp/opensessions-plugin.log`); rotated to `<path>.1` past 5 MiB |

## Event mapping

| Amp event | opensessions status |
| --- | --- |
| `session.start` | `idle` |
| `agent.start` | `running` |
| `agent.end` (`status=done`) | `done` |
| `agent.end` (`status=error`) | `error` |
| `agent.end` (other) | `interrupted` |
| `tool.call` | `tool-running` |
| `tool.result` (`status=error`) | `error` |
| `tool.result` (`status=cancelled`) | `interrupted` |
| `tool.result` (success) | `running` (agent streaming the reply) |

## Session resolution

Every event payload carries:

1. `tmuxSession` — from `tmux display-message -p '#S'`, if Amp is running
   inside a tmux pane.
2. `projectDir` — from `process.cwd()`.

The server prefers a known tmux session name; otherwise it resolves
`projectDir` against its session→directory map.
