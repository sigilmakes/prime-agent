/** Image-turn routing: `settings.imageModel` serves image turns on text-only session models. */

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Agent } from "@earendil-works/pi-agent-core";
import type { AssistantMessage, AssistantMessageEvent } from "@earendil-works/pi-ai";
import { EventStream, type ImageContent } from "@earendil-works/pi-ai";
import { expect, it } from "vitest";
import { AgentSession } from "../src/core/agent-session.js";
import { AuthStorage } from "../src/core/auth-storage.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { getCodingAgentFixtureModel } from "./fixture-models.js";
import { assistantMsg, createTestResourceLoader } from "./utilities.js";

const IMAGE: ImageContent = { type: "image", mimeType: "image/png", data: "aGk=" };
const SET = { imageModel: "claude-haiku-4-5" };
const SET_BLOCKED = { imageModel: "claude-haiku-4-5", images: { blockImages: true } };
const SET_UNUSABLE = { imageModel: "openai/gpt-5.4" };
const SET_TEXTONLY = { imageModel: "deepseek/deepseek-v4-pro" };
const RETRY = { enabled: true, maxRetries: 3, baseDelayMs: 1 };
const SET_BACKUP_TEXT = { ...SET, providerBackupModel: "deepseek/deepseek-v4-pro", retry: RETRY };
const SET_BACKUP_VISION = { ...SET, providerBackupModel: "claude-opus-4-7", retry: RETRY };
const SET_BACKUP_SAME = { ...SET, providerBackupModel: "claude-haiku-4-5", retry: RETRY };
const SERVED_IMAGE = ["claude-haiku-4-5", "claude-haiku-4-5"];
const SERVED_BACKUP = ["claude-haiku-4-5", "claude-opus-4-7"];
const transientFailure = (): AssistantMessage => ({
	...assistantMsg(""),
	stopReason: "error",
	errorMessage: "overloaded_error",
	diagnostics: [{ type: "provider_stream_failure", timestamp: Date.now(), details: { kind: "overloaded" } }],
});

// [name, settings, vision session model, attaches images, served model ids, rejection]
it.each([
	["routes image turns to imageModel", SET, false, true, "claude-haiku-4-5", undefined],
	["vision session model serves image turns", SET, true, true, "claude-opus-4-7", undefined],
	["session model serves image-free turns", SET, false, false, "claude-opus-4-7-text-only", undefined],
	["session model serves blocked-image turns", SET_BLOCKED, false, true, "claude-opus-4-7-text-only", undefined],
	["refuses image turns without imageModel", {}, false, true, undefined, /does not accept image input/],
	["refuses an unusable imageModel", SET_UNUSABLE, false, true, undefined, /could not be resolved/],
	["refuses a text-only imageModel", SET_TEXTONLY, false, true, undefined, /could not be resolved/],
	["skips a text-only backup for image turns", SET_BACKUP_TEXT, false, true, SERVED_IMAGE, undefined],
	["image-capable backup serves the retry", SET_BACKUP_VISION, false, true, SERVED_BACKUP, undefined],
	["duplicate backup skips the no-op switch", SET_BACKUP_SAME, false, true, SERVED_IMAGE, undefined],
	["cycling mid-stream keeps routing", SET, false, true, SERVED_IMAGE, undefined, undefined, true],
	["cycling clears the routed override", SET, false, true, "claude-haiku-4-5", undefined, true],
])("%s", async (_name, settings, vision, images, served, reject, cycleAfter?: boolean, cycleMidStream?: boolean) => {
	const dir = mkdtempSync(join(tmpdir(), "pi-image-model-"));
	writeFileSync(join(dir, "settings.json"), JSON.stringify(settings));
	const base = getCodingAgentFixtureModel("anthropic", "claude-opus-4-7");
	const sessionModel = (vision ? base : { ...base, id: "claude-opus-4-7-text-only", input: ["text"] }) as typeof base;
	const servedIds: string[] = [];
	let cycleMidStreamHook: (() => void) | undefined;
	const agent = new Agent({
		getApiKey: () => "test-key",
		initialState: { model: sessionModel, systemPrompt: "Test", tools: [] },
		streamFn: (model) => {
			servedIds.push(model.id);
			if (cycleMidStream && servedIds.length === 1) cycleMidStreamHook?.();
			const stream = new EventStream<AssistantMessageEvent, AssistantMessage>(
				(e) => e.type === "done",
				(e: any) => e.message,
			);
			// Rows listing a served sequence inject the transient failure that
			// triggers the backup retry; the retry serves the next entry.
			const message = Array.isArray(served) && servedIds.length === 1 ? transientFailure() : assistantMsg("ok");
			stream.push({ type: "done", reason: "stop", message });
			return stream;
		},
	});
	const auth = AuthStorage.create(join(dir, "auth.json"));
	auth.setRuntimeApiKey("anthropic", "test-key");
	auth.setRuntimeApiKey("deepseek", "test-key");
	const session = new AgentSession({
		agent,
		sessionManager: SessionManager.inMemory(),
		settingsManager: SettingsManager.create(dir, dir),
		cwd: dir,
		modelRegistry: ModelRegistry.create(auth, dir),
		resourceLoader: createTestResourceLoader(),
	});
	cycleMidStreamHook = () => void session.cycleModel("forward", { waitForExtensions: false });
	const backupSwitches: string[] = [];
	session.subscribe((event) => {
		if (event.type === "auto_retry_start" && event.reason === "backup") backupSwitches.push(event.reason);
	});
	try {
		const prompt = session.prompt("describe", images ? { images: [IMAGE] } : undefined);
		if (reject) return await expect(prompt).rejects.toThrow(reject);
		await prompt;
		expect(servedIds).toEqual(Array.isArray(served) ? served : [served]);
		expect(backupSwitches).toEqual(Array.isArray(served) && served[1] !== served[0] ? ["backup"] : []);
		if (cycleMidStream) expect(session.model?.id).not.toBe(sessionModel.id);
		else expect(session.model?.id).toBe(sessionModel.id);
		if (!cycleAfter) return;
		// The routed turn leaves its override behind; cycling must clear it so
		// the selection wins over later continues, retries, and compaction.
		expect(session.agent.modelOverride?.model.id).toBe(served);
		await session.cycleModel("forward", { waitForExtensions: false });
		expect(session.agent.modelOverride).toBeUndefined();
		expect(session.model?.id).not.toBe(sessionModel.id);
	} finally {
		session.dispose();
		rmSync(dir, { recursive: true, force: true });
	}
});

