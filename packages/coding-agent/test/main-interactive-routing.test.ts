import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test, vi } from "vitest";
import { parseArgs } from "../src/cli/args.js";
import { ENV_AGENT_DIR } from "../src/config.js";
import { mergeAgentSessionRuntimeConfig } from "../src/core/agent-session-config.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import type { CreateAgentSessionOptions } from "../src/core/sdk.js";
import {
	type DaemonClientStartupDecision,
	type DaemonInteractiveSessionManagerDecision,
	daemonServerDefaultSessionConfig,
	findActiveDaemonSessionSummaryForSessionFile,
	isClientOwnedDaemonSession,
	parseAgentsViewCommand,
	resolveActiveSessionLookupFailure,
	resolveRuntimeSessionOptions,
	runOneShotModelRefresh,
	runtimeConfigFromArgs,
	shouldEnsureDaemonBeforeActiveSessionLookup,
	shouldEnsureInteractiveDaemonForStartup,
	shouldOpenAgentsViewForDaemonInteractive,
	shouldRejectNonInteractiveAttach,
	shouldRejectNonInteractiveBareResume,
	shouldUseDaemonClient,
	shouldUseDaemonClientRuntime,
	shouldUseEphemeralSessionManagerForDaemonInteractive,
} from "../src/main.js";
import { formatModelRefreshReport } from "../src/modes/agent-connection/types.js";
import { DaemonCapabilityUnavailableError, DaemonClient } from "../src/modes/daemon/daemon-client.js";
import { DaemonSessionRecoveringError } from "../src/modes/daemon/daemon-errors.js";
import type { SessionSummary } from "../src/modes/index.js";

const isolatedCliEnv = Object.fromEntries(
	Object.entries(process.env).filter(([name]) => !/(TOKEN|KEY|SECRET|AUTH|CREDENTIAL)/i.test(name)),
);

