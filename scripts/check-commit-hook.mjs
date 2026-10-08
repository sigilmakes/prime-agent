#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gitFixtureEnv } from "./git-fixture-env.mjs";

const hook = join(dirname(fileURLToPath(import.meta.url)), "..", ".husky", "pre-commit");
const cwd = mkdtempSync(join(tmpdir(), "prime-commit-hook-"));
const env = gitFixtureEnv(cwd);
const git = (...args) => {
    const result = spawnSync("git", args, { cwd, encoding: "utf8", env });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout;
};
try {
    git("init");
    const file = join(cwd, "partially staged file.txt");
    writeFileSync(file, "staged\n");
    git("add", "partially staged file.txt");
    writeFileSync(file, "staged\nunstaged\n");
    const index = git("diff", "--cached", "--binary");
    const worktree = readFileSync(file, "utf8");
    const bin = join(cwd, "bin");
    mkdirSync(bin);
    // Stand-in rejects the old mutating check and records the public command.
    writeFileSync(join(bin, "npm"), '#!/bin/sh\nprintf "%s\n" "$*" > npm-call\n[ "$*" = "run check:ci" ] || exit 42\nexit "${CHECK_STATUS:-0}"\n');
    chmodSync(join(bin, "npm"), 0o755);
    for (const status of ["0", "1"]) {
        const result = spawnSync("sh", [hook], {
            cwd, encoding: "utf8", timeout: 9000,
            env: { ...env, PATH: `${bin}:${env.PATH}`, CHECK_STATUS: status },
        });
        assert.equal(result.status, Number(status), result.stderr);
        assert.equal(readFileSync(join(cwd, "npm-call"), "utf8"), "run check:ci\n");
        assert.equal(git("diff", "--cached", "--binary"), index);
        assert.equal(readFileSync(file, "utf8"), worktree);
    }
} finally {
    rmSync(cwd, { recursive: true, force: true });
}
console.log("pre-commit hook: staged hunks and failure propagation passed");
