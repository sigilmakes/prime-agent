import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fauxAssistantMessage, getApiProvider, registerFauxProvider } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ENV_AGENT_DIR } from "../src/config.js";
import { AGENT_MESSAGE_SKILL_NAME, type AgentSessionMessageController } from "../src/core/agent-messages.js";
import { AGENT_OBSERVE_SKILL_NAME, type AgentObserveController } from "../src/core/agent-observe.js";
import { createAgentSessionFromServices, createAgentSessionServices } from "../src/core/agent-session-services.js";
import { AuthStorage } from "../src/core/auth-storage.js";
import { McpConnectionStore } from "../src/core/mcp/connection-store.js";
import { McpManager } from "../src/core/mcp/mcp-manager.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createSyntheticSourceInfo } from "../src/core/source-info.js";

describe("createAgentSessionFromServices", () => {
	const cleanupPaths: string[] = [];
	const unregisters: Array<() => void> = [];

	afterEach(() => {
		vi.unstubAllGlobals();
		vi.unstubAllEnvs();
		while (unregisters.length > 0) {
			unregisters.pop()?.();
		}
		while (cleanupPaths.length > 0) {
			const path = cleanupPaths.pop();
			if (path && existsSync(path)) {
				rmSync(path, { recursive: true, force: true });
			}
		}
	});

	it("enables CLI login reuse only for default services storage", async () => {
		const tempDir = join(tmpdir(), `pi-default-services-auth-${Date.now()}`);
		mkdirSync(tempDir, { recursive: true });
		cleanupPaths.push(tempDir);
		vi.stubEnv("HOME", tempDir);
		vi.stubEnv(ENV_AGENT_DIR, "");
		const injected = AuthStorage.inMemory();
		for (const options of [{}, { agentDir: join(tempDir, "custom") }, { authStorage: injected }]) {
			const services = await createAgentSessionServices({
				cwd: tempDir,
				...options,
				telemetryDisabled: true,
				resourceLoaderOptions: {
					noExtensions: true,
					noSkills: true,
					noPromptTemplates: true,
					noThemes: true,
					noContextFiles: true,
				},
			});
			expect(services.modelRegistry.authStorage).toBe(services.authStorage);
			expect(services.authStorage.getPrimeCliConfigPath()).toBe(
				"agentDir" in options || "authStorage" in options ? undefined : join(tempDir, ".prime", "config.json"),
			);
			if ("authStorage" in options) expect(services.authStorage).toBe(injected);
		}
	});

	it("issue #4: legacy reporting opt-ins cannot upload persisted sessions or create reporting state", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pi-reporting-removal-"));
		cleanupPaths.push(tempDir);
		vi.stubEnv("HOME", tempDir);
		vi.stubEnv(ENV_AGENT_DIR, tempDir);
		vi.stubEnv("PI_OFFLINE", "");
		vi.stubEnv("DO_NOT_TRACK", "0");
		vi.stubEnv("PRIME_AGENT_TELEMETRY", "1");
		vi.stubEnv("PRIME_AGENT_TRACES_BASE_URL", "https://reports.invalid");
		const fetchSpy = vi.fn(
			async (_input: string | URL | Request, _init?: RequestInit) => new Response("{}", { status: 200 }),
		);
		vi.stubGlobal("fetch", fetchSpy);
		writeFileSync(
			join(tempDir, "settings.json"),
			JSON.stringify({
				agentTraces: { enabled: true },
				telemetry: { enabled: true, noticeShown: true },
			}),
		);
		// Seed a real old transcript and pending cursor before any service starts.
		const legacyManager = SessionManager.create(tempDir, join(tempDir, "legacy-sessions"));
		legacyManager.appendMessage({ role: "user", content: "legacy private content", timestamp: Date.now() });
		legacyManager.flushNow();
		const legacySessionFile = legacyManager.getSessionFile()!;
		const legacyTranscript = readFileSync(legacySessionFile, "utf8");
		const outboxDir = join(tempDir, "agent-traces-outbox");
		mkdirSync(outboxDir);
		const entryName = `${createHash("sha256").update(legacySessionFile).digest("hex").slice(0, 32)}.json`;
		const pending = JSON.stringify({ sessionFile: legacySessionFile });
		writeFileSync(join(outboxDir, entryName), pending);
		const expectLegacyUntouched = () => {
			expect(readdirSync(outboxDir)).toEqual([entryName]);
			expect(readFileSync(join(outboxDir, entryName), "utf8")).toBe(pending);
			expect(readFileSync(legacySessionFile, "utf8")).toBe(legacyTranscript);
		};
		const authStorage = AuthStorage.inMemory();
		const faux = registerFauxProvider({ provider: "faux-reporting-removal" });
		unregisters.push(() => faux.unregister());
		const fauxApi = getApiProvider(faux.api)!;
		const model = faux.getModel();
		faux.setResponses([fauxAssistantMessage("local response"), fauxAssistantMessage("resumed response")]);
		authStorage.set("prime-agent-traces", { type: "api_key", key: "test-key" });
		authStorage.setRuntimeApiKey(model.provider, "faux-key");
		const services = await createAgentSessionServices({
			cwd: tempDir,
			agentDir: tempDir,
			authStorage,
			modelRegistry: ModelRegistry.inMemory(authStorage),
			mcpManager: new McpManager({ authStorage, getServiceCatalog: () => [], noBackgroundVerification: true }),
			resourceLoaderOptions: {
				noExtensions: true,
				noSkills: true,
				noPromptTemplates: true,
				noThemes: true,
				noContextFiles: true,
			},
		});
		services.modelRegistry.registerProvider(model.provider, {
			baseUrl: model.baseUrl,
			apiKey: "faux-key",
			api: faux.api,
			streamSimple: fauxApi.streamSimple,
			models: faux.models,
		});
		const manager = SessionManager.create(tempDir, join(tempDir, "sessions"));
		const { session } = await createAgentSessionFromServices({
			services,
			sessionManager: manager,
			model,
			noTools: "all",
			prewarmIpythonKernel: false,
		});
		try {
			await session.prompt("local private content");
			expect(faux.state.callCount).toBe(1);
			await session.disposeAsync();
			expect(readFileSync(manager.getSessionFile()!, "utf8")).toContain("local private content");
			expect(readFileSync(manager.getSessionFile()!, "utf8")).toContain("local response");
			expectLegacyUntouched();
			expect(existsSync(join(tempDir, "telemetry.json"))).toBe(false);
			// An old outbox is user data: do not replay, rewrite, or prune it on resume.
			const resumedManager = SessionManager.open(manager.getSessionFile()!);
			const resumed = await createAgentSessionFromServices({
				services,
				sessionManager: resumedManager,
				model,
				noTools: "all",
				prewarmIpythonKernel: false,
			});
			try {
				await resumed.session.prompt("private resumed content");
				expect(faux.state.callCount).toBe(2);
				await resumed.session.disposeAsync();
				expect(readFileSync(manager.getSessionFile()!, "utf8")).toContain("private resumed content");
				expect(readFileSync(manager.getSessionFile()!, "utf8")).toContain("resumed response");
				expectLegacyUntouched();
				expect(existsSync(join(tempDir, "telemetry.json"))).toBe(false);
			} finally {
				await resumed.session.disposeAsync();
				services.mcpManager.dispose();
			}
			// Catalog refreshes are separate from reporting and remain supported.
			for (const [input, init] of fetchSpy.mock.calls) {
				expect(init?.method ?? "GET").toBe("GET");
				expect(String(input)).toMatch(
					/^https:\/\/(raw\.githubusercontent\.com\/PrimeIntellect-ai\/prime-agent-catalog\/main\/|api\.pinference\.ai\/api\/v1\/models$)/,
				);
			}
		} finally {
			await session.disposeAsync();
			vi.unstubAllGlobals();
		}
	});

	it("advertises enabled generic MCP servers and refreshes the prompt on reload", async () => {
		/**
		 * The enabled-generic-servers line: user-declared servers plus per-account
		 * records; a catalog entry appears only when it is explicitly public
		 * no-auth AND setup-ready (credential-free dispatch — api_key and
		 * requires-setup rows fail closed). The exact catalog contents evolve, so
		 * the contract pins WHICH servers appear, not the full list.
		 */
		const enabledServersLine = (prompt: string): string => {
			const match = prompt.match(/Enabled generic MCP servers: ([^\n]*)\./);
			return match?.[1] ?? "";
		};

		const tempDir = join(tmpdir(), `pi-session-mcp-prompt-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		const projectDir = join(tempDir, "project");
		const agentDir = join(tempDir, "agent");
		mkdirSync(join(projectDir, ".prime", "agent"), { recursive: true });
		mkdirSync(agentDir, { recursive: true });
		cleanupPaths.push(tempDir);

		writeFileSync(
			join(agentDir, "settings.json"),
			JSON.stringify({
				mcpServers: {
					zebra: { type: "http", url: "https://secret.example/mcp", headers: { Authorization: "secret" } },
					filesystem: {
						type: "stdio",
						command: "/secret/bin/filesystem",
						args: ["/private/data"],
						cwd: "/secret/cwd",
						env: { TOKEN: { env: "FILESYSTEM_SECRET" } },
					},
					disabled: { type: "stdio", command: "disabled-secret", enabled: false },
					linear: { type: "stdio", command: "reserved-secret" },
				},
			}),
		);
		writeFileSync(
			join(projectDir, ".prime", "agent", "settings.json"),
			JSON.stringify({ mcpServers: { projectOnly: { type: "stdio", command: "project-secret" } } }),
		);

		const settingsManager = SettingsManager.create(projectDir, agentDir);
		const services = await createAgentSessionServices({
			cwd: projectDir,
			agentDir,
			settingsManager,
			resourceLoaderOptions: { noPromptTemplates: true, noThemes: true },
		});
		const { session } = await createAgentSessionFromServices({
			services,
			sessionManager: SessionManager.create(projectDir, join(tempDir, "sessions")),
		});

		try {
			const initialPrompt = session.systemPrompt;
			expect(initialPrompt).toContain(
				"Generic MCP connections are accessed through the pre-imported Python `mcp` object in the Python REPL, not as top-level native tool namespaces or installed Python skills.",
			);
			// Zero none+ready catalog rows exist today, so the enabled line is exactly the user-declared servers.
			expect(enabledServersLine(initialPrompt)).toBe("`filesystem`, `zebra`");
			expect(initialPrompt).toContain('await mcp.list_tools("filesystem")');
			expect(initialPrompt).toContain('await mcp.call_tool("filesystem", "<tool>", arguments)');
			for (const hidden of [
				"disabled",
				"projectOnly",
				"https://secret.example/mcp",
				"Authorization",
				"/secret/bin/filesystem",
				"/private/data",
				"/secret/cwd",
				"FILESYSTEM_SECRET",
				"reserved-secret",
			]) {
				expect(initialPrompt).not.toContain(hidden);
			}
			expect(initialPrompt).not.toContain("Enabled generic MCP servers: `linear`");

			const rebuildRuntime = vi.spyOn(
				session as unknown as { _rebuildRuntimeForAcpMcpServers(): void },
				"_rebuildRuntimeForAcpMcpServers",
			);
			session.replaceAcpMcpServers(
				[
					{
						name: "task",
						type: "http",
						url: "https://task-secret.example/mcp",
						headers: { Authorization: "Bearer task-secret" },
					},
				],
				"owner-a",
			);
			expect(enabledServersLine(session.systemPrompt)).toBe("`filesystem`, `zebra`");
			expect(session.systemPrompt).not.toContain('await mcp.list_tools("task")');
			expect(session.getActiveToolNames()).toEqual(expect.arrayContaining(["mcp_list_tools_task", "mcp_call_task"]));
			expect(session.systemPrompt).not.toContain("task-secret");
			rebuildRuntime.mockClear();
			const waitForIdle = vi.spyOn(session.agent, "waitForIdle");
			await session.releaseAcpMcpServers("unknown-owner", ["task"]);
			expect(waitForIdle).not.toHaveBeenCalled();
			const originalProvisioner = Reflect.get(session, "_ipythonKernelProvisioner");
			const execute = vi.fn(async (_code: string) => ({ status: "ok" }));
			Reflect.set(session, "_ipythonKernelProvisioner", { manager: { isRunning: true, execute } });
			await session.releaseAcpMcpServers("owner-a", ["task"]);
			Reflect.set(session, "_ipythonKernelProvisioner", originalProvisioner);
			expect(rebuildRuntime).not.toHaveBeenCalled();
			expect(execute).toHaveBeenCalledOnce();
			expect(execute.mock.calls[0]?.[0]).toContain("await _prime_mcp.reload(_prime_mcp_name)");
			expect(execute.mock.calls[0]?.[0]).toContain('["task"]');
			expect(enabledServersLine(session.systemPrompt)).toBe("`filesystem`, `zebra`");
			expect(session.getAllTools().map((tool) => tool.name)).not.toContain("mcp_call_task");
			expect(session.getActiveToolNames()).not.toContain("mcp_call_task");

			settingsManager.setGlobalMcpServer("added", { type: "stdio", command: "new-secret" });
			settingsManager.removeGlobalMcpServer("filesystem");
			await settingsManager.flush();
			await session.reload();

			expect(enabledServersLine(session.systemPrompt)).toBe("`added`, `zebra`");
			expect(session.systemPrompt).toContain('await mcp.list_tools("added")');
			expect(session.systemPrompt).not.toContain('await mcp.list_tools("filesystem")');
			expect(session.systemPrompt).not.toContain("new-secret");
		} finally {
			session.dispose();
		}
	});

	it("ENG-6108: activates a verified catalog connection in the live conversation after /plugins login", async () => {
		// A /plugins login must become usable in the current (daemon-backed) conversation without a restart: credentials
		// land in the shared credential store, the connection is verified with a real MCP handshake (token presence alone
		// is never Connected), and the session reload the picker triggers rebuilds the system prompt and serves the catalog
		// service through the generic mcp route.
		const tempDir = join(
			tmpdir(),
			`pi-session-catalog-activation-${Date.now()}-${Math.random().toString(36).slice(2)}`,
		);
		mkdirSync(tempDir, { recursive: true });
		cleanupPaths.push(tempDir);
		const faux = registerFauxProvider({ provider: "faux-eng6108" });
		unregisters.push(() => faux.unregister());
		const authStorage = AuthStorage.inMemory();
		const store = McpConnectionStore.open(join(tempDir, "mcp-connections.json"));
		// Pin a minimal catalog: this exercises connect-then-activate, not the merged catalog's breadth.
		const mcpManager = new McpManager({
			authStorage,
			connectionStore: store,
			getServiceCatalog: () => [
				{
					serviceId: "notion",
					label: "Notion",
					aliases: [],
					transport: { type: "http", url: "https://mcp.notion.com/mcp" },
					authStrategy: "oauth",
					setup: { status: "ready" },
					metadataReviewed: true,
					legacyBuiltin: true,
				},
			],
			noBackgroundVerification: true,
			probeConnection: async () => ({ ok: true, toolCount: 3 }),
		});
		const model = faux.getModel();
		const modelRegistry = ModelRegistry.inMemory(authStorage);
		modelRegistry.registerProvider(model.provider, {
			baseUrl: model.baseUrl,
			apiKey: "faux-key",
			api: faux.api,
			models: faux.models.map((registeredModel) => ({
				id: registeredModel.id,
				name: registeredModel.name,
				api: registeredModel.api,
				reasoning: registeredModel.reasoning,
				input: registeredModel.input,
				cost: registeredModel.cost,
				contextWindow: registeredModel.contextWindow,
				maxTokens: registeredModel.maxTokens,
				baseUrl: registeredModel.baseUrl,
			})),
		});
		const services = await createAgentSessionServices({
			cwd: tempDir,
			agentDir: join(tempDir, "agent"),
			authStorage,
			settingsManager: SettingsManager.inMemory(),
			modelRegistry,
			mcpManager,
			telemetryDisabled: true,
			noBuiltinHerdrReporter: true,
			resourceLoaderOptions: { noExtensions: true },
		});
		const { session } = await createAgentSessionFromServices({
			services,
			sessionManager: SessionManager.inMemory(),
			model,
		});
		try {
			expect(session.agent.state.systemPrompt).not.toContain("Enabled generic MCP servers");
			expect(services.mcpManager.getDisabledBuiltinSkillOverrides()).toContain("-notion/SKILL.md");

			// The /plugins connect flow, minus the UI: browser OAuth writes the
			// shared credential, then the host verifies with a real handshake.
			authStorage.set("mcp:notion", {
				type: "oauth",
				access: "notion-access",
				refresh: "notion-refresh",
				expires: Date.now() + 3600_000,
				endpoint: "https://mcp.notion.com/mcp",
			});
			const record = await services.mcpManager.verifyConnection("notion");
			expect(record.status).toBe("connected");

			// The picker triggers the same session reload the interactive client performs: the prompt now
			// advertises the service and the authored skill override clears.
			await session.reload();
			const promptAfter = session.agent.state.systemPrompt;
			expect(promptAfter).toContain("Enabled generic MCP servers");
			expect(promptAfter).toContain('await mcp.list_tools("notion")');
			expect(services.mcpManager.getDisabledBuiltinSkillOverrides()).not.toContain("-notion/SKILL.md");
		} finally {
			session.dispose();
		}
	});

	it("forwards daemon-backed agent message controllers into AgentSession", async () => {
		const tempDir = join(tmpdir(), `pi-session-services-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
		cleanupPaths.push(tempDir);

		const faux = registerFauxProvider();
		unregisters.push(() => faux.unregister());

		const authStorage = AuthStorage.inMemory();
		authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
		const services = await createAgentSessionServices({
			cwd: tempDir,
			agentDir: tempDir,
			authStorage,
			resourceLoaderOptions: {
				noPromptTemplates: true,
				noThemes: true,
				skillsOverride: () => ({
					skills: [
						{
							name: AGENT_MESSAGE_SKILL_NAME,
							description: "hidden agent message skill",
							filePath: "<test:agent-message>",
							baseDir: tempDir,
							sourceInfo: createSyntheticSourceInfo("<test:agent-message>", { source: "test" }),
							disableModelInvocation: true,
							kind: "python" as const,
							python: {
								importName: "agent_message",
								packagePath: tempDir,
								pyprojectPath: join(tempDir, "pyproject.toml"),
							},
						},
					],
					diagnostics: [],
				}),
			},
		});
		services.modelRegistry.registerProvider(faux.getModel().provider, {
			baseUrl: faux.getModel().baseUrl,
			apiKey: "faux-key",
			api: faux.api,
			models: faux.models,
		});

		const agentMessageController: AgentSessionMessageController = {
			listAgents: () => ({
				current: { activeSessionId: "current", sessionId: "session-current", runtimeKind: "top-level" },
				agents: [
					{
						activeSessionId: "worker",
						sessionId: "session-worker",
						runtimeKind: "top-level",
						cwd: tempDir,
						isStreaming: false,
						unfinishedActionCount: 0,
					},
				],
			}),
			sendAgentMessage: async () => {
				throw new Error("not used");
			},
		};

		const { session } = await createAgentSessionFromServices({
			services,
			sessionManager: SessionManager.create(tempDir, join(tempDir, "sessions")),
			model: faux.getModel(),
			agentMessageController,
		});

		try {
			expect(() => session.handleAgentMessageHostRequest("agent_message.list")).toThrow(
				"unknown agent message request",
			);
			expect(
				(
					session as unknown as {
						_createKernelHostHandlers(): Record<string, unknown>;
					}
				)._createKernelHostHandlers(),
			).not.toHaveProperty("agent_message.send");
		} finally {
			session.dispose();
		}
	});

	it("hides daemon-backed orchestration skills unless their host bridges are available", async () => {
		const tempDir = join(tmpdir(), `pi-session-skills-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
		cleanupPaths.push(tempDir);

		const authStorage = AuthStorage.inMemory();
		const services = await createAgentSessionServices({
			cwd: tempDir,
			agentDir: tempDir,
			authStorage,
			resourceLoaderOptions: {
				noPromptTemplates: true,
				noThemes: true,
			},
		});

		const createSession = async (options: Parameters<typeof createAgentSessionFromServices>[0]) => {
			const { session } = await createAgentSessionFromServices(options);
			return session;
		};
		const visibleSkillNames = (session: unknown) =>
			(
				session as {
					_modelVisibleSkills(): Array<{ name: string }>;
				}
			)
				._modelVisibleSkills()
				.map((skill) => skill.name);
		const kernelHostHandlers = (session: unknown) =>
			(
				session as {
					_createKernelHostHandlers(): Record<string, unknown>;
				}
			)._createKernelHostHandlers();

		const withoutControllers = await createSession({
			services,
			sessionManager: SessionManager.create(tempDir, join(tempDir, "sessions-without")),
		});
		try {
			expect(visibleSkillNames(withoutControllers)).not.toContain(AGENT_MESSAGE_SKILL_NAME);
			expect(visibleSkillNames(withoutControllers)).not.toContain(AGENT_OBSERVE_SKILL_NAME);
		} finally {
			withoutControllers.dispose();
		}

		const agentObserveController: AgentObserveController = {
			listAgents: () => ({
				current: {
					activeSessionId: "current",
					sessionId: "session-current",
					runtimeKind: "top-level",
					cwd: tempDir,
					status: "idle",
					isCurrent: true,
					isStreaming: false,
					isCompacting: false,
					attachedClients: 1,
					messageCount: 0,
					queuedCount: 0,
					isSessionActive: false,
				},
				agents: [],
			}),
			getAgent: () => {
				throw new Error("not used");
			},
			recentMessages: () => {
				throw new Error("not used");
			},
		};
		const withControllers = await createSession({
			services,
			sessionManager: SessionManager.create(tempDir, join(tempDir, "sessions-with")),
			agentObserveController,
		});
		try {
			expect(visibleSkillNames(withControllers)).toContain(AGENT_OBSERVE_SKILL_NAME);
			expect(visibleSkillNames(withControllers)).not.toContain(AGENT_MESSAGE_SKILL_NAME);
		} finally {
			withControllers.dispose();
		}

		const agentMessageController: AgentSessionMessageController = {
			listAgents: () => ({
				current: { activeSessionId: "current", sessionId: "session-current" },
				agents: [],
			}),
			sendAgentMessage: async () => {
				throw new Error("not used");
			},
		};
		const withMessageController = await createSession({
			services,
			sessionManager: SessionManager.create(tempDir, join(tempDir, "sessions-with-message")),
			agentMessageController,
		});
		try {
			expect(visibleSkillNames(withMessageController)).toContain(AGENT_MESSAGE_SKILL_NAME);
			expect(kernelHostHandlers(withMessageController)).toHaveProperty("agent_message.send");
		} finally {
			withMessageController.dispose();
		}
	});
});
