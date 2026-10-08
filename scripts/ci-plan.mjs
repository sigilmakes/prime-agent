import { execFileSync } from "node:child_process";
import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(import.meta.dirname, "..");
export const groups = ["ai", "agent", "tui", "session", "mcp", "catalog"];

export function planChanges(paths, full = false, manifest = {}) {
	const selected = new Set();
	for (const [group, spec] of Object.entries(manifest)) {
		if (!groups.includes(group)) throw new Error(`Unknown manifest test group: ${group}`);
		if (spec.files.some((file) => paths.includes(`packages/${spec.package}/${file}`))) selected.add(group);
	}
	for (const path of paths) {
		// Markdown used as a prompt or skill is executable input, not documentation.
		if (path.startsWith(".github/upstream-workflows/")) continue;
		if ((path.endsWith(".md") && !/(?:^|\/)(?:src|skills|prompts)\//.test(path)) || path === "LICENSE") continue;
		if (path.startsWith("packages/ai/")) groups.forEach((group) => selected.add(group));
		else if (path.startsWith("packages/agent/")) ["agent", "session", "catalog"].forEach((group) => selected.add(group));
		else if (path.startsWith("packages/tui/")) ["tui", "session", "catalog"].forEach((group) => selected.add(group));
		else if (path.startsWith("packages/coding-agent/src/core/mcp")) ["mcp", "session", "catalog"].forEach((group) => selected.add(group));
		else groups.forEach((group) => selected.add(group));
	}
	if (full) groups.forEach((group) => selected.add(group));
	return { build: selected.size > 0, groups: groups.filter((group) => selected.has(group)) };
}

export function unauditedTests(paths, manifest) {
	const covered = new Set(["scripts/ci.test.mjs"]);
	for (const group of Object.values(manifest)) {
		for (const file of group.files) covered.add(`packages/${group.package}/${file}`);
	}
	return paths.filter((path) =>
		(/(?:^|\/)test_[^/]+\.py$/.test(path) || /\.(?:test|spec)\.[cm]?[jt]sx?$/.test(path)) && !covered.has(path),
	);
}

function git(args) {
	return execFileSync("git", args, { cwd: root, encoding: "utf8" }).trim();
}

function main() {
	const full = process.env.GITHUB_EVENT_NAME !== "push";
	const branch = process.env.GITHUB_REF_NAME;
	const before = process.env.BEFORE_SHA;
	let base;
	if (branch !== "main") base = git(["merge-base", "HEAD", "origin/main"]);
	else if (before && /^[0-9a-f]{40,64}$/.test(before) && !/^0+$/.test(before)) base = git(["rev-parse", "--verify", `${before}^{commit}`]);
	else base = git(["rev-parse", "HEAD^"]);
	const paths = git(["diff", "--name-only", "--no-renames", "-z", base, "HEAD"]).split("\0").filter(Boolean);
	const manifest = JSON.parse(readFileSync(resolve(root, "nix/test-groups.json"), "utf8"));
	const missing = unauditedTests(paths.filter((path) => existsSync(resolve(root, path))), manifest);
	if (missing.length > 0) throw new Error(`Changed tests need an offline audit and a nix/test-groups.json entry: ${missing.join(", ")}`);
	const plan = planChanges(paths, full, manifest);
	const result = { ...plan, base, paths };
	console.log(JSON.stringify(result, null, 2));
	if (process.env.GITHUB_OUTPUT) {
		appendFileSync(process.env.GITHUB_OUTPUT, `build=${plan.build}\ngroups=${plan.groups.join(" ")}\nbase=${base}\n`);
	}
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
