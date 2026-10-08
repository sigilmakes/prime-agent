import assert from "node:assert/strict";
import test from "node:test";
import { groups, planChanges, unauditedTests } from "./ci-plan.mjs";
import { isBlockingViolation, scan } from "./check-test-policy.mjs";

for (const paths of [["README.md"], ["packages/ai/README.md", "LICENSE"], [".github/upstream-workflows/ci.yml"]]) {
	test(`documentation avoids compilation: ${paths.join(",")}`, () => {
		assert.deepEqual(planChanges(paths), { build: false, groups: [] });
	});
}
for (const path of ["new-build-tool", "package-lock.json", "nix/package.nix", ".forgejo/workflows/ci.yml", "packages/ai/src/types.ts", "packages/coding-agent/skills/example/SKILL.md", "packages/coding-agent/src/prompt.md"]) {
	test(`unknown or shared input selects all checks: ${path}`, () => {
		assert.deepEqual(planChanges([path]), { build: true, groups });
	});
}
test("agent changes include runtime consumers", () => {
	assert.deepEqual(planChanges(["packages/agent/src/agent.ts"]), { build: true, groups: ["agent", "session", "catalog"] });
});
test("TUI changes include coding-agent consumers", () => {
	assert.deepEqual(planChanges(["packages/tui/src/editor.ts"]), { build: true, groups: ["tui", "session", "catalog"] });
});
test("renamed source remains selected even when its destination is documentation", () => {
	assert.deepEqual(planChanges(["packages/ai/src/old.ts", "docs/old.md"]), { build: true, groups });
});
test("manual and scheduled runs select all audited checks even without changed code", () => {
	assert.deepEqual(planChanges(["README.md"], true), { build: true, groups });
});
for (const source of ['test.skip("case", () => {});', 'test.only("case", () => {});', 'test("case", () => { if (process.env.API_KEY) return; });']) {
	test(`blocks conditional CI coverage: ${source}`, () => {
		assert.ok(scan(source, "test/example.test.ts").some(isBlockingViolation));
	});
}
for (const source of ['test("case", async () => { await sleep(10); });', 'test("case", { timeout: 1000 }, () => {});', 'test.fails("expected failure", () => {});']) {
	test(`heuristics require review but do not block: ${source}`, () => {
		const findings = scan(source, "test/example.test.ts");
		assert.ok(findings.length > 0);
		assert.equal(findings.some(isBlockingViolation), false);
	});
}

test("changed tests cannot silently escape the audited manifest", () => {
	const manifest = { ai: { package: "ai", files: ["test/known.test.ts"] } };
	assert.deepEqual(unauditedTests(["packages/ai/test/known.test.ts", "scripts/ci.test.mjs", "packages/ai/test/new.test.ts", "prime-agent-runtime/test/test_new.py"], manifest), ["packages/ai/test/new.test.ts", "prime-agent-runtime/test/test_new.py"]);
});

test("manifest membership always selects the changed test's actual group", () => {
	const manifest = { mcp: { package: "agent", files: ["test/special.test.ts"] } };
	assert.deepEqual(planChanges(["packages/agent/test/special.test.ts"], false, manifest), { build: true, groups: ["agent", "session", "mcp", "catalog"] });
});
test("test helpers are not incorrectly required as runnable test entries", () => {
	assert.deepEqual(unauditedTests(["packages/ai/test/helpers.ts", "prime-agent-runtime/test/helpers.py"], {}), []);
});
