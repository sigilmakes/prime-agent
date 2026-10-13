import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { resolveRuntimeIdentity } from "../src/core/kernel/bootstrap.js";

let directory: string | undefined;

afterEach(() => {
	vi.unstubAllEnvs();
	if (directory) rmSync(directory, { recursive: true, force: true });
});

it("resolves and fingerprints the Python source beside a standalone executable", async () => {
	directory = mkdtempSync(join(tmpdir(), "prime-binary-runtime-"));
	vi.stubEnv("PI_PACKAGE_DIR", directory);
	const source = join(directory, "prime-agent-runtime");
	mkdirSync(join(source, "src/rlm"), { recursive: true });
	const files = {
		"pyproject.toml": '[project]\nname = "prime-agent-runtime"\n',
		"src/rlm/repl.py": "VALUE = 1\n",
		"uv.lock": "version = 1\n",
	};
	const expected = createHash("sha256");
	for (const [name, content] of Object.entries(files)) {
		writeFileSync(join(source, name), content);
		expected.update(name).update("\0").update(content).update("\0");
	}
	const initial = await resolveRuntimeIdentity();
	expect(initial).toBe(`sha256:${expected.digest("hex")}`);
	writeFileSync(join(source, "uv.lock"), "version = 2\n");
	const lockChanged = await resolveRuntimeIdentity();
	expect(lockChanged).not.toBe(initial);
	writeFileSync(join(source, "src/rlm/repl.py"), "VALUE = 2\n");
	expect(await resolveRuntimeIdentity()).not.toBe(lockChanged);
	rmSync(join(source, "uv.lock"));
	await expect(resolveRuntimeIdentity()).rejects.toThrow("missing uv.lock");
});