describe("interactive startup routing", () => {
	test("#17 writes standalone refresh results to stdout, not stderr", async () => {
		const home = mkdtempSync(join(tmpdir(), "prime-refresh-streams-"));
		try {
			const child = spawn(
				process.execPath,
				[
					join(process.cwd(), "../../node_modules/tsx/dist/cli.mjs"),
					"src/cli.ts",
					"--refresh",
					"--offline",
					"--daemon-socket",
					join(home, "missing.sock"),
				],
				{
					cwd: process.cwd(),
					env: {
						...process.env,
						HOME: home,
						XDG_CONFIG_HOME: join(home, "xdg"),
						[ENV_AGENT_DIR]: join(home, "agent"),
					},
					stdio: ["ignore", "pipe", "pipe"],
				},
			);
			let stdout = "";
			let stderr = "";
			child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
				stdout += chunk;
			});
			child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
				stderr += chunk;
			});
			const exitCode = await new Promise<number | null>((resolve, reject) => {
				const deadline = setTimeout(() => {
					child.kill();
					reject(new Error("refresh child exceeded deadline"));
				}, 20_000);
				child.once("error", (error) => {
					clearTimeout(deadline);
					reject(error);
				});
				child.once("close", (code) => {
					clearTimeout(deadline);
					resolve(code);
				});
			});
			expect(exitCode).toBe(0);
			expect(stdout).toContain("curated: stale (offline)");
			expect(stderr).not.toContain("curated: stale");
		} finally {
			rmSync(home, { recursive: true, force: true });
		}
	});
	test("#17 prints sanitized partial failures to stdout and exits one", async () => {
		const home = mkdtempSync(join(tmpdir(), "prime-refresh-failure-"));
		const socketPath = join(home, "fake-daemon.sock");
		const server = createServer((socket) => {
			socket.on("error", (error: NodeJS.ErrnoException) => {
				if (error.code !== "ECONNRESET") throw error;
			});
			socket.write(
				`${JSON.stringify({
					type: "daemon_hello",
					socketPath,
					protocol: { name: "prime-agent.daemon", version: 7 },
					schemaRevision: 31,
					clientId: "test",
					serverCapabilities: ["model_refresh"],
				})}\n`,
			);
			let buffer = "";
			socket.setEncoding("utf8").on("data", (chunk: string) => {
				buffer += chunk;
				const newline = buffer.indexOf("\n");
				if (newline < 0) return;
				const request = JSON.parse(buffer.slice(0, newline)) as { id?: string; command?: { type?: string } };
				if (request.command?.type !== "refresh_all_models") return;
				socket.write(
					`${JSON.stringify({
						id: request.id,
						type: "response",
						command: request.command.type,
						success: true,
						data: {
							startedAt: 1,
							finishedAt: 2,
							sources: [{ source: "https://secret.example/token=sk-test", status: "failed", reason: "network" }],
							workers: { total: 1, succeeded: 0, failed: 1 },
						},
					})}\n`,
				);
			});
		});
		try {
			await new Promise<void>((resolve, reject) => {
				server.once("error", reject);
				server.listen(socketPath, () => {
					server.removeListener("error", reject);
					resolve();
				});
			});
			const child = spawn(
				process.execPath,
				[
					join(process.cwd(), "../../node_modules/tsx/dist/cli.mjs"),
					"src/cli.ts",
					"--refresh",
					"--daemon-socket",
					socketPath,
				],
				{
					cwd: process.cwd(),
					env: {
						...isolatedCliEnv,
						HOME: home,
						XDG_CONFIG_HOME: join(home, "xdg"),
						[ENV_AGENT_DIR]: join(home, "agent"),
					},
					stdio: ["ignore", "pipe", "pipe"],
				},
			);
			let stdout = "";
			let stderr = "";
			child.stdout.setEncoding("utf8").on("data", (chunk: string) => {
				stdout += chunk;
			});
			child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
				stderr += chunk;
			});
			const exitCode = await new Promise<number | null>((resolve, reject) => {
				const deadline = setTimeout(() => {
					child.kill();
					reject(new Error("refresh child exceeded deadline"));
				}, 20_000);
				child.once("error", (error) => {
					clearTimeout(deadline);
					reject(error);
				});
				child.once("close", (code) => {
					clearTimeout(deadline);
					resolve(code);
				});
			});
			expect(exitCode).toBe(1);
			expect(stdout).toContain("unknown-source: failed (network)");
			expect(stdout + stderr).not.toContain("sk-test");
			expect(stderr).not.toContain("unknown-source:");
		} finally {
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(home, { recursive: true, force: true });
		}
	});

	test("#17 refreshes through a compatible daemon without attaching a session", async () => {
		const connect = vi.spyOn(DaemonClient.prototype, "connect").mockResolvedValue();
		const hello = vi.spyOn(DaemonClient.prototype, "waitForHello").mockResolvedValue({
			type: "daemon_hello",
			socketPath: "/tmp/unused-refresh-test.sock",
			protocol: { name: "prime-agent.daemon", version: 7 },
			schemaRevision: 31,
			clientId: "test",
			serverCapabilities: ["model_refresh"],
		});
		const supports = vi.spyOn(DaemonClient.prototype, "supportsServerCapability").mockReturnValue(true);
		const request = vi.spyOn(DaemonClient.prototype, "request").mockResolvedValue({
			type: "response",
			command: "refresh_all_models",
			success: true,
			data: { startedAt: 1, finishedAt: 2, sources: [{ source: "curated", status: "unchanged" }] },
		});
		const close = vi.spyOn(DaemonClient.prototype, "close").mockImplementation(() => {});
		const log = vi.spyOn(console, "log").mockImplementation(() => {});
		try {
			await runOneShotModelRefresh(parseArgs(["--refresh", "--daemon-socket", "/tmp/unused-refresh-test.sock"]));
			expect(connect).toHaveBeenCalledTimes(1);
			expect(supports).toHaveBeenCalledWith("model_refresh");
			expect(request).toHaveBeenCalledWith({ type: "refresh_all_models" }, 120_000);
			expect(log).toHaveBeenCalledWith("curated: unchanged");
			expect(process.exitCode).not.toBe(1);
		} finally {
			process.exitCode = undefined;
			connect.mockRestore();
			hello.mockRestore();
			supports.mockRestore();
			request.mockRestore();
			close.mockRestore();
			log.mockRestore();
		}
	});

	test("#17 does not retry a failed daemon refresh in the standalone registry", async () => {
		const connect = vi.spyOn(DaemonClient.prototype, "connect").mockResolvedValue();
		const hello = vi.spyOn(DaemonClient.prototype, "waitForHello").mockResolvedValue({
			type: "daemon_hello",
			socketPath: "/tmp/unused-refresh-test.sock",
			protocol: { name: "prime-agent.daemon", version: 7 },
			schemaRevision: 31,
			clientId: "test",
			serverCapabilities: ["model_refresh"],
		});
		const supports = vi.spyOn(DaemonClient.prototype, "supportsServerCapability").mockReturnValue(true);
		const request = vi.spyOn(DaemonClient.prototype, "request").mockRejectedValue(new Error("secret endpoint"));
		const close = vi.spyOn(DaemonClient.prototype, "close").mockImplementation(() => {});
		const registry = vi.spyOn(ModelRegistry, "create");
		const error = vi.spyOn(console, "error").mockImplementation(() => {});
		try {
			await runOneShotModelRefresh(parseArgs(["--refresh", "--daemon-socket", "/tmp/unused-refresh-test.sock"]));
			expect(request).toHaveBeenCalledTimes(1);
			expect(registry).not.toHaveBeenCalled();
			expect(error).toHaveBeenCalledWith("Model catalog refresh failed. Check the agent log for details.");
			expect(process.exitCode).toBe(1);
		} finally {
			process.exitCode = undefined;
			connect.mockRestore();
			hello.mockRestore();
			supports.mockRestore();
			request.mockRestore();
			close.mockRestore();
			registry.mockRestore();
			error.mockRestore();
		}
	});

	test.each(["zero-workers", "prewrite-capability"] as const)(
		"#17 falls back to standalone refresh on %s",
		async (scenario) => {
			const connect = vi.spyOn(DaemonClient.prototype, "connect").mockResolvedValue();
			const hello = vi.spyOn(DaemonClient.prototype, "waitForHello").mockResolvedValue({
				type: "daemon_hello",
				socketPath: "/tmp/unused-refresh-test.sock",
				protocol: { name: "prime-agent.daemon", version: 7 },
				schemaRevision: 31,
				clientId: "test",
				serverCapabilities: ["model_refresh"],
			});
			const supports = vi.spyOn(DaemonClient.prototype, "supportsServerCapability").mockReturnValue(true);
			const request = vi.spyOn(DaemonClient.prototype, "request");
			if (scenario === "prewrite-capability") {
				request.mockRejectedValue(new DaemonCapabilityUnavailableError("refresh_all_models", "model_refresh"));
			} else {
				request.mockResolvedValue({
					type: "response",
					command: "refresh_all_models",
					success: true,
					data: { startedAt: 1, finishedAt: 2, sources: [], workers: { total: 0, succeeded: 0, failed: 0 } },
				});
			}
			const close = vi.spyOn(DaemonClient.prototype, "close").mockImplementation(() => {});
			const isolatedAgentDir = mkdtempSync(join(tmpdir(), "prime-model-refresh-cli-"));
			const priorAgentDir = process.env[ENV_AGENT_DIR];
			process.env[ENV_AGENT_DIR] = isolatedAgentDir;
			const forceRefreshModels = vi.fn(async () => ({
				startedAt: 3,
				finishedAt: 4,
				sources: [{ source: "curated", status: "updated" as const }],
			}));
			const registry = vi
				.spyOn(ModelRegistry, "create")
				.mockReturnValue({ forceRefreshModels } as unknown as ModelRegistry);
			const log = vi.spyOn(console, "log").mockImplementation(() => {});
			try {
				await runOneShotModelRefresh(parseArgs(["--refresh", "--daemon-socket", "/tmp/unused-refresh-test.sock"]));
				expect(request).toHaveBeenCalledWith({ type: "refresh_all_models" }, 120_000);
				expect(forceRefreshModels).toHaveBeenCalledTimes(1);
				expect(log).toHaveBeenCalledWith("curated: updated");
			} finally {
				process.exitCode = undefined;
				connect.mockRestore();
				hello.mockRestore();
				supports.mockRestore();
				request.mockRestore();
				close.mockRestore();
				registry.mockRestore();
				log.mockRestore();
				if (priorAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
				else process.env[ENV_AGENT_DIR] = priorAgentDir;
				rmSync(isolatedAgentDir, { recursive: true, force: true });
			}
		},
	);

	test("#17 rejects prompt options before contacting a daemon or registry", async () => {
		const error = vi.spyOn(console, "error").mockImplementation(() => {});
		try {
			await runOneShotModelRefresh(parseArgs(["--refresh", "hello"]));
			await runOneShotModelRefresh(parseArgs(["--refresh", "--goal", "oops"]));
			expect(error).toHaveBeenCalledWith(expect.stringContaining("no session or prompt options"));
			expect(process.exitCode).toBe(1);
		} finally {
			process.exitCode = undefined;
			error.mockRestore();
		}
	});

	test("#17 prints only safe refresh fields without leaking endpoints", () => {
		expect(
			formatModelRefreshReport({
				startedAt: 1,
				finishedAt: 2,
				sources: [{ source: "https://secret.example/?token=sk-test", status: "failed", reason: "network" }],
			}),
		).toEqual(["unknown-source: failed (network)"]);
	});
	test.each([
		["acp", false, false],
		["acp", true, true],
		["rpc", false, true],
		["print", false, true],
	] as const)("classifies %s noSession=%s ownership", (appMode, noSession, expected) => {
		expect(isClientOwnedDaemonSession(appMode, noSession)).toBe(expected);
	});

	test.each([
		["interactive client", { appMode: "interactive", startupBenchmark: false }, true],
		["print client", { appMode: "print", startupBenchmark: false }, true],
		["json client", { appMode: "json", startupBenchmark: false }, true],
		["rpc client", { appMode: "rpc", startupBenchmark: false }, true],
		["--no-session", { appMode: "interactive", startupBenchmark: false, noSession: true }, true],
		["daemon process", { appMode: "daemon", startupBenchmark: false }, false],
		["startup benchmark", { appMode: "interactive", startupBenchmark: true }, false],
		["help", { appMode: "interactive", startupBenchmark: false, help: true }, false],
		["model listing", { appMode: "interactive", startupBenchmark: false, listModels: true }, false],
	] satisfies Array<[string, DaemonClientStartupDecision, boolean]>)(
		"routes %s to the daemon client runtime: %s",
		(_label, decision, expected) => {
			expect(shouldUseDaemonClient(decision)).toBe(expected);
		},
	);

	test("keeps process-local extension factories and rollback workers in process", () => {
		expect(
			shouldUseDaemonClientRuntime({
				appMode: "print",
				startupBenchmark: false,
				hasProcessLocalExtensionFactories: true,
			}),
		).toBe(false);
		expect(
			shouldUseDaemonClientRuntime({
				appMode: "rpc",
				startupBenchmark: false,
				ownedSessionWorker: true,
			}),
		).toBe(false);
	});

	test("rejects interactive-only selectors before non-interactive startup", () => {
		expect(shouldRejectNonInteractiveAttach("worker", "print")).toBe(true);
		expect(shouldRejectNonInteractiveAttach("worker", "interactive")).toBe(false);
		expect(shouldRejectNonInteractiveAttach(undefined, "print")).toBe(false);
		expect(shouldRejectNonInteractiveBareResume(true, "print")).toBe(true);
		expect(shouldRejectNonInteractiveBareResume(true, "rpc")).toBe(true);
		expect(shouldRejectNonInteractiveBareResume("session-id", "print")).toBe(false);
		expect(shouldRejectNonInteractiveBareResume(true, "interactive")).toBe(false);
	});

	test("does not start the daemon for attach", () => {
		expect(shouldEnsureInteractiveDaemonForStartup(true, undefined)).toBe(true);
		expect(shouldEnsureInteractiveDaemonForStartup(true, "worker")).toBe(false);
		expect(shouldEnsureInteractiveDaemonForStartup(false, undefined)).toBe(false);
	});
});

