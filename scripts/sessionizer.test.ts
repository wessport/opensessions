import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const sessionizer = resolve(import.meta.dir, "../apps/tui/scripts/sessionizer.sh");
const tempDirs: string[] = [];

afterEach(() => {
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "opensessions-sessionizer-"));
  tempDirs.push(root);
  const bin = join(root, "bin");
  const search = join(root, "search");
  mkdirSync(bin);
  mkdirSync(search);

  writeFileSync(
    join(bin, "fzf"),
    `#!/bin/sh
cat > "$FZF_INPUT"
printf '%s\\n%s\\n' "$FZF_QUERY" "$FZF_MATCH"
printf '%s\\n' "$@" > "$FZF_ARGS"
`,
  );
  writeFileSync(
    join(bin, "tmux"),
    `#!/bin/sh
printf '%s\\n' "$*" >> "$TMUX_LOG"
[ "$1" = has-session ] && [ "$TMUX_HAS_SESSION" = 1 ] && exit 0
[ "$1" = has-session ] && exit 1
exit 0
`,
  );
  chmodSync(join(bin, "fzf"), 0o755);
  chmodSync(join(bin, "tmux"), 0o755);

  const files = {
    fzfInput: join(root, "fzf-input"),
    fzfArgs: join(root, "fzf-args"),
    tmuxLog: join(root, "tmux-log"),
  };
  writeFileSync(files.tmuxLog, "");
  const env = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    SESSIONIZER_DIR: search,
    SESSIONIZER_MAXDEPTH: "3",
    FZF_INPUT: files.fzfInput,
    FZF_ARGS: files.fzfArgs,
    TMUX_LOG: files.tmuxLog,
  };
  return { root, search, files, env };
}

