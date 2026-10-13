import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import {
	applyOllamaCloudModelOverrides,
	loadOllamaCloudConfig,
	overlayOllamaCloudRequest,
	resolveOllamaCloudInferenceParams,
} from "../src/core/ollama-cloud-config.js";

let root: string;
let agentDir: string;
let cwd: string;

beforeEach(() => {
	root = mkdtempSync(join(tmpdir(), "ollama-cloud-config-"));
	agentDir = join(root, "global");
	cwd = join(root, "project");
	mkdirSync(agentDir);
	mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
});

afterEach(() => {
	vi.restoreAllMocks();
	rmSync(root, { recursive: true, force: true });
});

function writeGlobal(value: unknown): void {
	writeFileSync(join(agentDir, "ollama-cloud.json"), JSON.stringify(value));
}

function writeProject(value: unknown): void {
	writeFileSync(join(cwd, ".prime", "agent", "ollama-cloud.json"), JSON.stringify(value));
}

describe("Ollama Cloud config", () => {
	test("merges global and project params and model caps per key; overlays only the outgoing body", () => {
		writeGlobal({
			inferenceParams: { temperature: 0.2, top_p: 0.9, seed: 12 },
			models: { "glm-5": { top_k: 10, temperature: 0.4 }, other: { seed: 20 } },
			modelOverrides: { "glm-5": { contextWindow: 300000, maxTokens: 32000 }, other: { maxTokens: 1000 } },
		});
		writeProject({
			inferenceParams: { temperature: 0.3 },
			models: { "glm-5": { top_k: 25, stop: ["END"] } },
			modelOverrides: { "glm-5": { maxTokens: 16000 } },
		});
		const config = loadOllamaCloudConfig(cwd, agentDir);
		expect(config.models?.other).toEqual({ seed: 20 });
		expect(resolveOllamaCloudInferenceParams(config, "unknown")).toEqual({ temperature: 0.3, top_p: 0.9, seed: 12 });
		expect(resolveOllamaCloudInferenceParams(config, "glm-5")).toEqual({
			temperature: 0.4,
			top_p: 0.9,
			seed: 12,
			top_k: 25,
			stop: ["END"],
		});
		const body = Object.freeze({
			model: "glm-5",
			messages: Object.freeze([{ role: "user", content: "hi" }]),
			temperature: 1,
			stream: true,
		});
		expect(overlayOllamaCloudRequest(body, config, "glm-5")).toEqual({
			model: "glm-5",
			messages: [{ role: "user", content: "hi" }],
			stream: true,
			temperature: 0.4,
			top_p: 0.9,
			seed: 12,
			top_k: 25,
			stop: ["END"],
		});
		expect(body.temperature).toBe(1);
		const models = Object.freeze([
			Object.freeze({ id: "glm-5", contextWindow: 200000, maxTokens: 32000, name: "GLM" }),
			Object.freeze({ id: "absent", contextWindow: 100000, maxTokens: 4000, name: "Other" }),
		]);
		const overridden = applyOllamaCloudModelOverrides([...models], config.modelOverrides);
		expect(overridden).toEqual([{ id: "glm-5", contextWindow: 300000, maxTokens: 16000, name: "GLM" }, models[1]]);
		expect(models[0].maxTokens).toBe(32000);
		expect(overridden[1]).toBe(models[1]);
	});

	test("drops unknown fields and invalid sampler and cap values without letting them into the request", () => {
		writeGlobal({
			inferenceParams: {
				temperature: "0.4",
				top_p: 0.8,
				seed: null,
				stop: ["ok", 3],
				model: "hijack",
				messages: [],
				max_tokens: 4096,
			},
			models: { "glm-5": { top_k: "100", stop: "DONE", stream: false }, ignored: [], empty: { foo: 1 } },
			modelOverrides: {
				"glm-5": { contextWindow: -1, maxTokens: 1.2, api: "hijack" },
				good: { contextWindow: 50000, maxTokens: 2000 },
				invalid: { maxTokens: 0, contextWindow: "100" },
			},
		});
		const config = loadOllamaCloudConfig(cwd, agentDir);
		expect(config).toEqual({
			inferenceParams: { top_p: 0.8, max_tokens: 4096 },
			models: { "glm-5": { stop: "DONE" } },
			modelOverrides: { good: { contextWindow: 50000, maxTokens: 2000 } },
		});
		expect(overlayOllamaCloudRequest({ model: "real", messages: [], stream: true }, config, "glm-5")).toEqual({
			model: "real",
			messages: [],
			stream: true,
			top_p: 0.8,
			max_tokens: 4096,
			stop: "DONE",
		});
		expect(overlayOllamaCloudRequest(null, config, "glm-5")).toBeUndefined();
		expect(overlayOllamaCloudRequest([], config, "glm-5")).toBeUndefined();
	});

	test("rejects non-finite JSON numbers, invalid stop entries, and unsafe model caps", () => {
		writeFileSync(
			join(agentDir, "ollama-cloud.json"),
			'{"inferenceParams":{"temperature":1e999,"top_p":0.6,"stop":["ok",null]},' +
				'"models":{"glm-5":{"seed":1e999,"stop":[]}},' +
				'"modelOverrides":{"glm-5":{"contextWindow":1e999,"maxTokens":9007199254740992}}}',
		);
		const config = loadOllamaCloudConfig(cwd, agentDir);
		expect(config).toEqual({
			inferenceParams: { top_p: 0.6 },
			models: { "glm-5": { stop: [] } },
			modelOverrides: undefined,
		});
		expect(overlayOllamaCloudRequest({ model: "glm-5", temperature: 1 }, config, "glm-5")).toEqual({
			model: "glm-5",
			temperature: 1,
			top_p: 0.6,
			stop: [],
		});
	});

	test("invalid JSON and non-object files fall back independently without aborting", () => {
		writeFileSync(join(agentDir, "ollama-cloud.json"), "{bad json");
		writeProject({ inferenceParams: { seed: 1 } });
		const error = vi.spyOn(console, "error").mockImplementation(() => undefined);
		expect(loadOllamaCloudConfig(cwd, agentDir).inferenceParams).toEqual({ seed: 1 });
		expect(error).toHaveBeenCalledTimes(1);
		writeGlobal({ inferenceParams: { temperature: 0.5 } });
		writeProject(["not a config"]);
		expect(loadOllamaCloudConfig(cwd, agentDir).inferenceParams).toEqual({ temperature: 0.5 });
		writeGlobal(null);
		expect(loadOllamaCloudConfig(cwd, agentDir).inferenceParams).toBeUndefined();
		expect(overlayOllamaCloudRequest({ model: "x" }, loadOllamaCloudConfig(cwd, agentDir), "x")).toBeUndefined();
	});
});