describe("daemon-backed interactive session manager routing", () => {
	test.each([
		["default daemon-backed startup", { useDaemonInteractive: true, needsOnboarding: false }, false],
		[
			"an explicit agents view request",
			{ useDaemonInteractive: true, needsOnboarding: false, explicitAgentsView: true },
			true,
		],
		["bare --resume", { useDaemonInteractive: true, needsOnboarding: false, resume: true }, true],
		["bare --resume during onboarding", { useDaemonInteractive: true, needsOnboarding: true, resume: true }, true],
		[
			"the non-daemon interactive path",
			{ useDaemonInteractive: false, needsOnboarding: false, explicitAgentsView: true },
			false,
		],
		["pending onboarding", { useDaemonInteractive: true, needsOnboarding: true, explicitAgentsView: true }, false],
		[
			"a resume selector",
			{ useDaemonInteractive: true, needsOnboarding: false, explicitAgentsView: true, resume: "active-1" },
			false,
		],
		[
			"continue recent",
			{ useDaemonInteractive: true, needsOnboarding: false, explicitAgentsView: true, continue: true },
			false,
		],
		[
			"fork",
			{ useDaemonInteractive: true, needsOnboarding: false, explicitAgentsView: true, fork: "source-session-id" },
			false,
		],
	] satisfies Array<[string, Parameters<typeof shouldOpenAgentsViewForDaemonInteractive>[0], boolean]>)(
		"opens the agents view for %s: %s",
		(_label, decision, expected) => {
			expect(shouldOpenAgentsViewForDaemonInteractive(decision)).toBe(expected);
		},
	);

	test("ensures daemon is available before probing non-path session selectors", () => {
		expect(
			shouldEnsureDaemonBeforeActiveSessionLookup({
				useDaemonInteractive: true,
				resumeSelector: "active-1",
			}),
		).toBe(true);
		expect(
			shouldEnsureDaemonBeforeActiveSessionLookup({
				useDaemonInteractive: true,
				resumeSelector: "/tmp/session.jsonl",
			}),
		).toBe(false);
		expect(
			shouldEnsureDaemonBeforeActiveSessionLookup({
				useDaemonInteractive: true,
				resumeSelector: "/tmp/session.jsonl",
				explicitAttach: true,
			}),
		).toBe(true);
		expect(
			shouldEnsureDaemonBeforeActiveSessionLookup({
				useDaemonInteractive: false,
				resumeSelector: "active-1",
			}),
		).toBe(false);
	});

	test.each([
		["a fresh daemon-owned session", {}, true],
		["bare --resume", { resume: true }, true],
		["an active daemon attach", { hasActiveDaemonSession: true }, false],
		["an explicit saved session", { resume: "saved-session-id" }, false],
		["continue recent", { continue: true }, false],
		["fork", { fork: "source-session-id" }, false],
	] satisfies Array<[string, DaemonInteractiveSessionManagerDecision, boolean]>)(
		"uses an ephemeral local session manager for %s: %s",
		(_label, decision, expected) => {
			expect(shouldUseEphemeralSessionManagerForDaemonInteractive(decision)).toBe(expected);
		},
	);

	test("finds an active daemon session by resolved session file", () => {
		const inactiveSummary = makeSessionSummary({
			id: "saved-1",
			activeSessionId: undefined,
			sessionFile: "/tmp/project/session.jsonl",
		});
		const activeSummary = makeSessionSummary({
			id: "active-1",
			activeSessionId: "active-1",
			sessionFile: "/tmp/project/session.jsonl",
		});

		expect(
			findActiveDaemonSessionSummaryForSessionFile(
				[inactiveSummary, activeSummary],
				"/tmp/project/../project/session.jsonl",
			),
		).toBe(activeSummary);
	});

	test("finds an active daemon session through a symlinked resume path", () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-agent-resume-"));
		try {
			const sessionFile = join(directory, "session.jsonl");
			const symlink = join(directory, "session-link.jsonl");
			writeFileSync(sessionFile, "");
			symlinkSync(sessionFile, symlink);
			const activeSummary = makeSessionSummary({
				id: "active-1",
				activeSessionId: "active-1",
				sessionFile,
			});

			expect(findActiveDaemonSessionSummaryForSessionFile([activeSummary], symlink)).toBe(activeSummary);
		} finally {
			rmSync(directory, { recursive: true, force: true });
		}
	});
});

