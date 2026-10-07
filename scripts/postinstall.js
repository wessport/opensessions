// Download the prebuilt opensessions binaries for this platform into bin/.
// Mirrors integrations/tmux-plugin/scripts/install-binaries.sh: honors
// OPENSESSIONS_RELEASE_BASE / OPENSESSIONS_RELEASE_VERSION /
// OPENSESSIONS_SKIP_BINARY_DOWNLOAD, verifies the release .sha256 before
// extracting, and writes the same version/source markers so the TPM loader
// accepts the bundle without downloading it again.
const crypto = require("crypto");
const fs = require("fs");
const http = require("http");
const https = require("https");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const REPO_RELEASE_BASE = "https://github.com/ataraxy-labs/opensessions/releases/download";
const EXECUTABLE_NAMES = ["opensessions-sidebar", "opensessions-server", "lazydiff"];
const MAX_REDIRECTS = 5;

function targetTriple(platform = os.platform(), arch = os.arch()) {
  if (platform === "darwin" && arch === "arm64") return "aarch64-apple-darwin";
  if (platform === "darwin" && arch === "x64") return "x86_64-apple-darwin";
  if (platform === "linux" && arch === "x64") return "x86_64-unknown-linux-gnu";
  if (platform === "linux" && arch === "arm64") return "aarch64-unknown-linux-gnu";
  throw new Error(`Unsupported platform: ${platform}-${arch}`);
}

function artifactName(triple) {
  return `opensessions-sidebar-${triple}.tar.gz`;
}

function releaseBase(env = process.env) {
  return (env.OPENSESSIONS_RELEASE_BASE || REPO_RELEASE_BASE).replace(/\/+$/, "");
}

function releaseUrl(version, triple, base = REPO_RELEASE_BASE) {
  return `${base.replace(/\/+$/, "")}/v${version}/${artifactName(triple)}`;
}

// Resolve redirects before opening the destination, so a redirect never races
// a cleanup of the same path (the old code unlinked dest after the redirected
// download had already started writing it).
function download(url, dest, redirectsLeft = MAX_REDIRECTS) {
  return new Promise((resolve, reject) => {
    const parsed = new URL(url);
    const client = parsed.protocol === "http:" ? http : https;
    const request = client.get(parsed, (response) => {
      const status = response.statusCode ?? 0;
      if (status >= 300 && status < 400 && response.headers.location) {
        response.resume();
        if (redirectsLeft <= 0) {
          reject(new Error(`Too many redirects: ${url}`));
          return;
        }
        const next = new URL(response.headers.location, parsed);
        if (parsed.protocol === "https:" && next.protocol !== "https:") {
          reject(new Error(`Refusing insecure redirect: ${url} -> ${next}`));
          return;
        }
        download(next.toString(), dest, redirectsLeft - 1).then(resolve, reject);
        return;
      }

      if (status !== 200) {
        response.resume();
        reject(new Error(`Download failed: ${status} ${url}`));
        return;
      }

      const file = fs.createWriteStream(dest);
      const fail = (err) => {
        file.destroy();
        fs.rm(dest, { force: true }, () => reject(err));
      };
      response.on("error", fail);
      file.on("error", fail);
      file.on("finish", () => file.close((err) => (err ? fail(err) : resolve())));
      response.pipe(file);
    });
    request.on("error", reject);
  });
}

function parseChecksum(text) {
  const sum = text.trim().split(/\s+/, 1)[0] ?? "";
  if (!/^[0-9a-fA-F]{64}$/.test(sum)) throw new Error("opensessions: invalid release checksum");
  return sum.toLowerCase();
}

function sha256File(file) {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

async function install({
  version = require("../package.json").version,
  triple = targetTriple(),
  binDir = path.join(__dirname, "..", "bin"),
  env = process.env,
} = {}) {
  if (env.OPENSESSIONS_SKIP_BINARY_DOWNLOAD === "1") return;
  version = env.OPENSESSIONS_RELEASE_VERSION || version;
  const base = releaseBase(env);
  const url = releaseUrl(version, triple, base);
  const work = fs.mkdtempSync(path.join(os.tmpdir(), "opensessions-postinstall-"));
  try {
    const tarball = path.join(work, artifactName(triple));
    const checksum = `${tarball}.sha256`;
    await download(url, tarball);
    await download(`${url}.sha256`, checksum);
    const expected = parseChecksum(fs.readFileSync(checksum, "utf8"));
    if (sha256File(tarball) !== expected) {
      throw new Error("opensessions: release checksum verification failed");
    }

    const stage = path.join(work, "stage");
    fs.mkdirSync(stage);
    execFileSync("tar", ["-xzf", tarball, "-C", stage]);
    for (const name of EXECUTABLE_NAMES) {
      const file = path.join(stage, name);
      if (!fs.existsSync(file)) throw new Error(`opensessions: release bundle is missing ${name}`);
      fs.chmodSync(file, 0o755);
    }
    fs.writeFileSync(path.join(stage, ".opensessions-version"), `${version}\n`);
    fs.writeFileSync(path.join(stage, ".opensessions-release-source"), `${base}:v${version}\n`);

    // Publish only the validated bundle. Copy next to the destination, then
    // rename, so a running binary is replaced atomically, never truncated.
    fs.mkdirSync(binDir, { recursive: true });
    for (const name of [...EXECUTABLE_NAMES, ".opensessions-version", ".opensessions-release-source"]) {
      const temporary = path.join(binDir, `.${name}.${process.pid}.tmp`);
      fs.copyFileSync(path.join(stage, name), temporary);
      fs.chmodSync(temporary, fs.statSync(path.join(stage, name)).mode);
      fs.renameSync(temporary, path.join(binDir, name));
    }
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
}

if (require.main === module) {
  install().catch((err) => {
    console.error(err);
    process.exit(1);
  });
}

module.exports = { artifactName, download, install, parseChecksum, releaseBase, releaseUrl, targetTriple };
