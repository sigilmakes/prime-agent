import { beforeAll, describe, expect, it, vi } from "vitest";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

type SubmitContext = {
	defaultEditor: { onSubmit?: (text: string) => Promise<void> };
	editor: { getText: () => string; setText: (text: string) => void };
	[key: string]: unknown;
};

type Prototype = {
	setupEditorSubmitHandler(this: SubmitContext): void;
};

const prototype = InteractiveMode.prototype as unknown as Prototype;

function makeSubmitContext() {
	const prompt = vi.fn(async () => undefined);
	let editorText = "";
	const context: SubmitContext = {
		defaultEditor: {},
		editor: {
			getText: () => editorText,
			setText: (text: string) => {
				editorText = text;
			},
		},
		agentConnection: { prompt },
		showError: vi.fn(),
		showStatus: vi.fn(),
		echoLocalCommand: vi.fn(),
		handleSessionCommand: vi.fn(async () => undefined),
		showTreeSelector: vi.fn(async () => undefined),
		showSettingsSelector: vi.fn(async () => undefined),
		showHeartbeatManager: vi.fn(async () => undefined),
		submittedInputBehavior: "steer",
		inputSubmissionGeneration: 0,
		inputSubmissionsPending: 0,
		pendingPromptStashReleases: [],
		promptStashState: {},
		pendingSubmittedPromptStash: undefined,
		snapshotPromptStash: vi.fn(() => ({ text: "" })),
		promptStash: undefined,
		promptStashSessionId: "session-1",
		sessionId: "session-1",
		clearShortcutGuide: vi.fn(),
	};
	return context;
}