describe("sessionizer path selection", () => {
  test("prefers a typed valid directory over the highlighted match", () => {
    const { root, search, files, env } = fixture();
    const typed = join(root, "typed");
    const match = join(search, "highlighted");
    mkdirSync(typed);
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: { ...env, FZF_QUERY: typed, FZF_MATCH: match },
    });

    expect(result.exitCode).toBe(0);
    expect(readFileSync(files.fzfArgs, "utf8")).toContain("--print-query\n");
    expect(readFileSync(files.tmuxLog, "utf8")).toContain(`new-session -d -s typed -c ${typed}`);
    expect(readFileSync(files.tmuxLog, "utf8")).not.toContain(`-c ${match}`);
  });

  test("creates the selected directory with an explicitly entered session name", () => {
    const { search, files, env } = fixture();
    const match = join(search, "project-directory");
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: { ...env, FZF_QUERY: "", FZF_MATCH: match },
      stdin: Buffer.from("descriptive session name\n"),
    });

    expect(result.exitCode).toBe(0);
    const tmuxLog = readFileSync(files.tmuxLog, "utf8");
    expect(tmuxLog).toContain("has-session -t =descriptive session name");
    expect(tmuxLog).toContain(`new-session -d -s descriptive session name -c ${match}`);
    expect(tmuxLog).toContain("switch-client -t descriptive session name");
  });

  test("normalizes tmux separators in an explicitly entered session name", () => {
    const { search, files, env } = fixture();
    const match = join(search, "project-directory");
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: { ...env, FZF_QUERY: "", FZF_MATCH: match },
      stdin: Buffer.from("team:project.v2\n"),
    });

    expect(result.exitCode).toBe(0);
    expect(readFileSync(files.tmuxLog, "utf8")).toContain(
      `new-session -d -s team_project_v2 -c ${match}`,
    );
  });

  test("expands a typed home-relative directory", () => {
    const { root, search, files, env } = fixture();
    const typed = join(root, "repos", "project");
    const match = join(search, "highlighted");
    mkdirSync(typed, { recursive: true });
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: {
        ...env,
        HOME: root,
        FZF_QUERY: "~/repos/project",
        FZF_MATCH: match,
      },
    });

    expect(result.exitCode).toBe(0);
    expect(readFileSync(files.tmuxLog, "utf8")).toContain(`new-session -d -s project -c ${typed}`);
    expect(readFileSync(files.tmuxLog, "utf8")).not.toContain(`-c ${match}`);
  });

  test("excludes hidden directories from fzf candidates", () => {
    const { search, files, env } = fixture();
    const visible = join(search, "visible");
    const hidden = join(search, ".hidden");
    mkdirSync(visible);
    mkdirSync(join(hidden, "child"), { recursive: true });

    const result = Bun.spawnSync([sessionizer], {
      env: { ...env, FZF_QUERY: "", FZF_MATCH: "" },
    });

    expect(result.exitCode).toBe(0);
    const candidates = readFileSync(files.fzfInput, "utf8");
    expect(candidates).toContain(`${visible}\n`);
    expect(candidates).not.toContain(hidden);
    expect(readFileSync(files.tmuxLog, "utf8")).toBe("");
  });

  test("uses a non-empty match when the query is not a valid directory", () => {
    const { root, search, files, env } = fixture();
    const match = join(search, "matched");
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: { ...env, FZF_QUERY: join(root, "missing"), FZF_MATCH: match },
    });

    expect(result.exitCode).toBe(0);
    expect(readFileSync(files.tmuxLog, "utf8")).toContain(`new-session -d -s matched -c ${match}`);
  });

  test("switches to an existing same-named session instead of duplicating it", () => {
    const { search, files, env } = fixture();
    const match = join(search, "existing");
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: {
        ...env,
        FZF_QUERY: "existing",
        FZF_MATCH: match,
        TMUX_HAS_SESSION: "1",
      },
    });

    expect(result.exitCode).toBe(0);
    const tmuxLog = readFileSync(files.tmuxLog, "utf8");
    expect(tmuxLog).toContain("has-session -t =existing");
    expect(tmuxLog).toContain("switch-client -t existing");
    expect(tmuxLog).not.toContain("new-session");
  });

  test("switches to an existing explicitly named session instead of duplicating it", () => {
    const { search, files, env } = fixture();
    const match = join(search, "project-directory");
    mkdirSync(match);

    const result = Bun.spawnSync([sessionizer], {
      env: {
        ...env,
        FZF_QUERY: "",
        FZF_MATCH: match,
        TMUX_HAS_SESSION: "1",
      },
      stdin: Buffer.from("existing custom name\n"),
    });

    expect(result.exitCode).toBe(0);
    const tmuxLog = readFileSync(files.tmuxLog, "utf8");
    expect(tmuxLog).toContain("has-session -t =existing custom name");
    expect(tmuxLog).toContain("switch-client -t existing custom name");
    expect(tmuxLog).not.toContain("new-session");
  });

  test("preserves colon-separated roots and max depth without creating an empty selection", () => {
    const { root, search, files, env } = fixture();
    const secondRoot = join(root, "second");
    const firstLevel = join(search, "first-level");
    const tooDeep = join(firstLevel, "too-deep");
    const secondCandidate = join(secondRoot, "candidate");
    mkdirSync(tooDeep, { recursive: true });
    mkdirSync(secondCandidate, { recursive: true });

    const result = Bun.spawnSync([sessionizer], {
      env: {
        ...env,
        SESSIONIZER_DIR: `${search}:${secondRoot}`,
        SESSIONIZER_MAXDEPTH: "1",
        FZF_QUERY: "",
        FZF_MATCH: "",
      },
    });

    expect(result.exitCode).toBe(0);
    const candidates = readFileSync(files.fzfInput, "utf8");
    expect(candidates).toContain(`${firstLevel}\n`);
    expect(candidates).toContain(`${secondCandidate}\n`);
    expect(candidates).not.toContain(tooDeep);
    expect(readFileSync(files.tmuxLog, "utf8")).toBe("");
  });
});
