import { afterAll, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { artifactName, download, install, releaseBase, releaseUrl, targetTriple } from "./postinstall";

const tempDirs: string[] = [];
afterAll(() => {
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "opensessions-postinstall-test-"));
  tempDirs.push(dir);
  return dir;
}

function sha256(bytes: Uint8Array): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

// Release server whose artifact URLs redirect (like GitHub release downloads).
function releaseServer(files: Record<string, Uint8Array | string>) {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(req) {
      const { pathname } = new URL(req.url);
      if (pathname.startsWith("/releases/")) {
        return new Response(null, { status: 302, headers: { location: pathname.replace("/releases/", "/objects/") } });
      }
      const body = files[pathname.replace("/objects/", "")];
      return body === undefined ? new Response("missing", { status: 404 }) : new Response(body);
    },
  });
  return { base: `http://127.0.0.1:${server.port}/releases`, stop: () => server.stop(true) };
}

function bundle(): Uint8Array {
  const dir = tempDir();
  const payload = join(dir, "payload");
  mkdirSync(payload);
  for (const name of ["opensessions-sidebar", "opensessions-server", "lazydiff"]) {
    writeFileSync(join(payload, name), `#!/bin/sh\necho ${name}\n`);
    chmodSync(join(payload, name), 0o644);
  }
  const tarball = join(dir, "bundle.tar.gz");
  expect(spawnSync("tar", ["-czf", tarball, "-C", payload, "."]).status).toBe(0);
  return readFileSync(tarball);
}

describe("opensessions-sidebar postinstall", () => {
  test("maps supported npm platforms to Rust target triples", () => {
    expect(targetTriple("darwin", "arm64")).toBe("aarch64-apple-darwin");
    expect(targetTriple("darwin", "x64")).toBe("x86_64-apple-darwin");
    expect(targetTriple("linux", "x64")).toBe("x86_64-unknown-linux-gnu");
    expect(targetTriple("linux", "arm64")).toBe("aarch64-unknown-linux-gnu");
    expect(() => targetTriple("win32", "x64")).toThrow("Unsupported platform: win32-x64");
  });

  test("builds release artifact URL from package version", () => {
    expect(artifactName("aarch64-apple-darwin")).toBe("opensessions-sidebar-aarch64-apple-darwin.tar.gz");
    expect(releaseUrl("0.2.0-alpha.5", "aarch64-apple-darwin")).toBe(
      "https://github.com/ataraxy-labs/opensessions/releases/download/v0.2.0-alpha.5/opensessions-sidebar-aarch64-apple-darwin.tar.gz",
    );
  });

  test("honors OPENSESSIONS_RELEASE_BASE", () => {
    expect(releaseBase({})).toBe("https://github.com/ataraxy-labs/opensessions/releases/download");
    expect(releaseBase({ OPENSESSIONS_RELEASE_BASE: "https://example.test/fork/releases/download/" })).toBe(
      "https://example.test/fork/releases/download",
    );
  });

  test("a redirected download keeps the complete file", async () => {
    const body = new Uint8Array(512 * 1024).map((_, index) => index % 251);
    const server = releaseServer({ "big.bin": body });
    try {
      const dest = join(tempDir(), "big.bin");
      for (let attempt = 0; attempt < 20; attempt++) {
        await download(`${server.base}/big.bin`, dest);
        await Bun.sleep(5); // the old cleanup unlinked dest shortly after
        expect(existsSync(dest)).toBe(true);
        expect(sha256(readFileSync(dest))).toBe(sha256(body));
      }
    } finally {
      server.stop();
    }
  });

  test("installs a checksum-verified bundle with release markers", async () => {
    const triple = "aarch64-apple-darwin";
    const tarball = bundle();
    const name = `v1.2.3/${artifactName(triple)}`;
    const server = releaseServer({ [name]: tarball, [`${name}.sha256`]: `${sha256(tarball)}  ${artifactName(triple)}\n` });
    try {
      const binDir = join(tempDir(), "bin");
      await install({ version: "0.0.0", triple, binDir, env: { OPENSESSIONS_RELEASE_BASE: server.base, OPENSESSIONS_RELEASE_VERSION: "1.2.3" } });
      for (const exe of ["opensessions-sidebar", "opensessions-server", "lazydiff"]) {
        expect(statSync(join(binDir, exe)).mode & 0o111).toBe(0o111);
      }
      expect(readFileSync(join(binDir, ".opensessions-version"), "utf8")).toBe("1.2.3\n");
      expect(readFileSync(join(binDir, ".opensessions-release-source"), "utf8")).toBe(`${server.base}:v1.2.3\n`);
      expect(spawnSync("ls", ["-A", binDir], { encoding: "utf8" }).stdout.trim().split("\n").sort()).toEqual([
        ".opensessions-release-source",
        ".opensessions-version",
        "lazydiff",
        "opensessions-server",
        "opensessions-sidebar",
      ]);
    } finally {
      server.stop();
    }
  });

  test("rejects a bundle whose checksum does not match without touching bin/", async () => {
    const triple = "aarch64-apple-darwin";
    const name = `v1.2.3/${artifactName(triple)}`;
    const server = releaseServer({ [name]: bundle(), [`${name}.sha256`]: `${"0".repeat(64)}  x\n` });
    try {
      const binDir = join(tempDir(), "bin");
      await expect(
        install({ version: "1.2.3", triple, binDir, env: { OPENSESSIONS_RELEASE_BASE: server.base } }),
      ).rejects.toThrow("checksum verification failed");
      expect(existsSync(binDir)).toBe(false);
    } finally {
      server.stop();
    }
  });

  test("fails when the release checksum is missing", async () => {
    const triple = "aarch64-apple-darwin";
    const server = releaseServer({ [`v1.2.3/${artifactName(triple)}`]: bundle() });
    try {
      await expect(
        install({ version: "1.2.3", triple, binDir: join(tempDir(), "bin"), env: { OPENSESSIONS_RELEASE_BASE: server.base } }),
      ).rejects.toThrow("Download failed: 404");
    } finally {
      server.stop();
    }
  });
});
