/**
 * opensessions plugin for Amp
 *
 * Reports agent status to the opensessions server via HTTP. When this plugin
 * is installed and the server is running, opensessions will skip its cloud
 * API / DTW WebSocket polling for the active thread and rely entirely on the
 * events emitted here.
 *
 * Install:
 *   Copy (or symlink) this file to ~/.config/amp/plugins/opensessions.ts
 *
 * Requires a recent Amp build that exposes `ctx.thread.id` on session.start.
 *
 * Event mapping:
 *   session.start → idle      (registers the thread + its project/session)
 *   agent.start   → running
 *   agent.end     → done | error | interrupted  (from event.status)
 *   tool.call     → tool-running
 *   tool.result   → error on failure, interrupted on cancel, running on success
 *
 * Session identity:
 *   1. `tmux display-message -p '#S'` — works when Amp is launched inside a
 *      tmux pane managed by opensessions.
 *   2. `process.cwd()` — server resolves to a session via its dir→session map.
 *   Both are sent in every payload; the server picks whichever resolves.
 */

// @i-know-the-amp-plugin-api-is-wip-and-very-experimental-right-now
import type { PluginAPI } from "@ampcode/plugin";
import { appendFileSync, readFileSync, readdirSync, realpathSync } from "fs";
import { createHash } from "crypto";
import { join } from "path";
import { homedir } from "os";

const PLUGIN_LOG_PATH = process.env.OPENSESSIONS_AMP_PLUGIN_LOG || "/tmp/opensessions-plugin.log";
function plog(msg: string): void {
  try { appendFileSync(PLUGIN_LOG_PATH, `[${new Date().toISOString()}] ${msg}\n`); } catch {}
}

const DEFAULT_SERVER_PORT = 7391;
const RUST_SERVER_PORT_BASE = 22000;
const POST_TIMEOUT_MS = 750;
const RETRY_INITIAL_MS = 250;
const RETRY_MAX_MS = 2_000;
const RETRY_FOR_MS = 30_000;

type Status = "idle" | "running" | "tool-running" | "done" | "error" | "interrupted";

interface EventPayload {
  agent: "amp";
  status: Status;
  threadId?: string;
  threadName?: string;
  lastUserPrompt?: string;
  tmuxSession?: string;
  paneId?: string;
  projectDir: string;
  ts: number;
}

/**
 * Resolve a thread's title via the Amp cloud API. Recent Amp builds no longer
 * persist every thread to ~/.local/share/amp/threads/<id>.json, so we hit
 * GET <ampUrl>/api/threads/:id with the local apiKey instead.
 *
 * This is a one-shot fetch per thread — the plugin caches the result in
 * memory and includes it in every subsequent event POST.
 */
const SETTINGS_PATH = join(homedir(), ".config", "amp", "settings.json");
const SECRETS_PATH = join(homedir(), ".local", "share", "amp", "secrets.json");
const DEFAULT_AMP_URL = "https://ampcode.com";
const TITLE_FETCH_TIMEOUT_MS = 5_000;

function loadAmpUrl(): string {
  try {
    const raw = readFileSync(SETTINGS_PATH, "utf8");
    const settings = JSON.parse(raw) as { url?: unknown };
    if (typeof settings.url === "string" && settings.url.length > 0) {
      return settings.url.replace(/\/$/, "");
    }
  } catch {}
  return DEFAULT_AMP_URL;
}

function loadApiKey(ampUrl: string): string | null {
  try {
    const raw = readFileSync(SECRETS_PATH, "utf8");
    const secrets = JSON.parse(raw) as Record<string, unknown>;
    const urlWithSlash = ampUrl.endsWith("/") ? ampUrl : `${ampUrl}/`;
    const urlWithoutSlash = ampUrl.replace(/\/$/, "");
    const key =
      secrets[`apiKey@${urlWithSlash}`] ??
      secrets[`apiKey@${urlWithoutSlash}`] ??
      secrets.apiKey;
    return typeof key === "string" && key.length > 0 ? key : null;
  } catch {
    return null;
  }
}

const AMP_URL = loadAmpUrl();
const API_KEY = loadApiKey(AMP_URL);

