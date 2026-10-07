import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const repo = resolve(import.meta.dir, "..");
const scriptsDir = join(repo, "integrations/tmux-plugin/scripts");
const cleanups: Array<() => void> = [];

afterAll(() => {
  for (const cleanup of cleanups.splice(0).reverse()) {
    try {
      cleanup();
    } catch {}
  }
});

function tempDir(prefix: string, base = tmpdir()): string {
  const dir = mkdtempSync(join(base, prefix));
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function writeExecutable(path: string, contents: string): void {
  writeFileSync(path, contents);
  chmodSync(path, 0o755);
}

function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolvePort(typeof address === "object" && address ? address.port : 0));
    });
  });
}

describe("uninstall.sh", () => {
  const providerSource = readFileSync(join(repo, "packages/runtime-rs/src/tmux_provider.rs"), "utf8");
  const uninstallSource = readFileSync(join(scriptsDir, "uninstall.sh"), "utf8");

  function serverHooks(): string[] {
    const cleanup = /fn cleanup_hooks\(&self\) \{[\s\S]*?for hook in \[([\s\S]*?)\]/.exec(providerSource);
    expect(cleanup).not.toBeNull();
    return Array.from(cleanup![1].matchAll(/"([a-z-]+)"/g), (match) => match[1]);
  }

  function uninstallHooks(): string[] {
    const loop = /for hook in \\\n([\s\S]*?); do/.exec(uninstallSource);
    expect(loop).not.toBeNull();
    return loop![1].split(/\s+/).filter((word) => word && word !== "\\");
  }

  test("covers every hook the server installs, at the server's slot", () => {
    expect(uninstallHooks().sort()).toEqual(serverHooks().sort());
    const index = /const OPENSESSIONS_HOOK_INDEX: u16 = (\d+);/.exec(providerSource)?.[1];
    expect(uninstallSource).toContain(`OPENSESSIONS_HOOK_INDEX=${index}`);
  });

  const realTmux = spawnSync("sh", ["-c", "command -v tmux"], { encoding: "utf8" }).stdout.trim();
  test.skipIf(!realTmux)("removes only opensessions hook slots on a private tmux server", () => {
    // Short path: tmux socket paths are limited to ~104 bytes on macOS.
    const dir = tempDir("os-uninst-", "/tmp");
    const socket = join(dir, "s");
    const bin = join(dir, "bin");
    mkdirSync(bin);
    // Every bare `tmux` call in the script goes to the private server.
    writeExecutable(join(bin, "tmux"), `#!/bin/sh\nexec "${realTmux}" -S "${socket}" "$@"\n`);
    const tmux = (...args: string[]) =>
      spawnSync(realTmux, ["-S", socket, ...args], { encoding: "utf8" });
    cleanups.push(() => tmux("kill-server"));

    expect(tmux("-f", "/dev/null", "new-session", "-d", "-s", "main").status).toBe(0);
    // Unreachable port so the script's /quit request fails fast.
    tmux("set-environment", "-g", "OPENSESSIONS_PORT", "1");
    tmux("set-hook", "-g", "client-session-changed[0]", "display-message user-zero");
    tmux("set-hook", "-g", "after-select-pane[42]", "display-message user-forty-two");
    for (const hook of serverHooks()) {
      expect(tmux("set-hook", "-g", `${hook}[909]`, "display-message opensessions").status).toBe(0);
    }

    const env = { ...process.env, PATH: `${bin}:${process.env.PATH}`, TMUX: `${socket},1,0` };
    const result = spawnSync("sh", [join(scriptsDir, "uninstall.sh")], { env, encoding: "utf8" });
    expect(result.status).toBe(0);

    const hooks = tmux("show-hooks", "-g").stdout;
    expect(hooks).toContain("client-session-changed[0] display-message user-zero");
    expect(hooks).toContain("after-select-pane[42] display-message user-forty-two");
    expect(hooks).not.toContain("[909]");
    expect(hooks).not.toContain("opensessions");
  }, 20000);
});

describe("toggle.sh after a server start", () => {
  // A fake opensessions-server: binds the port, publishes its pid file, then
  // answers the identity probe after FAKE_READY_MS. FAKE_SLOW_PROBES makes the
  // first identity probes slower than server_alive's 200ms timeout.
  const fakeServer = `#!${process.execPath}
import { appendFileSync, writeFileSync } from "node:fs";
const env = process.env;
const readyAt = Date.now() + Number(env.FAKE_READY_MS ?? 0);
let slowProbes = Number(env.FAKE_SLOW_PROBES ?? 0);
try {
  Bun.serve({
    hostname: "127.0.0.1",
    port: Number(env.OPENSESSIONS_PORT),
    async fetch(req) {
      const path = new URL(req.url).pathname;
      if (path === "/") {
        if (slowProbes > 0) { slowProbes--; await Bun.sleep(400); }
        if (Date.now() < readyAt) return new Response("starting", { status: 503 });
        return new Response("opensessions server " + env.OPENSESSIONS_SERVER_KEY);
      }
      appendFileSync(env.FAKE_LOG, path + " " + (req.headers.get("authorization") ?? "") + "\\n");
      return new Response(null, { status: 204 });
    },
  });
} catch {
  process.exit(1);
}
writeFileSync(env.OPENSESSIONS_PID_FILE, String(process.pid));
setTimeout(() => process.exit(0), Number(env.FAKE_LIFETIME_MS ?? 10000));
`;

  async function fixture() {
    const dir = tempDir("os-toggle-", "/tmp");
    const pluginDir = join(dir, "plugin");
    const bin = join(dir, "fakebin");
    mkdirSync(join(pluginDir, "bin"), { recursive: true });
    mkdirSync(bin);
    writeExecutable(join(pluginDir, "bin/opensessions-server"), fakeServer);
    const port = await freePort();
    const key = `toggletest${process.pid}${port}`;
    const files = {
      log: join(dir, "requests.log"),
      pid: join(dir, "server.pid"),
      token: join(dir, "server.token"),
    };
    writeFileSync(files.log, "");
    writeFileSync(files.token, "secret");
    // Fake tmux: reports a restored sidebar and serves the plugin env.
    writeExecutable(
      join(bin, "tmux"),
      `#!/bin/sh
case "$1 $3" in
  "show-environment OPENSESSIONS_PORT") echo "OPENSESSIONS_PORT=${port}" ;;
  "show-environment OPENSESSIONS_PID_FILE") echo "OPENSESSIONS_PID_FILE=${files.pid}" ;;
  "show-environment OPENSESSIONS_TOKEN_FILE") echo "OPENSESSIONS_TOKEN_FILE=${files.token}" ;;
  "show-environment OPENSESSIONS_DIR") echo "OPENSESSIONS_DIR=${pluginDir}" ;;
  "show-environment "*) exit 1 ;;
esac
case "$1" in
  list-panes) echo opensessions-sidebar ;;
  display-message) echo "/dev/ttys999|main|@1|%1|1" ;;
esac
exit 0
`,
    );
    const env: Record<string, string> = {
      ...(process.env as Record<string, string>),
      PATH: `${bin}:${process.env.PATH}`,
      OPENSESSIONS_SERVER_KEY: key,
      FAKE_LOG: files.log,
    };
    delete env.TMUX;
    const killServer = () => {
      try {
        process.kill(Number(readFileSync(files.pid, "utf8")), "SIGTERM");
      } catch {}
    };
    cleanups.push(killServer);
    cleanups.push(() => {
      for (const suffix of ["server.log", "start.lock"]) {
        rmSync(`/tmp/opensessions.${key}.${suffix}`, { recursive: true, force: true });
      }
    });

    const runToggle = (extraEnv: Record<string, string> = {}, script = "toggle.sh") =>
      new Promise<number>((done) => {
        const child = spawn("sh", [join(scriptsDir, script)], { env: { ...env, ...extraEnv }, stdio: "ignore" });
        child.on("exit", (code) => done(code ?? -1));
      });
    const startServer = (extraEnv: Record<string, string>) =>
      spawn(join(pluginDir, "bin/opensessions-server"), [], {
        env: {
          ...env,
          ...extraEnv,
          OPENSESSIONS_PORT: String(port),
          OPENSESSIONS_PID_FILE: files.pid,
        },
        stdio: "ignore",
      });
    const toggles = () => readFileSync(files.log, "utf8").split("\n").filter((line) => line.startsWith("/toggle"));
    return { runToggle, startServer, toggles, killServer, files };
  }

  async function waitFor(predicate: () => boolean, timeoutMs = 5000): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (!predicate()) {
      if (Date.now() > deadline) throw new Error("timed out");
      await Bun.sleep(25);
    }
  }

  test("toggles an already-running server with the auth token", async () => {
    const { runToggle, startServer, toggles, killServer, files } = await fixture();
    startServer({});
    await waitFor(() => existsSync(files.pid));
    expect(await runToggle()).toBe(0);
    expect(toggles()).toEqual(["/toggle Bearer secret"]);
    killServer();
  }, 15000);

  test("a second press waiting on a cold start does not hide restored sidebars", async () => {
    const { runToggle, toggles, killServer } = await fixture();
    const first = runToggle({ FAKE_READY_MS: "1000" });
    await Bun.sleep(150);
    const second = runToggle();
    expect(await Promise.all([first, second])).toEqual([0, 0]);
    expect(toggles()).toEqual([]);
    killServer();
  }, 15000);

  test("zellij-toggle.sh starts the bin/ server and sends an authenticated toggle", async () => {
    const { runToggle, toggles, killServer } = await fixture();
    expect(await runToggle({}, "zellij-toggle.sh")).toBe(0);
    expect(toggles()).toEqual(["/toggle Bearer secret"]);
    killServer();
  }, 15000);

  test("a slow running server is toggled even when its probe timed out", async () => {
    const { runToggle, startServer, toggles, killServer, files } = await fixture();
    startServer({ FAKE_SLOW_PROBES: "2" });
    await waitFor(() => existsSync(files.pid));
    expect(await runToggle()).toBe(0);
    expect(toggles()).toEqual(["/toggle Bearer secret"]);
    killServer();
  }, 15000);
});

