import type { ExtensionAPI, ExtensionContext } from "@mariozechner/pi-coding-agent";
import { readFileSync, realpathSync } from "node:fs";
import { createHash } from "node:crypto";

interface PiRuntimePayload {
  pid: number;
  ppid: number;
  sessionId: string;
  sessionFile?: string;
  cwd: string;
  sessionName?: string;
  ts: number;
}

type AgentStatus = "running" | "done" | "error" | "interrupted";

interface AgentEventPayload {
  agent: "pi";
  status: AgentStatus;
  threadId: string;
  threadName?: string;
  lastUserPrompt?: string;
  projectDir: string;
  ts: number;
}

const DEFAULT_SERVER_PORT = 7391;
const RUST_SERVER_PORT_BASE = 22000;
const HEARTBEAT_MS = 5_000;
const REQUEST_TIMEOUT_MS = 750;

/**
 * Mirror opensessions Rust runtime port resolution. The
 * server port is derived from a hash of the tmux socket path so concurrent
 * tmux servers on the same machine get independent opensessions servers.
 */
function hashServerKey(input: string): string {
  return createHash("sha256").update(input).digest("hex").slice(0, 16);
}

function portForServerKey(key: string): number | null {
  const legacy = /^\d{1,5}$/.test(key) ? Number.parseInt(key, 10) : null;
  const value = legacy ?? Number.parseInt(key.slice(0, 8), 16);
  return Number.isFinite(value) ? RUST_SERVER_PORT_BASE + (value % 20000) : null;
}

const tokenFileByUrl = new Map<string, string>();

function resolveServerUrls(): string[] {
  tokenFileByUrl.clear();
  const urls: string[] = [];
  const add = (url: string | undefined, tokenFile?: string): void => {
    if (!url) return;
    if (!urls.includes(url)) urls.push(url);
    if (tokenFile) tokenFileByUrl.set(url, tokenFile);
  };

  add(process.env.OPENSESSIONS_URL?.replace(/\/+$/, ""));

  const explicit = Number.parseInt(process.env.OPENSESSIONS_PORT ?? "", 10);
  if (Number.isFinite(explicit) && explicit > 0) add(`http://127.0.0.1:${explicit}`);

  const explicitKey = process.env.OPENSESSIONS_SERVER_KEY?.trim();
  if (explicitKey) {
    const port = portForServerKey(explicitKey);
    if (port) add(`http://127.0.0.1:${port}`, `/tmp/opensessions.${explicitKey}.token`);
  }

  const tmux = process.env.TMUX?.trim();
  if (tmux) {
    const socketPath = tmux.split(",", 1)[0];
    if (socketPath) {
      let canonicalPath = socketPath;
      try { canonicalPath = realpathSync(socketPath); } catch {}
      const key = hashServerKey(canonicalPath);
      const port = portForServerKey(key);
      if (port) add(`http://127.0.0.1:${port}`, `/tmp/opensessions.${key}.token`);
    }
  }

  add(`http://127.0.0.1:${DEFAULT_SERVER_PORT}`);
  return urls;
}

function authToken(serverUrl: string): string | undefined {
  try {
    const explicit = process.env.OPENSESSIONS_TOKEN_FILE?.trim();
    if (explicit) return readFileSync(explicit, "utf8").trim();
    return readFileSync(tokenFileByUrl.get(serverUrl) ?? "/tmp/opensessions.token", "utf8").trim();
  } catch {
    return undefined;
  }
}

export default function opensessionsRuntime(pi: ExtensionAPI) {
  let heartbeat: ReturnType<typeof setInterval> | null = null;
  let heartbeatRequest: Promise<void> | null = null;
  let current: Omit<PiRuntimePayload, "ts" | "sessionName"> | null = null;

  function buildPayload(ctx: ExtensionContext): PiRuntimePayload {
    return {
      pid: process.pid,
      ppid: process.ppid,
      sessionId: ctx.sessionManager.getSessionId(),
      sessionFile: ctx.sessionManager.getSessionFile(),
      cwd: ctx.sessionManager.getCwd(),
      sessionName: pi.getSessionName(),
      ts: Date.now(),
    };
  }

  async function post(path: string, body: unknown): Promise<void> {
    for (const serverUrl of resolveServerUrls()) {
      try {
        const token = authToken(serverUrl);
        if (!token) continue;
        const response = await fetch(`${serverUrl}${path}`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
          body: JSON.stringify(body),
          signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
        });
        if (response.status >= 200 && response.status < 300) return;
      } catch {
        // opensessions may not be running yet; retry on next heartbeat
      }
    }
  }

  function agentPayload(
    status: AgentStatus,
    ctx: ExtensionContext,
    lastUserPrompt?: string,
  ): AgentEventPayload {
    return {
      agent: "pi",
      status,
      threadId: ctx.sessionManager.getSessionId(),
      threadName: pi.getSessionName(),
      lastUserPrompt,
      projectDir: ctx.sessionManager.getCwd(),
      ts: Date.now(),
    };
  }

  function clearHeartbeat(): void {
    if (!heartbeat) return;
    clearInterval(heartbeat);
    heartbeat = null;
  }

  function startHeartbeat(ctx: ExtensionContext): void {
    clearHeartbeat();
    heartbeat = setInterval(() => {
      if (!current) {
        current = {
          pid: process.pid,
          ppid: process.ppid,
          sessionId: ctx.sessionManager.getSessionId(),
          sessionFile: ctx.sessionManager.getSessionFile(),
          cwd: ctx.sessionManager.getCwd(),
        };
      }
      if (!heartbeatRequest) {
        heartbeatRequest = post("/api/runtime/pi/upsert", {
          ...current,
          sessionName: pi.getSessionName(),
          ts: Date.now(),
        } satisfies PiRuntimePayload).finally(() => {
          heartbeatRequest = null;
        });
      }
    }, HEARTBEAT_MS);
  }

  pi.on("session_start", async (_event, ctx) => {
    const payload = buildPayload(ctx);
    current = {
      pid: payload.pid,
      ppid: payload.ppid,
      sessionId: payload.sessionId,
      sessionFile: payload.sessionFile,
      cwd: payload.cwd,
    };
    void post("/api/runtime/pi/upsert", payload);
    startHeartbeat(ctx);
  });

  pi.on("before_agent_start", async (event, ctx) => {
    void post(
      "/api/agent-event",
      agentPayload("running", ctx, typeof event.prompt === "string" ? event.prompt : undefined),
    );
  });

  pi.on("agent_end", async (_event, ctx) => {
    void post("/api/agent-event", agentPayload("done", ctx));
  });

  pi.on("session_shutdown", async () => {
    clearHeartbeat();
    current = null;
    void post("/api/runtime/pi/delete", { pid: process.pid });
  });
}
