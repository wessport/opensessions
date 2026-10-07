import { afterAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Shared vectors; keep in sync with the Rust test
// explicit_keys_map_to_the_same_ports_as_shell_and_typescript in
// packages/runtime-rs/src/shared.rs.
const VECTORS: Array<[string, number]> = [
  ["123", 22123],
  ["00123", 22123],
  ["  123\n", 22123],
  ["123456", 25456],
  ["999999999999999", 41999],
  ["1234567890123456", 41896],
  ["12345678abcdef00", 41896],
  ["DEADBEEF", 30559],
  ["work", 23687],
  ["deadbeefzz", 29937],
];

const fakeBin = mkdtempSync(join(tmpdir(), "opensessions-server-key-"));
// Keep the Amp plugin's load-time log line out of the real plugin log.
process.env.OPENSESSIONS_AMP_PLUGIN_LOG = join(fakeBin, "amp-plugin.log");
const { portForServerKey: ampPortForServerKey } = await import("../integrations/amp/opensessions");
const { portForServerKey: piPortForServerKey } = await import("../integrations/pi-extension/opensessions-runtime");

const scriptsDir = resolve(import.meta.dir, "../integrations/tmux-plugin/scripts");
// server-common.sh queries the tmux global environment; never reach a real
// tmux server from this test.
writeFileSync(join(fakeBin, "tmux"), "#!/bin/sh\nexit 1\n");
chmodSync(join(fakeBin, "tmux"), 0o755);

afterAll(() => rmSync(fakeBin, { recursive: true, force: true }));

function shellEndpoint(shell: string, key: string): { key: string; port: number; tokenFile: string } {
  const env: Record<string, string> = {
    ...(process.env as Record<string, string>),
    PATH: `${fakeBin}:${process.env.PATH}`,
    OPENSESSIONS_SERVER_KEY: key,
    SCRIPT_DIR: scriptsDir,
  };
  delete env.TMUX;
  const result = spawnSync(
    shell,
    ["-c", '. "$SCRIPT_DIR/server-common.sh"; printf "%s|%s|%s" "$SERVER_KEY" "$PORT" "$TOKEN_FILE"'],
    { env, encoding: "utf8" },
  );
  expect(result.status).toBe(0);
  const [serverKey, port, tokenFile] = result.stdout.split("|");
  return { key: serverKey, port: Number(port), tokenFile };
}

const shells = ["sh", "dash", "bash"].filter(
  (shell) => spawnSync("sh", ["-c", `command -v ${shell}`]).status === 0,
);

describe("OPENSESSIONS_SERVER_KEY port resolution", () => {
  test.each(VECTORS)("Amp and Pi map %p to %p", (key, port) => {
    expect(ampPortForServerKey(key)).toBe(port);
    expect(piPortForServerKey(key)).toBe(port);
  });

  test("blank keys have no derived port", () => {
    expect(ampPortForServerKey("  ")).toBeNull();
    expect(piPortForServerKey("")).toBeNull();
  });

  for (const shell of shells) {
    test.each(VECTORS)(`${shell} server-common.sh maps %p to %p without aborting`, (key, port) => {
      const endpoint = shellEndpoint(shell, key);
      expect(endpoint.port).toBe(port);
      expect(endpoint.key).toBe(key.trim());
      expect(endpoint.tokenFile).toBe(`/tmp/opensessions.${key.trim()}.token`);
    });
  }
});