// A project cap must follow a model that serves only one routed/retried turn.
it.each([
	["image route", { imageModel: "ollama-cloud/vision" }, true],
	["backup retry", { providerBackupModel: "ollama-cloud/vision", retry: RETRY }, false],
])("#6 applies project caps on %s without changing the primary model", async (_name, settings, images) => {
	const root = mkdtempSync(join(tmpdir(), "pi-ollama-route-"));
	const cwd = join(root, "project");
	const agentDir = join(root, "agent");
	mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
	mkdirSync(agentDir);
	writeFileSync(
		join(cwd, ".prime", "agent", "ollama-cloud.json"),
		JSON.stringify({ modelOverrides: { vision: { contextWindow: 4000 } } }),
	);
	writeFileSync(join(agentDir, "settings.json"), JSON.stringify(settings));
	writeFileSync(
		join(agentDir, "models.json"),
		JSON.stringify({
			providers: {
				"ollama-cloud": {
					api: "openai-completions",
					baseUrl: "http://127.0.0.1:1/v1",
					apiKey: "test-key",
					models: [{ id: "vision", input: ["text", "image"], contextWindow: 32768, maxTokens: 1024 }],
				},
			},
		}),
	);
	const auth = AuthStorage.create(join(agentDir, "auth.json"));
	auth.setRuntimeApiKey("anthropic", "test-key");
	auth.setRuntimeApiKey("ollama-cloud", "test-key");
	const primary = { ...getCodingAgentFixtureModel("anthropic", "claude-opus-4-7"), input: ["text"] as ["text"] };
	const served: Array<{ provider: string; contextWindow: number }> = [];
	const agent = new Agent({
		getApiKey: () => "test-key",
		initialState: { model: primary, systemPrompt: "Test", tools: [] },
		streamFn: (model) => {
			served.push({ provider: model.provider, contextWindow: model.contextWindow });
			const stream = new EventStream<AssistantMessageEvent, AssistantMessage>(
				(e) => e.type === "done",
				(e: any) => e.message,
			);
			const response = !images && served.length === 1 ? transientFailure() : assistantMsg("ok");
			stream.push({ type: "done", reason: "stop", message: response });
			return stream;
		},
	});
	const registry = ModelRegistry.create(auth, join(agentDir, "models.json"));
	expect(
		registry
			.getAll()
			.filter((m) => m.provider === "ollama-cloud" && m.id === "vision")
			.map((m) => [m.id, m.input]),
	).toEqual([["vision", ["text", "image"]]]);
	const session = new AgentSession({
		agent,
		sessionManager: SessionManager.inMemory(cwd),
		settingsManager: SettingsManager.create(cwd, agentDir),
		cwd,
		agentDir,
		modelRegistry: registry,
		resourceLoader: createTestResourceLoader(),
	});
	try {
		await session.prompt("describe", images ? { images: [IMAGE] } : undefined);
		expect(served).toEqual(
			images
				? [{ provider: "ollama-cloud", contextWindow: 4000 }]
				: [
						{ provider: "anthropic", contextWindow: primary.contextWindow },
						{ provider: "ollama-cloud", contextWindow: 4000 },
					],
		);
		expect(session.model?.provider).toBe("anthropic");
	} finally {
		session.dispose();
		rmSync(root, { recursive: true, force: true });
	}
});
