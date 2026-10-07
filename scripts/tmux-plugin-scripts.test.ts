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
