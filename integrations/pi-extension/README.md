# opensessions Pi runtime extension

Pi extension that registers the live `pi` process with opensessions so tmux pane scans can map Pi session IDs to exact panes. It also sends live Pi agent events with the latest user prompt so the sidebar does not have to wait for JSONL scanning.

## Usage

Run Pi with the extension:

```bash
pi --extension /path/to/opensessions/integrations/pi-extension/opensessions-runtime.ts
```

Or copy/symlink the file into one of Pi's extension locations:

- `~/.pi/agent/extensions/`
- `.pi/extensions/`

## What it does

The extension POSTs the current Pi runtime identity to opensessions on localhost:

- `POST /api/runtime/pi/upsert` on `session_start`
- heartbeat every 5 seconds while Pi is alive
- `POST /api/runtime/pi/delete` on `session_shutdown`
- `POST /api/agent-event` on `before_agent_start` with `status=running` and `lastUserPrompt`
- `POST /api/agent-event` on `agent_end` with `status=done`

Each request goes to the first of these endpoints that accepts it:

1. `OPENSESSIONS_URL`, if set (trailing `/` ignored)
2. `http://127.0.0.1:$OPENSESSIONS_PORT`, if set
3. the port derived from `OPENSESSIONS_SERVER_KEY`, if set
4. the port derived from the tmux socket in `$TMUX` (the per-socket key the
   tmux scripts and server use)
5. `http://127.0.0.1:7391`

Requests carry the bearer token from `OPENSESSIONS_TOKEN_FILE`, or
`/tmp/opensessions.<key>.token` for key-derived endpoints, or
`/tmp/opensessions.token`. See the server key rule in the
[configuration reference](../../docs/reference/configuration.md#server-key-and-port).
