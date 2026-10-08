#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gitFixtureEnv } from "./git-fixture-env.mjs";

const hook = join(dirname(fileURLToPath(import.meta.url)), "..", ".husky", "pre-push");
const primary = "ssh://git@mnemosyne.sigilzero.dev/sigilzero/prime-agent.git";
const zero = "0".repeat(40);
const oid = "a".repeat(40);
const update = (ref) => `${ref} ${oid} ${ref} ${zero}`;
const deletion = `(delete) ${zero} refs/heads/old ${oid}`;
const refs = (n) => Array.from({ length: n }, (_, i) => update(`refs/heads/b${i}`)).join("\n");
const cases = [
    ["primary branch", "origin", primary, update("refs/heads/main"), 0],
    ["primary tag", "origin", primary, update("refs/tags/v1"), 0],
    ["up-to-date", "origin", primary, "", 0],
    ["ten refs", "origin", primary, refs(10), 0],
    ["eleven refs", "origin", primary, refs(11), 1],
    ["deletion", "origin", primary, deletion, 1],
    ["tracking refs", "origin", primary, update("refs/remotes/origin/main"), 1],
    ["malformed", "origin", primary, "bad input", 1],
    ["origin renamed transport", "origin", "git@private-alias:repo.git", deletion, 1],
    ["primary by URL", primary, primary, deletion, 1],
    ["primary different name", "backup", primary, deletion, 1],
    ["scratch mirror", "scratch", "file:///tmp/scratch.git", refs(11), 0],
    ["scratch deletion", "scratch", "/tmp/scratch.git", deletion, 0],
    ["lookalike host", "scratch", "https://github.com.example/repo.git", refs(11), 0],
    ["github remote alias", "github", "git@alias:repo.git", "", 1],
    ["upstream remote alias", "upstream", "/tmp/upstream.git", "", 1],
];
for (const url of ["git@github.com:a/b", "github.com:a/b", "https://token@github.com:443/a/b", "ssh://git@ssh.github.com:443/a/b", "https://WWW.GITHUB.COM./a/b", "git@GITHUB.COM.:a/b"]) {
    cases.push([`github ${url}`, "other", url, update("refs/heads/main"), 1]);
    cases.push([`github override ${url}`, "other", url, "", 1, "1"]);
}
for (const value of ["", "0", "true", "1"]) {
    cases.push([`override ${value}`, "origin", primary, deletion, value === "1" ? 0 : 1, value]);
}
cases.push(["override malformed", "origin", primary, "bad", 1, "1"]);
const cwd = mkdtempSync(join(tmpdir(), "prime-push-guard-"));
const env = gitFixtureEnv(cwd);
try {
    assert.equal(spawnSync("git", ["init", cwd], { encoding: "utf8", env }).status, 0);
    for (const [name, remote, url, input, expected, override = ""] of cases) {
        const result = spawnSync("sh", [hook, remote, url], {
            cwd, input, encoding: "utf8", timeout: 9000,
            env: { ...env, PRIME_AGENT_ALLOW_MIRROR_PUSH: override },
        });
        assert.equal(result.status, expected, `${name}: ${result.stderr}`);
    }
    const git = (...args) => spawnSync("git", args, {
        cwd, encoding: "utf8", timeout: 9000,
        env,
    });
    const ok = (...args) => {
        const result = git(...args);
        assert.equal(result.status, 0, result.stderr);
    };
    ok("init", "--bare", "remote.git");
    ok("config", "user.name", "Hook Test");
    ok("config", "user.email", "hook@example.invalid");
    mkdirSync(join(cwd, ".husky"));
    mkdirSync(join(cwd, "scripts"));
    copyFileSync(hook, join(cwd, ".husky", "pre-push"));
    copyFileSync(join(dirname(hook), "..", "scripts", "pre-push-guard.sh"), join(cwd, "scripts", "pre-push-guard.sh"));
    ok("config", "core.hooksPath", ".husky");
    writeFileSync(join(cwd, "file"), "test\n");
    ok("add", "file");
    ok("commit", "-m", "fixture");
    ok("remote", "add", "origin", join(cwd, "remote.git"));
    ok("push", "origin", "HEAD:refs/heads/topic");
    assert.notEqual(git("push", "origin", "--delete", "topic").status, 0);
    ok("remote", "add", "scratch", join(cwd, "remote.git"));
    ok("push", "scratch", "--delete", "topic");
} finally {
    rmSync(cwd, { recursive: true, force: true });
}
console.log(`pre-push guard: ${cases.length} cases passed`);
