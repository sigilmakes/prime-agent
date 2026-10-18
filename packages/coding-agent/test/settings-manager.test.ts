import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "fs";
import { homedir, tmpdir } from "os";
import { join } from "path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { SettingsManager } from "../src/core/settings-manager.js";

describe("SettingsManager", () => {
	let testDir: string;
	let agentDir: string;
	let projectDir: string;
	let globalPath: string;
	let projectPath: string;

	beforeEach(() => {
		testDir = mkdtempSync(join(tmpdir(), "prime-settings-"));
		agentDir = join(testDir, "agent");
		projectDir = join(testDir, "project");
		globalPath = join(agentDir, "settings.json");
		projectPath = join(projectDir, ".prime", "agent", "settings.json");
		mkdirSync(agentDir, { recursive: true });
		mkdirSync(join(projectDir, ".prime", "agent"), { recursive: true });
	});

	afterEach(() => {
		if (existsSync(testDir)) rmSync(testDir, { recursive: true });
	});

	const writeSettings = (path: string, value: unknown): void => writeFileSync(path, JSON.stringify(value));
	const readSettings = (path: string): Record<string, unknown> =>
		JSON.parse(readFileSync(path, "utf-8")) as Record<string, unknown>;

	describe("on-disk settings file", () => {
		it("preserves keys it does not own, including ones written after load", async () => {
			writeSettings(globalPath, { theme: "dark", defaultModel: "claude-sonnet" });
			const manager = SettingsManager.create(projectDir, agentDir);

			// A second writer (another process, or the user's editor) adds keys after load.
			writeSettings(globalPath, {
				...readSettings(globalPath),
				enabledModels: ["claude-opus-4-5", "gpt-5.2-codex"],
				shellPath: "/bin/zsh",
				extensions: ["/path/to/extension.ts"],
				defaultThinkingLevel: "low",
			});

			manager.setDefaultThinkingLevel("high");
			manager.setTheme("light");
			await manager.flush();

			expect(readSettings(globalPath)).toEqual({
				theme: "light",
				defaultModel: "claude-sonnet",
				enabledModels: ["claude-opus-4-5", "gpt-5.2-codex"],
				shellPath: "/bin/zsh",
				extensions: ["/path/to/extension.ts"],
				// In-memory changes win for keys the manager owns.
				defaultThinkingLevel: "high",
			});
		});

		it("reloads rewritten settings from disk", async () => {
			writeSettings(globalPath, { theme: "dark", extensions: ["/before.ts"] });
			const manager = SettingsManager.create(projectDir, agentDir);

			writeSettings(globalPath, { theme: "light", extensions: ["/after.ts"], defaultModel: "claude-sonnet" });
			await manager.reload();

			expect(manager.getTheme()).toBe("light");
			expect(manager.getExtensionPaths()).toEqual(["/after.ts"]);
			expect(manager.getDefaultModel()).toBe("claude-sonnet");
		});

		// An unparsable file must never be clobbered: the user's settings would be lost.
		it.each<[string, (manager: SettingsManager) => void, string]>([
			["global", (manager) => manager.setRlmMaxDepth(3), "Global settings not saved"],
			["project", (manager) => manager.setProjectPackages(["npm:test-pkg"]), "Project settings not saved"],
		])("keeps invalid %s settings on disk and reports the failed save", async (scope, mutate, message) => {
			const path = scope === "global" ? globalPath : projectPath;
			const invalid = `{ invalid ${scope} json`;
			writeFileSync(path, invalid);

			const manager = SettingsManager.create(projectDir, agentDir);
			expect(manager.drainErrors().map((entry) => entry.scope)).toEqual([scope]);
			expect(manager.drainErrors()).toEqual([]);

			mutate(manager);
			await manager.flush();

			const errors = manager.drainErrors();
			expect(errors).toHaveLength(1);
			expect(errors[0]?.scope).toBe(scope);
			expect(errors[0]?.error.message).toContain(message);
			expect(readFileSync(path, "utf-8")).toBe(invalid);
		});

		it("keeps the previously loaded values when a reload finds invalid JSON", async () => {
			writeSettings(globalPath, { theme: "dark" });
			const manager = SettingsManager.create(projectDir, agentDir);

			writeFileSync(globalPath, "{ invalid json");
			await manager.reload();

			expect(manager.getTheme()).toBe("dark");
		});

		it("drains errors per scope", () => {
			writeFileSync(globalPath, "{ invalid global json");
			writeFileSync(projectPath, "{ invalid project json");
			const manager = SettingsManager.create(projectDir, agentDir);

			expect(manager.drainErrors("global").map((entry) => entry.scope)).toEqual(["global"]);
			expect(manager.drainErrors().map((entry) => entry.scope)).toEqual(["project"]);
		});

		it("creates the project settings directory only when project settings are written", async () => {
			writeSettings(globalPath, { theme: "dark" });
			rmSync(join(projectDir, ".prime", "agent"), { recursive: true });

			const manager = SettingsManager.create(projectDir, agentDir);
			expect(existsSync(join(projectDir, ".prime", "agent"))).toBe(false);
			expect(manager.getTheme()).toBe("dark");

			manager.setProjectPackages([{ source: "npm:test-pkg" }]);
			await manager.flush();

			expect(existsSync(projectPath)).toBe(true);
		});
	});

	describe("shared settings scope", () => {
		it("#18 applies HOME settings changes immediately and after reload", async () => {
			const sharedDir = join(projectDir, ".prime", "agent");
			writeSettings(projectPath, {
				treeFilterMode: "user-only",
				quietStartup: false,
				terminal: { showImages: true },
			});
			const manager = SettingsManager.create(projectDir, sharedDir);

			manager.setTreeFilterMode("default");
			manager.setQuietStartup(true);
			manager.setShowImages(false);
			expect(manager.getTreeFilterMode()).toBe("default");
			expect(manager.getQuietStartup()).toBe(true);
			expect(manager.getShowImages()).toBe(false);
			expect(manager.getProjectSettings()).toEqual({});
			await manager.flush();
			expect(readSettings(projectPath)).toEqual({
				treeFilterMode: "default",
				quietStartup: true,
				terminal: { showImages: false },
			});

			await manager.reload();
			manager.setQuietStartup(false);
			expect(manager.getQuietStartup()).toBe(false);
			await manager.flush();
			const reopened = SettingsManager.create(projectDir, sharedDir);
			expect(reopened.getTreeFilterMode()).toBe("default");
			expect(reopened.getQuietStartup()).toBe(false);
			expect(reopened.getShowImages()).toBe(false);
			expect(reopened.getProjectSettings()).toEqual({});
			expect(manager.drainErrors()).toEqual([]);
		});

		it.each(["existing directory", "missing file", "missing parents", "file symlink", "dangling file symlink"])(
			"#18 keeps %s aliases in one scope across saves and reopening",
			async (kind) => {
				const sharedDir = join(projectDir, ".prime", "agent");
				const aliasDir = join(testDir, "alias");
				if (kind === "file symlink" || kind === "dangling file symlink") {
					mkdirSync(aliasDir);
					symlinkSync(projectPath, join(aliasDir, "settings.json"));
				} else {
					symlinkSync(projectDir, aliasDir, "dir");
				}
				if (kind === "missing parents") rmSync(sharedDir, { recursive: true });
				if (kind === "existing directory" || kind === "file symlink") {
					writeSettings(projectPath, { quietStartup: false });
				}
				const aliasedAgentDir = kind.includes("file symlink") ? aliasDir : join(aliasDir, ".prime", "agent");
				const manager = SettingsManager.create(projectDir, aliasedAgentDir);
				manager.setQuietStartup(true);
				expect(manager.getQuietStartup()).toBe(true);
				expect(manager.getProjectSettings()).toEqual({});
				await manager.flush();
				expect(readSettings(projectPath).quietStartup).toBe(true);
				expect(SettingsManager.create(projectDir, aliasedAgentDir).getProjectSettings()).toEqual({});
				if (kind.includes("file symlink")) {
					expect(lstatSync(join(aliasDir, "settings.json")).isSymbolicLink()).toBe(true);
				}
				expect(manager.drainErrors()).toEqual([]);
			},
		);

		it.each([
			["packages", "setProjectPackages", "setPackages", "getPackages"],
			["extensions", "setProjectExtensionPaths", "setExtensionPaths", "getExtensionPaths"],
			["skills", "setProjectSkillPaths", "setSkillPaths", "getSkillPaths"],
			["prompts", "setProjectPromptTemplatePaths", "setPromptTemplatePaths", "getPromptTemplatePaths"],
			["themes", "setProjectThemePaths", "setThemePaths", "getThemePaths"],
		] as const)(
			"#18 routes shared project %s writes to the global scope",
			async (field, setProject, setGlobal, get) => {
				const sharedDir = join(projectDir, ".prime", "agent");
				writeSettings(projectPath, { quietStartup: true });
				const manager = SettingsManager.create(projectDir, sharedDir);
				writeSettings(projectPath, { quietStartup: true, untouched: "external" });
				manager[setProject](["project-value"]);
				expect(manager[get]()).toEqual(["project-value"]);
				expect(manager.getGlobalSettings()[field]).toEqual(["project-value"]);
				expect(manager.getProjectSettings()).toEqual({});
				await manager.flush();
				expect(readSettings(projectPath)).toEqual({
					quietStartup: true,
					untouched: "external",
					[field]: ["project-value"],
				});
				manager[setGlobal](["global-value"]);
				expect(manager[get]()).toEqual(["global-value"]);
				await manager.reload();
				expect(manager[get]()).toEqual(["global-value"]);
				expect(SettingsManager.create(projectDir, sharedDir)[get]()).toEqual(["global-value"]);
				expect(manager.getProjectSettings()).toEqual({});
				expect(manager.drainErrors()).toEqual([]);
			},
		);

		it("#18 reports a shared parse error once and never clobbers invalid settings", async () => {
			writeFileSync(projectPath, "{ invalid shared json");
			const manager = SettingsManager.create(projectDir, join(projectDir, ".prime", "agent"));
			expect(manager.drainErrors().map((entry) => entry.scope)).toEqual(["global"]);
			manager.setQuietStartup(true);
			manager.setProjectPackages(["npm:test-pkg"]);
			await manager.flush();
			expect(manager.drainErrors().map((entry) => entry.scope)).toEqual(["global", "global"]);
			expect(readFileSync(projectPath, "utf8")).toBe("{ invalid shared json");
		});
	});

	describe("tree filter preference", () => {
		it.each([undefined, null, "invalid"])("#18 defaults an unset or invalid tree filter (%s) to default", (value) => {
			writeSettings(globalPath, { treeFilterMode: value });
			expect(SettingsManager.create(projectDir, agentDir).getTreeFilterMode()).toBe("default");
		});

		it.each(["default", "no-tools", "user-only", "labeled-only", "all"])(
			"#18 preserves an explicit %s tree filter",
			(value) => {
				writeSettings(globalPath, { treeFilterMode: value });
				expect(SettingsManager.create(projectDir, agentDir).getTreeFilterMode()).toBe(value);
			},
		);
	});

	describe("project vs global precedence", () => {
		it.each<[string, unknown, unknown, (manager: SettingsManager) => unknown, unknown]>([
			[
				"project sessionDir overrides global",
				{ sessionDir: "/global/sessions" },
				{ sessionDir: "./sessions" },
				(manager) => manager.getSessionDir(),
				"./sessions",
			],
			[
				"global sessionDir expands ~",
				{ sessionDir: "~/sessions" },
				{},
				(manager) => manager.getSessionDir(),
				join(homedir(), "sessions"),
			],
			[
				"mcpServers stay global-only",
				{ mcpServers: { shared: { type: "http", url: "https://global.shared/mcp" } } },
				{ mcpServers: { shared: { type: "http", url: "https://project.shared/mcp" } } },
				(manager) => manager.getGlobalMcpServers(),
				{ shared: { type: "http", url: "https://global.shared/mcp" } },
			],
			[
				"idle eviction stays global-only",
				{ idleEvictionMinutes: 60 },
				{ idleEvictionMinutes: 30 },
				(manager) => manager.getIdleEvictionMinutes(),
				60,
			],
			[
				"autonomous limits merge project over global, dropping invalid entries",
				{ autonomous: { maxContinuations: 10, maxTurns: 40 } },
				{ autonomous: { maxContinuations: "unlimited", maxTokens: "one million" } },
				(manager) => manager.getAutonomousLimits(),
				{
					maxContinuations: Number.MAX_SAFE_INTEGER,
					maxTurns: 40,
					maxTokens: undefined,
					timeoutMs: undefined,
				},
			],
		])("%s", (_label, global, project, read, expected) => {
			writeSettings(globalPath, global);
			writeSettings(projectPath, project);

			expect(read(SettingsManager.create(projectDir, agentDir))).toEqual(expected);
		});
	});

	// Park bounds are clamped like the other wait bounds: one week per park at most,
	// and non-finite park settings fall back to the defaults.
	describe("provider park bounds", () => {
		it("clamps park bounds and falls back to defaults on invalid values", () => {
			const wait = SettingsManager.inMemory({
				retry: { provider: { waitForUsage: { maxPauseMs: 365 * 86_400_000, maxParks: 99 } } },
			}).getProviderWaitSettings();
			expect([wait.pauseUntilReset, wait.maxPauseMs, wait.maxParks]).toEqual([true, 7 * 86_400_000, 99]);
			const invalid = SettingsManager.inMemory({
				retry: { provider: { waitForUsage: { maxPauseMs: Number.NaN, maxParks: -1 } } },
			}).getProviderWaitSettings();
			expect([invalid.maxPauseMs, invalid.maxParks]).toEqual([86_400_000, 0]);
		});
	});
});