describe("InteractiveMode no-argument command usage errors", () => {
	beforeAll(() => initTheme("dark"));

	it("rejects arguments to /tree with a usage error instead of prompting", async () => {
		const context = makeSubmitContext();
		const prompt = (context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt;
		prototype.setupEditorSubmitHandler.call(context);
		// The real editor clears its buffer before onSubmit runs; the usage
		// error must put the draft back for editing (as /clear does).
		// The real editor clears its buffer BEFORE invoking onSubmit.
		context.editor.setText("");
		await context.defaultEditor.onSubmit?.("/tree fix the bug");
		expect(context.showError).toHaveBeenCalledWith("Usage: /tree");
		expect(prompt).not.toHaveBeenCalled();
		expect(context.showTreeSelector).not.toHaveBeenCalled();
		expect(context.editor.getText()).toBe("/tree fix the bug");
	});

	it("still opens the tree selector without arguments", async () => {
		const context = makeSubmitContext();
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/tree");
		expect(context.showTreeSelector).toHaveBeenCalledTimes(1);
		expect(context.showError).not.toHaveBeenCalled();
	});

	it.each(["settings", "session", "heartbeats"])("rejects arguments to /%s", async (command) => {
		const context = makeSubmitContext();
		const prompt = (context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt;
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.(`/${command} stray argument`);
		expect(context.showError).toHaveBeenCalledWith(`Usage: /${command}`);
		expect(prompt).not.toHaveBeenCalled();
	});

	it("#17 refreshes only catalogs and reports sanitized per-source results", async () => {
		const context = makeSubmitContext();
		Object.setPrototypeOf(context, InteractiveMode.prototype);
		const refreshModels = vi.fn(async () => ({
			startedAt: 1,
			finishedAt: 2,
			sources: [
				{ source: "curated", status: "updated", modelCount: 3 },
				{ source: "provider", status: "failed", reason: "network" },
			],
		}));
		const getModelCatalog = vi.fn(async () => ({
			models: [{ provider: "new", id: "new-model" }],
			configuredProviders: ["new"],
		}));
		Object.assign(context, {
			agentConnection: { prompt: vi.fn(), refreshModels, getModelCatalog, reload: vi.fn() },
			connectionState: { sessionId: "session-1" },
			connectionModelCatalog: [{ provider: "old", id: "old-model" }],
			connectionConfiguredProviders: new Set(["old"]),
			connectionModelsFetchedAt: Date.now(),
			connectionModelsRefreshVersion: 0,
			isAgentStreaming: () => false,
			isAgentCompacting: () => false,
			showWarning: vi.fn(),
		});
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/refresh");
		expect(refreshModels).toHaveBeenCalledTimes(1);
		expect(getModelCatalog).toHaveBeenCalledTimes(1);
		expect(context.connectionModelCatalog).toEqual([{ provider: "new", id: "new-model" }]);
		expect(context.connectionModelsFetchedAt).toBeGreaterThan(0);
		expect(context.showStatus).toHaveBeenCalledWith("curated: updated (3 models)");
		expect(context.showStatus).toHaveBeenCalledWith("provider: failed (network)");
		expect((context.agentConnection as { reload: ReturnType<typeof vi.fn> }).reload).not.toHaveBeenCalled();
	});

	it.each(["streaming", "compacting"] as const)(
		"#17 refuses model refresh while %s without changing the selected model",
		async (busy) => {
			const context = makeSubmitContext();
			Object.setPrototypeOf(context, InteractiveMode.prototype);
			const refreshModels = vi.fn(async () => ({ startedAt: 1, finishedAt: 2, sources: [] }));
			const getModelCatalog = vi.fn(async () => ({ models: [], configuredProviders: [] }));
			const selectedModel = { provider: "openai", id: "selected" };
			Object.assign(context, {
				agentConnection: { prompt: vi.fn(), refreshModels, getModelCatalog },
				connectionState: { sessionId: "session-1", model: selectedModel },
				connectionModelCatalog: [selectedModel],
				connectionModelsFetchedAt: 123,
				isAgentStreaming: () => busy === "streaming",
				isAgentCompacting: () => busy === "compacting",
				showWarning: vi.fn(),
			});
			prototype.setupEditorSubmitHandler.call(context);
			await context.defaultEditor.onSubmit?.("/refresh");
			expect(refreshModels).not.toHaveBeenCalled();
			expect(getModelCatalog).not.toHaveBeenCalled();
			expect(context.connectionState).toMatchObject({ model: selectedModel });
			expect(context.connectionModelCatalog).toEqual([selectedModel]);
			expect(context.connectionModelsFetchedAt).toBe(123);
			expect(context.showWarning).toHaveBeenCalledWith(expect.stringContaining("Wait for the current response"));
		},
	);

	it("#17 ignores a refresh completed after switching sessions", async () => {
		const context = makeSubmitContext();
		Object.setPrototypeOf(context, InteractiveMode.prototype);
		let finishRefresh: ((report: { startedAt: number; finishedAt: number; sources: [] }) => void) | undefined;
		const refreshModels = vi.fn(
			() =>
				new Promise<{ startedAt: number; finishedAt: number; sources: [] }>((resolve) => {
					finishRefresh = resolve;
				}),
		);
		const getModelCatalog = vi.fn(async () => ({ models: [], configuredProviders: [] }));
		Object.assign(context, {
			agentConnection: { prompt: vi.fn(), refreshModels, getModelCatalog },
			connectionState: { sessionId: "session-1" },
			connectionModelCatalog: [{ provider: "old", id: "old-model" }],
			connectionModelsFetchedAt: 123,
			connectionModelsRefreshVersion: 0,
			isAgentStreaming: () => false,
			isAgentCompacting: () => false,
		});
		prototype.setupEditorSubmitHandler.call(context);
		const submission = context.defaultEditor.onSubmit?.("/refresh");
		expect(refreshModels).toHaveBeenCalledTimes(1);
		context.connectionState = { sessionId: "session-2" };
		finishRefresh?.({ startedAt: 1, finishedAt: 2, sources: [] });
		await submission;
		expect(getModelCatalog).not.toHaveBeenCalled();
		expect(context.connectionModelsFetchedAt).toBe(123);
		expect(context.connectionModelCatalog).toEqual([{ provider: "old", id: "old-model" }]);
	});

	it("rejects /refresh arguments without sending a model prompt", async () => {
		const context = makeSubmitContext();
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/refresh all");
		expect(context.showError).toHaveBeenCalledWith("Usage: /refresh");
		expect(context.editor.getText()).toBe("/refresh all");
		expect((context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt).not.toHaveBeenCalled();
	});

	it("toggles /speed on and off, parses explicit args, and rejects invalid arguments", async () => {
		const context = makeSubmitContext();
		Object.setPrototypeOf(context, InteractiveMode.prototype); // runs the real setSpeedDisplay
		const footer = { setSpeedEnabled: vi.fn(), setSpeedText: vi.fn() };
		Object.assign(context, { footer, speedDisplayEnabled: false });
		Object.assign(context, { ui: { requestRender: vi.fn() }, uiServices: { settingsManager: {} } });
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/speed");
		expect(footer.setSpeedEnabled).toHaveBeenLastCalledWith(true);
		await context.defaultEditor.onSubmit?.("/speed off");
		expect(footer.setSpeedEnabled).toHaveBeenLastCalledWith(false);
		expect(footer.setSpeedText).toHaveBeenCalledWith(undefined);
		await context.defaultEditor.onSubmit?.("/speed banana");
		expect(context.showError).toHaveBeenCalledWith("Usage: /speed [on|off]");
		expect((context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt).not.toHaveBeenCalled();
	});
});