async function fetchThreadTitle(threadId: string): Promise<string | null> {
  if (!API_KEY) return null;
  try {
    const res = await fetch(`${AMP_URL}/api/threads/${threadId}`, {
      headers: { Authorization: `Bearer ${API_KEY}` },
      signal: AbortSignal.timeout(TITLE_FETCH_TIMEOUT_MS),
    });
    if (!res.ok) return null;
    const body = (await res.json()) as { title?: unknown };
    return typeof body.title === "string" && body.title.length > 0 ? body.title : null;
  } catch {
    return null;
  }
}

/**
 * Port resolution — matches the tmux-scoped opensessions server namespace.
 * Rust servers map the canonical socket SHA key into the 22000–41999 range.
 */
export function hashServerKey(input: string): string {
  return createHash("sha256").update(input).digest("hex").slice(0, 16);
}

/**
 * Server key -> port. Keep identical to server_port_offset in
 * packages/runtime-rs/src/shared.rs and integrations/tmux-plugin/scripts/
 * server-common.sh. After trimming: 1-15 digits are a legacy decimal key;
 * hex-only keys (socket-derived SHA keys) use their first 8 hex digits; any
 * other key uses the first 8 hex digits of its SHA-256.
 */
export function portForServerKey(rawKey: string): number | null {
  const key = rawKey.trim();
  if (!key) return null;
  const value = /^\d{1,15}$/.test(key)
    ? Number.parseInt(key, 10)
    : Number.parseInt((/^[0-9a-fA-F]+$/.test(key) ? key : hashServerKey(key)).slice(0, 8), 16);
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

  add(process.env.OPENSESSIONS_URL?.trim().replace(/\/+$/, ""));

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

  // Also consider every live opensessions server discovered through its
  // /tmp pid file. This is a fallback candidate list, not a broadcast:
  // postOnce delivers each event to the first candidate that accepts it,
  // trying the last successful endpoint first.
  try {
    for (const entry of readdirSync("/tmp")) {
      const match = /^opensessions\.([A-Za-z0-9_-]+)\.pid$/.exec(entry);
      if (!match) continue;
      if (!pidFileIsAlive(join("/tmp", entry))) continue;
      const key = match[1];
      const port = portForServerKey(key);
      if (!port) continue;
      add(`http://127.0.0.1:${port}`, `/tmp/opensessions.${key}.token`);
    }
  } catch {}

  if (urls.length === 0) add(`http://127.0.0.1:${DEFAULT_SERVER_PORT}`);
  return urls;
}