describe("even-horizontal.sh", () => {
  const realTmux = spawnSync("sh", ["-c", "command -v tmux"], { encoding: "utf8" }).stdout.trim();

  // failJoin: shell `case` pattern of join-pane argument lists to reject.
  function privateTmux(failJoin = "") {
    const dir = tempDir("os-evenh-", "/tmp");
    const socket = join(dir, "s");
    const bin = join(dir, "bin");
    const emptyPlugin = join(dir, "plugin");
    mkdirSync(bin);
    mkdirSync(emptyPlugin);
    writeExecutable(
      join(bin, "tmux"),
      `#!/bin/sh
case "$*" in
  ${failJoin || "__never__"}) exit 1 ;;
esac
exec "${realTmux}" -S "${socket}" "$@"
`,
    );
    const tmux = (...args: string[]) =>
      spawnSync(realTmux, ["-S", socket, ...args], { encoding: "utf8" }).stdout.trim();
    cleanups.push(() => tmux("kill-server"));
    spawnSync(realTmux, ["-S", socket, "-f", "/dev/null", "new-session", "-d", "-s", "main", "-x", "200", "-y", "50", "sleep 600"]);
    tmux("set-option", "-g", "allow-rename", "off");
    // No server binary and an unreachable port: ensure-sidebar.sh fails fast.
    tmux("set-environment", "-g", "OPENSESSIONS_PORT", "1");
    tmux("set-environment", "-g", "OPENSESSIONS_DIR", emptyPlugin);
    const windowId = tmux("display-message", "-p", "-t", "main", "#{window_id}");
    const sidebar = tmux("display-message", "-p", "-t", "main", "#{pane_id}");
    tmux("select-pane", "-t", sidebar, "-T", "opensessions-sidebar");
    const a = tmux("split-window", "-h", "-P", "-F", "#{pane_id}", "-t", sidebar, "sleep 600");
    tmux("split-window", "-h", "-P", "-F", "#{pane_id}", "-t", a, "sleep 600");
    tmux("resize-pane", "-t", sidebar, "-x", "30");
    tmux("resize-pane", "-t", a, "-x", "20");
    const run = () =>
      spawnSync("sh", [join(scriptsDir, "even-horizontal.sh"), windowId, a], {
        env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, TMUX: `${socket},1,0` },
        encoding: "utf8",
      });
    const panes = () =>
      tmux("list-panes", "-t", windowId, "-F", "#{pane_id} #{pane_title} #{pane_width} #{pane_left}")
        .split("\n")
        .map((line) => {
          const [id, title, width, left] = line.split(" ");
          return { id, title, width: Number(width), left: Number(left) };
        });
    const stashPanes = () => tmux("list-panes", "-s", "-t", "_os_stash", "-F", "#{pane_id}");
    return { run, panes, stashPanes, sidebar };
  }

  test.skipIf(!realTmux)("spreads the other panes and keeps the sidebar at its edge and width", () => {
    const { run, panes, sidebar } = privateTmux();
    expect(run().status).toBe(0);
    const result = panes();
    expect(result[0]).toMatchObject({ id: sidebar, title: "opensessions-sidebar", width: 30, left: 0 });
    expect(result).toHaveLength(3);
    expect(Math.abs(result[1].width - result[2].width)).toBeLessThanOrEqual(1);
  }, 20000);

  test.skipIf(!realTmux)("rejoins the sidebar when the full-height restore fails", () => {
    const { run, panes, stashPanes, sidebar } = privateTmux('"join-pane -hb -d -f "*');
    expect(run().status).toBe(0);
    expect(panes().map((pane) => pane.id)).toContain(sidebar);
    expect(stashPanes()).not.toContain(sidebar);
  }, 20000);

  test.skipIf(!realTmux)("never strands the sidebar in the stash when every rejoin fails", () => {
    const { run, panes, stashPanes, sidebar } = privateTmux('"join-pane -hb "*');
    expect(run().status).toBe(0);
    expect(panes().map((pane) => pane.id)).not.toContain(sidebar);
    expect(stashPanes()).not.toContain(sidebar);
  }, 20000);
});
