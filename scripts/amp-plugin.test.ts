import { afterAll, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = mkdtempSync(join(tmpdir(), "opensessions-amp-plugin-"));
const tokenFile = join(root, "token");
writeFileSync(tokenFile, "test-token\n");

const SERVER = "http://127.0.0.1:1";
process.env.OPENSESSIONS_AMP_PLUGIN_LOG = join(root, "plugin.log");
process.env.OPENSESSIONS_URL = `${SERVER}/`;
process.env.OPENSESSIONS_TOKEN_FILE = tokenFile;
delete process.env.OPENSESSIONS_PORT;
delete process.env.OPENSESSIONS_SERVER_KEY;
delete process.env.TMUX;

type Behavior = { fail?: boolean; delayMs?: number };
const delivered: string[] = [];
const requested: string[] = [];
let script: Behavior[] = [];

const originalFetch = globalThis.fetch;
// Never touch the network: only the configured endpoint can accept events.
globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = String(input);
  if (url !== `${SERVER}/api/agent-event`) return new Response(null, { status: 503 });
  const { status } = JSON.parse(String(init?.body)) as { status: string };
  requested.push(status);
  const behavior = script.shift() ?? {};
  if (behavior.delayMs) await Bun.sleep(behavior.delayMs);
  if (behavior.fail) throw new Error("connection refused");
  delivered.push(status);
  return new Response(null, { status: 204 });
}) as typeof fetch;

afterAll(() => {
  globalThis.fetch = originalFetch;
  rmSync(root, { recursive: true, force: true });
});

const { default: plugin, appendBoundedLog } = await import("../integrations/amp/opensessions");

type Handler = (event: Record<string, unknown>, ctx: Record<string, unknown>) => Promise<unknown>;
const handlers = new Map<string, Handler>();
const noTmux = (() => Promise.reject(new Error("no tmux"))) as unknown;
plugin({ on: (name: string, handler: Handler) => handlers.set(name, handler), $: noTmux } as never);

function emit(name: string, event: Record<string, unknown>, threadId: string): Promise<unknown> {
  return handlers.get(name)!(event, { thread: { id: threadId }, $: noTmux });
}

beforeEach(() => {
  delivered.length = 0;
  requested.length = 0;
  script = [];
});

describe("Amp plugin event delivery", () => {
  test("a newer event drops an older queued retry for the same thread", async () => {
    script = [{ fail: true }];
    await emit("agent.start", { message: "go" }, "thread-a");
    await emit("agent.end", { status: "done" }, "thread-a");
    await Bun.sleep(700);
    expect(delivered).toEqual(["done"]);
    expect(requested).toEqual(["running", "done"]);
  });

  test("an in-flight retry cannot land after a newer event", async () => {
    script = [{ fail: true }, { delayMs: 300 }];
    await emit("agent.start", { message: "go" }, "thread-b");
    await Bun.sleep(320); // retry for "running" is now in flight
    await emit("agent.end", { status: "done" }, "thread-b");
    await Bun.sleep(700);
    expect(delivered.at(-1)).toBe("done");
    expect(delivered.filter((status) => status === "done")).toHaveLength(1);
  });

  test("still retries the latest event until it is delivered", async () => {
    script = [{ fail: true }, { fail: true }];
    await emit("agent.end", { status: "done" }, "thread-c");
    await Bun.sleep(1_200);
    expect(delivered).toEqual(["done"]);
    expect(requested).toEqual(["done", "done", "done"]);
  });
});

describe("Amp plugin log", () => {
  test("rotates instead of growing past its size cap and keeps whole lines", () => {
    const path = join(root, "bounded.log");
    const line = "z".repeat(99);
    for (let i = 0; i < 25; i += 1) appendBoundedLog(path, line, 1_000);

    expect(statSync(path).size).toBeLessThanOrEqual(1_000);
    expect(existsSync(`${path}.1`)).toBe(true);
    expect(statSync(`${path}.1`).size).toBeLessThanOrEqual(1_000);
    for (const file of [path, `${path}.1`]) {
      for (const entry of readFileSync(file, "utf8").trimEnd().split("\n")) expect(entry).toBe(line);
    }
  });
});
