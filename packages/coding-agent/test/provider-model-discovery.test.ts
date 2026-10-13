import { describe, expect, test } from "vitest";
import { getBundledModels } from "../src/core/bundled-model-catalog.js";
import {
	discoveryHeaders,
	mergeProviderDiscoveryPayload,
	parseProviderDiscovery,
} from "../src/core/provider-model-discovery.js";

describe("issue #5 provider-owned model discovery", () => {
	const bundled = getBundledModels();

	test("OpenRouter metadata cannot redirect transport or replace installed compatibility and capabilities", () => {
		const template = bundled.find((model) => model.provider === "openrouter")!;
		const parsed = parseProviderDiscovery(
			"openrouter",
			{
				data: [
					{
						id: template.id,
						name: "Updated",
						api: "other",
						baseUrl: "https://hostile.invalid",
						headers: { Authorization: "Bearer attacker" },
						context_length: 256000,
						pricing: { prompt: "0.000001", completion: "0.000002" },
						architecture: { input_modalities: ["text"] },
					},
				],
			},
			bundled,
		);
		const model = parsed.find((item) => item.id === template.id)!;
		expect(model).toMatchObject({
			name: "Updated",
			api: template.api,
			baseUrl: template.baseUrl,
			headers: template.headers,
			compat: template.compat,
			input: template.input,
			contextWindow: 256000,
			cost: { input: 1, output: 2 },
		});
	});

	test("partial listings retain last-good rows and malformed listing fails without replacing snapshot", () => {
		const row = (id: string, name: string) => ({
			id,
			name,
			context_length: 32000,
			top_provider: { max_completion_tokens: 4096 },
			pricing: { prompt: "0.000001", completion: "0.000002" },
			architecture: { input_modalities: ["text"], output_modalities: ["text"] },
		});
		const first = parseProviderDiscovery(
			"openrouter",
			{ data: [row("test/new-a", "A"), row("test/new-b", "B")] },
			bundled,
		);
		const next = parseProviderDiscovery(
			"openrouter",
			{ data: [row("test/new-a", "A2"), { id: "\ninvalid" }] },
			bundled,
			first,
		);
		expect(next.find((model) => model.id === "test/new-a")?.name).toBe("A2");
		expect(next.find((model) => model.id === "test/new-b")?.name).toBe("B");
		expect(() => parseProviderDiscovery("openrouter", { data: [{ name: "missing ID" }] }, bundled, next)).toThrow();
	});

	test("unknown IDs cannot borrow model-specific metadata from a bundled template", () => {
		const template = bundled.find((model) => model.provider === "openrouter")!;
		const complete = {
			context_length: 32000,
			top_provider: { max_completion_tokens: 4096 },
			pricing: { prompt: "0.000001", completion: "0.000002" },
		};
		const parsed = parseProviderDiscovery(
			"openrouter",
			{
				data: [
					{ id: "test/incomplete", name: "Incomplete" },
					{
						id: "test/image-only",
						...complete,
						architecture: { input_modalities: ["image"], output_modalities: ["text"] },
					},
					{
						id: "test/audio-output",
						...complete,
						architecture: { input_modalities: ["text"], output_modalities: ["audio"] },
					},
					{ id: "test/no-architecture", ...complete },
					{
						id: "test/complete",
						...complete,
						architecture: { input_modalities: ["text"], output_modalities: ["text"] },
						api: "attacker",
						baseUrl: "https://attacker.invalid",
						headers: { Authorization: "attacker" },
					},
				],
			},
			bundled,
		);
		for (const id of ["test/incomplete", "test/image-only", "test/audio-output", "test/no-architecture"]) {
			expect(parsed.some((model) => model.id === id)).toBe(false);
		}
		expect(parsed.find((model) => model.id === "test/complete")).toMatchObject({
			name: "test/complete",
			contextWindow: 32000,
			maxTokens: 4096,
			reasoning: false,
			input: ["text"],
			api: template.api,
			baseUrl: template.baseUrl,
			headers: undefined,
			compat: undefined,
			thinkingLevelMap: undefined,
			cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
		});
	});

	test("normalized OpenRouter cache retains text output without persisting transport fields", () => {
		const payload = mergeProviderDiscoveryPayload(
			"openrouter",
			{
				data: [
					{
						id: "test/text",
						context_length: 32000,
						top_provider: { max_completion_tokens: 4096 },
						pricing: { prompt: "0.000001", completion: "0.000002" },
						architecture: { input_modalities: ["text"], output_modalities: ["text"] },
						baseUrl: "https://attacker.invalid",
						headers: { Authorization: "attacker" },
					},
				],
			},
			undefined,
			bundled,
		);
		expect(payload).toMatchObject({
			data: [
				{
					architecture: {
						input_modalities: ["text"],
						output_modalities: ["text"],
					},
				},
			],
		});
		expect(JSON.stringify(payload)).not.toContain("attacker");
		expect(parseProviderDiscovery("openrouter", payload, bundled).some((model) => model.id === "test/text")).toBe(
			true,
		);
	});

	test.each([
		["output-only audio", { output_modalities: ["audio"] }],
		["input-only image", { input_modalities: ["image"] }],
	] as const)("explicit %s revokes a previous OpenRouter chat model across snapshots", (_name, architecture) => {
		const firstPayload = {
			data: [
				{
					id: "test/transition",
					context_length: 32000,
					top_provider: { max_completion_tokens: 4096 },
					pricing: { prompt: "0.000001", completion: "0.000002" },
					architecture: { input_modalities: ["text"], output_modalities: ["text"] },
				},
			],
		};
		const first = parseProviderDiscovery("openrouter", firstPayload, bundled);
		expect(first.some((model) => model.id === "test/transition")).toBe(true);
		const nonChat = { data: [{ id: "test/transition", architecture }] };
		expect(
			parseProviderDiscovery("openrouter", nonChat, bundled, first).some((model) => model.id === "test/transition"),
		).toBe(false);
		const persisted = mergeProviderDiscoveryPayload("openrouter", nonChat, firstPayload, bundled);
		expect(
			parseProviderDiscovery("openrouter", persisted, bundled).some((model) => model.id === "test/transition"),
		).toBe(false);
		const omitted = parseProviderDiscovery("openrouter", { data: [{ id: "test/transition" }] }, bundled, first);
		expect(omitted.some((model) => model.id === "test/transition")).toBe(true);
	});

	test("Google filters non-generative rows and maps documented token limits; OpenAI lists IDs only", () => {
		const knownGoogle = bundled.find((model) => model.provider === "google")!;
		const google = parseProviderDiscovery(
			"google",
			{
				models: [
					{
						name: `models/${knownGoogle.id}`,
						displayName: "Known Gemini",
						inputTokenLimit: 500000,
						outputTokenLimit: 8192,
						supportedGenerationMethods: ["generateContent"],
					},
					{
						name: "models/gemini-unknown",
						displayName: "Unknown Gemini",
						inputTokenLimit: 500000,
						outputTokenLimit: 8192,
						supportedGenerationMethods: ["generateContent"],
					},
					{ name: "models/embedding", supportedGenerationMethods: ["embedContent"] },
				],
			},
			bundled,
		);
		expect(google.find((model) => model.id === knownGoogle.id)).toMatchObject({
			contextWindow: 500000,
			maxTokens: 8192,
			name: "Known Gemini",
		});
		expect(google.some((model) => model.id === "gemini-unknown")).toBe(false);
		expect(google.some((model) => model.id === "embedding")).toBe(false);
		const knownOpenAI = bundled.find((model) => model.provider === "openai")!;
		const openai = parseProviderDiscovery(
			"openai",
			{ data: [{ id: knownOpenAI.id }, { id: "gpt-unknown", owned_by: "openai" }] },
			bundled,
		);
		expect(openai.some((model) => model.id === "gpt-unknown")).toBe(false);
		expect(openai.find((model) => model.id === knownOpenAI.id)).toBeDefined();
		const knownAnthropic = bundled.find((model) => model.provider === "anthropic")!;
		const anthropic = parseProviderDiscovery(
			"anthropic",
			{
				data: [
					{ id: knownAnthropic.id, display_name: "Known" },
					{ id: "claude-unknown", display_name: "Unknown" },
				],
			},
			bundled,
		);
		expect(anthropic.find((model) => model.id === knownAnthropic.id)?.name).toBe("Known");
		expect(anthropic.some((model) => model.id === "claude-unknown")).toBe(false);
		expect(discoveryHeaders("openai")).toBeUndefined();
		expect(discoveryHeaders("anthropic", "key")).toEqual({ "x-api-key": "key", "anthropic-version": "2023-06-01" });
		expect(discoveryHeaders("google", "key")).toEqual({ "x-goog-api-key": "key" });
	});
});