function pidFileIsAlive(path: string): boolean {
  try {
    const pid = Number.parseInt(readFileSync(path, "utf8").trim(), 10);
    if (!Number.isFinite(pid) || pid <= 0) return false;
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function serverUrls(): string[] {
  // Resolve at send time, not plugin-load time. Amp plugin processes can live
  // across opensessions restarts and tmux env changes; stale endpoints during
  // a restart should not strand events until Amp itself is restarted.
  return resolveServerUrls();
}

function authToken(serverUrl: string): string | undefined {
  try {
    const explicit = process.env.OPENSESSIONS_TOKEN_FILE?.trim();
    if (explicit) return readFileSync(explicit, "utf8").trim();
    const path = tokenFileByUrl.get(serverUrl) ?? "/tmp/opensessions.token";
    return readFileSync(path, "utf8").trim();
  } catch {
    return undefined;
  }
}

let preferredServerUrl: string | undefined;

plog(`plugin loaded endpoints=${serverUrls().join(",")} ampUrl=${AMP_URL} apiKey=${API_KEY ? "set" : "missing"} tmux=${process.env.TMUX ?? "none"} cwd=${process.cwd()} pid=${process.pid}`);

async function resolveTmuxSession($: PluginAPI["$"]): Promise<string | null> {
  try {
    const result = await $`tmux display-message -p '#S'`;
    const name = result.stdout.trim();
    return name.length > 0 ? name : null;
  } catch {
    return null;
  }
}

async function resolveTmuxPane($: PluginAPI["$"]): Promise<string | null> {
  try {
    const result = await $`tmux display-message -p '#{pane_id}'`;
    const paneId = result.stdout.trim();
    return paneId.length > 0 ? paneId : null;
  } catch {
    return null;
  }
}

type PendingPayload = EventPayload & { firstAttemptTs: number; retryDelayMs: number };

// Delivery invariants:
// - Every network send (first attempt or retry) runs through `exclusive`, so
//   sends never overlap and an older event cannot land after a newer one.
// - Each event gets a sequence number; the newest event for a thread wins.
//   A newer event drops any queued older event for that thread, and an older
//   event that is still waiting to send is skipped once a newer one exists.
const pendingByThread = new Map<string, PendingPayload>();
const latestSeqByKey = new Map<string, number>();
let eventSeq = 0;
let retryTimer: ReturnType<typeof setTimeout> | undefined;
let sendChain: Promise<unknown> = Promise.resolve();

function exclusive<T>(task: () => Promise<T>): Promise<T> {
  const run = sendChain.then(task, task);
  sendChain = run.catch(() => {});
  return run;
}

function pendingKey(payload: EventPayload): string {
  return payload.threadId ?? `${payload.projectDir}:${payload.tmuxSession ?? ""}`;
}

function isLatest(key: string, seq: number): boolean {
  return latestSeqByKey.get(key) === seq;
}

function scheduleRetry(): void {
  if (retryTimer || pendingByThread.size === 0) return;
  const nextDelay = Math.min(
    ...Array.from(pendingByThread.values()).map((payload) => payload.retryDelayMs),
  );
  retryTimer = setTimeout(() => {
    retryTimer = undefined;
    void flushPending();
  }, nextDelay);
}

function flushPending(): Promise<void> {
  return exclusive(async () => {
    for (const [key, payload] of Array.from(pendingByThread.entries())) {
      // A newer event may have replaced or dropped this entry while an
      // earlier retry in this pass was awaiting the network.
      if (pendingByThread.get(key) !== payload) continue;
      const now = Date.now();
      if (now - payload.firstAttemptTs > RETRY_FOR_MS) {
        pendingByThread.delete(key);
        plog(`retry drop status=${payload.status} thread=${payload.threadId?.slice(0, 8)} ageMs=${now - payload.firstAttemptTs}`);
        continue;
      }
      const { firstAttemptTs: _firstAttemptTs, retryDelayMs, ...eventPayload } = payload;
      const delivered = await postOnce(eventPayload);
      if (pendingByThread.get(key) !== payload) continue;
      if (delivered) {
        pendingByThread.delete(key);
      } else {
        pendingByThread.set(key, {
          ...payload,
          retryDelayMs: Math.min(retryDelayMs * 2, RETRY_MAX_MS),
        });
      }
    }
  }).finally(scheduleRetry);
}

async function postOnce(payload: EventPayload): Promise<boolean> {
  const urls = serverUrls();
  const candidates = preferredServerUrl
    ? [preferredServerUrl, ...urls.filter((url) => url !== preferredServerUrl)]
    : urls;
  let lastError: unknown;
  for (const serverUrl of candidates) {
    const endpoint = `${serverUrl}/api/agent-event`;
    try {
      const token = authToken(serverUrl);
      if (!token) continue;
      const res = await fetch(endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` },
        body: JSON.stringify(payload),
        signal: AbortSignal.timeout(POST_TIMEOUT_MS),
      });
      if (res.status === 204) {
        preferredServerUrl = serverUrl;
        plog(`POST endpoint=${endpoint} status=${payload.status} thread=${payload.threadId?.slice(0, 8)} name=${payload.threadName ?? "-"} -> ${res.status}`);
        return true;
      }
      plog(`POST endpoint=${endpoint} status=${payload.status} thread=${payload.threadId?.slice(0, 8)} name=${payload.threadName ?? "-"} -> ${res.status}`);
    } catch (err) {
      lastError = err;
      plog(`POST endpoint=${endpoint} status=${payload.status} thread=${payload.threadId?.slice(0, 8)} ERROR ${String(err)}`);
    }
  }
  plog(`POST status=${payload.status} thread=${payload.threadId?.slice(0, 8)} failed all endpoints last=${String(lastError)}`);
  return false;
}

async function post(payload: EventPayload): Promise<void> {
  const key = pendingKey(payload);
  const seq = ++eventSeq;
  latestSeqByKey.set(key, seq);
  // This event supersedes any older event still queued for the same thread.
  pendingByThread.delete(key);

  await exclusive(async () => {
    if (!isLatest(key, seq)) return;
    if (await postOnce(payload)) return;
    if (!isLatest(key, seq)) return;
    pendingByThread.set(key, {
      ...payload,
      firstAttemptTs: Date.now(),
      retryDelayMs: RETRY_INITIAL_MS,
    });
    plog(`retry queued status=${payload.status} thread=${payload.threadId?.slice(0, 8)} key=${key} pending=${pendingByThread.size}`);
    scheduleRetry();
  });
}

export default function (amp: PluginAPI) {
  const projectDir = process.cwd();
  let tmuxSession: string | null = null;
  let paneId: string | null = null;

  // Resolve tmux session eagerly so we have it ready for the first event.
  resolveTmuxSession(amp.$).then((name) => {
    tmuxSession = name;
  });
  resolveTmuxPane(amp.$).then((id) => {
    paneId = id;
  });

  /**
   * ctx.thread.id is documented as "available when in the current invocation
   * context" — in practice it's present for session.start and tool.call but
   * often missing for agent.start, agent.end, and tool.result. The plugin
   * process is shared across concurrent threads, so we can't just stash a
   * single "current threadId" globally. Instead we correlate tool.result
   * with the preceding tool.call via toolUseID, and fall back to whatever
   * ctx.thread.id gives us otherwise.
   */
  const threadByToolUseID = new Map<string, string>();
  const lastPromptByThread = new Map<string, string>();
  let lastKnownThreadId: string | undefined;

  // Title cache. On first encounter of a thread we fire an async cloud fetch
  // so subsequent events carry the title. Titles are typed shortly after the
  // thread starts, so one retry on null is worth it — the first event is
  // usually session.start, before Amp has generated a title.
  const titleCache = new Map<string, string>();
  const titleInFlight = new Set<string>();

  const kickTitleFetch = (threadId: string): void => {
    if (titleCache.has(threadId) || titleInFlight.has(threadId)) return;
    titleInFlight.add(threadId);
    void fetchThreadTitle(threadId).then((title) => {
      titleInFlight.delete(threadId);
      if (title) {
        titleCache.set(threadId, title);
        plog(`title cached thread=${threadId.slice(0, 8)} title=${JSON.stringify(title)}`);
      }
    });
  };

  const resolveTitle = (threadId: string | undefined): string | undefined => {
    if (!threadId) return undefined;
    const cached = titleCache.get(threadId);
    if (cached) return cached;
    kickTitleFetch(threadId);
    return undefined;
  };

  const rememberThreadId = (threadId: string | undefined): void => {
    if (threadId) lastKnownThreadId = threadId;
  };

  const send = async (status: Status, threadId: string | undefined, lastUserPrompt?: string): Promise<void> => {
    rememberThreadId(threadId);
    const tid = threadId ?? lastKnownThreadId;
    if (tid && lastUserPrompt) lastPromptByThread.set(tid, lastUserPrompt);
    await post({
      agent: "amp",
      status,
      threadId: tid,
      threadName: resolveTitle(tid),
      lastUserPrompt: lastUserPrompt ?? (tid ? lastPromptByThread.get(tid) : undefined),
      tmuxSession: tmuxSession ?? undefined,
      paneId: paneId ?? undefined,
      projectDir,
      ts: Date.now(),
    });
  };

  amp.on("session.start", async (event, ctx) => {
    if (!tmuxSession) tmuxSession = await resolveTmuxSession(ctx.$);
    if (!paneId) paneId = await resolveTmuxPane(ctx.$);
    await send("idle", event.thread?.id ?? ctx.thread?.id);
  });

  amp.on("agent.start", async (event, ctx) => {
    await send("running", ctx.thread?.id, event.message);
    return {};
  });

  amp.on("agent.end", async (event, ctx) => {
    const status: Status =
      event.status === "done" ? "done" :
      event.status === "error" ? "error" :
      "interrupted";
    await send(status, ctx.thread?.id, event.message);
  });

  amp.on("tool.call", async (event, ctx) => {
    const threadId = event.thread?.id ?? ctx.thread?.id;
    if (threadId && event.toolUseID) threadByToolUseID.set(event.toolUseID, threadId);
    await send("tool-running", threadId);
    return { action: "allow" };
  });

  amp.on("tool.result", async (event, ctx) => {
    // Recover threadId from the matching tool.call since tool.result doesn't
    // carry one on the event payload.
    const threadId = ctx.thread?.id ?? (event.toolUseID ? threadByToolUseID.get(event.toolUseID) : undefined);
    if (event.toolUseID) threadByToolUseID.delete(event.toolUseID);

    if (event.status === "error") {
      await send("error", threadId);
    } else if (event.status === "cancelled") {
      await send("interrupted", threadId);
    } else {
      // Tool finished successfully. The agent is now streaming the reply, so
      // flip back to "running" — otherwise the UI stays pinned at
      // "tool-running" until the next agent.end.
      await send("running", threadId);
    }
  });
}
