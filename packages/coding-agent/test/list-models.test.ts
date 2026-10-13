import type { Api, Model } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, test, vi } from "vitest";
import { listModels } from "../src/cli/list-models.js";
import type { ModelRegistry } from "../src/core/model-registry.js";

const model: Model<Api> = {
	id: "example/chat",
	name: "Example Chat",
	provider: "openrouter",
	api: "openai-completions",
	baseUrl: "https://openrouter.ai/api/v1",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 128_000,
	maxTokens: 4096,
};

function registryWithStatus(status: ReturnType<ModelRegistry["getProviderCatalogStatus"]>): ModelRegistry {
	return {
		getError: () => undefined,
		refreshAvailableModels: async () => [model],
		getProviderCatalogStatus: () => status,
	} as unknown as ModelRegistry;
}

describe("listModels catalog health", () => {
	afterEach(() => {
		vi.restoreAllMocks();
		vi.useRealTimers();
	});

	test("shows source, age, and safe error next to listed provider models", async () => {
		vi.useFakeTimers();
		vi.setSystemTime(new Date("2026-10-13T12:00:00Z"));
		const log = vi.spyOn(console, "log").mockImplementation(() => {});
		await listModels(
			registryWithStatus({
				state: "error",
				source: "disk",
				fetchedAt: Date.now() - 120_000,
				error: "Catalog request failed with status 503",
			}),
		);
		const output = log.mock.calls.map(([line]) => String(line)).join("\n");
		expect(output).toContain(model.id);
		expect(output).toMatch(/catalog openrouter: disk, error, 2m old/);
		expect(output).toContain("Catalog request failed with status 503");
		expect(output).not.toContain("catalog anthropic:");
	});

	test("labels the pinned fallback when a provider has not fetched yet", async () => {
		const log = vi.spyOn(console, "log").mockImplementation(() => {});
		await listModels(registryWithStatus({ state: "idle", source: "none" }));
		const output = log.mock.calls.map(([line]) => String(line)).join("\n");
		expect(output).toContain("catalog openrouter: bundled fallback, idle, not fetched");
	});
});