describe("agents view command parsing", () => {
	test.each([
		{ args: ["agents"], explicitAgentsView: true, rest: [] as string[] },
		{ args: ["manage", "--verbose"], explicitAgentsView: false, rest: ["manage", "--verbose"] },
		{ args: ["fix the agents view"], explicitAgentsView: false, rest: ["fix the agents view"] },
		{ args: ["--verbose", "agents"], explicitAgentsView: false, rest: ["--verbose", "agents"] },
	])("parses $args", ({ args, explicitAgentsView, rest }) => {
		expect(parseAgentsViewCommand(args)).toEqual({ explicitAgentsView, args: rest });
	});
});

describe("runtime session option resolution", () => {
	test.each(["interactive", "print", "json", "rpc", "acp", "daemon"] as const)(
		"issue #4 always sends the legacy daemon opt-out in %s mode, even with analytics opt-in env",
		(appMode) => {
			vi.stubEnv("PRIME_AGENT_TELEMETRY", "1");
			vi.stubEnv("DO_NOT_TRACK", "0");
			try {
				const config = runtimeConfigFromArgs(parseArgs([]), "/repo", "/agent", undefined, appMode);
				expect(JSON.parse(JSON.stringify(config)).telemetryDisabled).toBe(true);
			} finally {
				vi.unstubAllEnvs();
			}
		},
	);

	test("keeps verifier goals per session instead of in the daemon fallback", () => {
		const headlessCreateConfig = {
			cwd: "/repo",
			serializedRefine: true,
			initialGoal: { objective: "solve the verifier task", tokenBudget: 100_000 },
		};

		const daemonFallback = daemonServerDefaultSessionConfig(headlessCreateConfig);
		expect(daemonFallback).toEqual({
			cwd: "/repo",
			serializedRefine: true,
			initialGoal: undefined,
		});
		expect(
			mergeAgentSessionRuntimeConfig(daemonFallback, {
				initialGoal: headlessCreateConfig.initialGoal,
			}),
		).toMatchObject({ initialGoal: headlessCreateConfig.initialGoal });
	});

	test("preserves daemon-provided RLM heartbeat controller when creating sessions", () => {
		const preparedModel = { id: "prepared-model" } as unknown as CreateAgentSessionOptions["model"];
		const runtimeModel = { id: "runtime-model" } as unknown as CreateAgentSessionOptions["model"];
		const rlmHeartbeatController: NonNullable<CreateAgentSessionOptions["rlmHeartbeatController"]> = {
			listRlmHeartbeats: () => [],
			createRlmHeartbeat: () => {
				throw new Error("not used");
			},
			updateRlmHeartbeat: async () => undefined,
			deleteRlmHeartbeat: async () => undefined,
		};

		const resolved = resolveRuntimeSessionOptions(
			{
				model: preparedModel,
				tools: ["ipython"],
				customTools: [],
			},
			{
				model: runtimeModel,
				rlmHeartbeatController,
				rlmDepth: 1,
				rlmSessionDir: "/tmp/rlm-session",
			},
		);

		expect(resolved).toMatchObject({
			model: runtimeModel,
			tools: ["ipython"],
			customTools: [],
			rlmHeartbeatController,
			rlmDepth: 1,
			rlmSessionDir: "/tmp/rlm-session",
		});
	});

	test("forwards child lineage - parent agent, semantic parent and spawn request - to the child session", () => {
		const resolved = resolveRuntimeSessionOptions(
			{},
			{
				rlmDepth: 1,
				rlmParentAgent: "parent-worker",
				semanticParentSessionId: "parent-session-id",
				semanticSpawnedByRequestId: "a".repeat(32),
			},
		);

		expect(resolved.rlmParentAgent).toBe("parent-worker");
		expect(resolved.semanticParentSessionId).toBe("parent-session-id");
		expect(resolved.semanticSpawnedByRequestId).toBe("a".repeat(32));
	});

	test.each([
		{ name: "a top-level runtime session", rlmDepth: undefined, enabled: true },
		{ name: "a subagent runtime session", rlmDepth: 1, enabled: false },
	])("deep-merges autonomous overrides for $name", ({ rlmDepth, enabled }) => {
		const resolved = resolveRuntimeSessionOptions(
			{
				autonomous: {
					enabled: true,
					maxTurns: 20,
					gates: { commands: ["npm test"], maxRetries: 3 },
				},
			},
			{
				...(rlmDepth === undefined ? {} : { rlmDepth }),
				autonomous: {
					maxContinuations: 5,
					gates: { timeoutMs: 1000 },
				},
			},
		);

		expect(resolved.autonomous).toEqual({
			enabled,
			maxTurns: 20,
			maxContinuations: 5,
			gates: { commands: ["npm test"], maxRetries: 3, timeoutMs: 1000 },
		});
	});

	test("classifies active-session lookup failures: recovering is typed, unknown falls back", () => {
		const recovering = resolveActiveSessionLookupFailure({
			type: "response",
			command: "get_state",
			success: false,
			error: "Active session active-gap is recovering; retry shortly",
			errorInfo: { code: "session_recovering", activeSessionId: "active-gap" },
		});
		expect(recovering).toBeInstanceOf(DaemonSessionRecoveringError);
		expect((recovering as DaemonSessionRecoveringError).activeSessionId).toBe("active-gap");
		expect(
			resolveActiveSessionLookupFailure({
				type: "response",
				command: "get_state",
				success: false,
				error: "Unknown active session: active-gap",
			}),
		).toBeUndefined();
		expect(
			resolveActiveSessionLookupFailure({
				type: "response",
				command: "get_state",
				success: false,
				error: "socket closed",
			}),
		).toBeInstanceOf(Error);
	});
});

function makeSessionSummary(overrides: Partial<SessionSummary>): SessionSummary {
	return {
		id: "session-1",
		lifecycle: "draft",
		activity: "idle",
		sessionId: "session-1",
		cwd: "/tmp/project",
		isStreaming: false,
		isCompacting: false,
		attachedClients: 0,
		messageCount: 0,
		sessionActions: { queuedCount: 0, steering: [], followUps: [] },
		...overrides,
		isSessionActive: overrides.isSessionActive ?? false,
	};
}
