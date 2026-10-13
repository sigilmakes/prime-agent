import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { setImmediate as nextTurn } from "node:timers/promises";
import type { Model } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, test, vi } from "vitest";
import {
	buildPrimeInferenceModels,
	PRIME_INFERENCE_BASE_URL,
	readCachedPrimeInferenceModels,
	readCachedPrimeInferenceModelsWithStatus,
	refreshPrimeInferenceModels,
	refreshPrimeInferenceModelsWithOutcome,
} from "../src/core/prime-inference-model-catalog.js";
import {
	fetchAuthorizedPrivatePrimeInferenceModels,
	isPrivatePrimeInferenceModel,
} from "../src/core/prime-inference-models.js";

const directories: string[] = [];
const model = (id: string, provider = "prime-inference"): Model<"openai-completions"> => ({
	id,
	name: `Bundled ${id}`,
	api: "openai-completions",
	provider,
	baseUrl: provider === "prime-inference" ? PRIME_INFERENCE_BASE_URL : "https://example.com/v1",
	reasoning: true,
	thinkingLevelMap: { high: "high" },
	input: ["text"],
	cost: { input: 9, output: 10, cacheRead: 0.9, cacheWrite: 11.25 },
	contextWindow: 100_000,
	maxTokens: 10_000,
	featured: true,
	compat: { supportsDeveloperRole: false, maxTokensField: "max_tokens" },
});

const entry = (id: string) => ({
	id,
	input: 1,
	output: 2,
	contextWindow: 200_000,
	maxTokens: 20_000,
	vision: true,
	reasoning: false,
});

const effortEntry = (id: string) => ({
	...entry(id),
	reasoning: true,
	supportedParameters: ["max_tokens", "reasoning", "reasoning_effort"],
	reasoningEfforts: ["low", "high", "max"],
	reasoningMandatory: true,
});

const toggleEntry = (id: string) => ({
	...entry(id),
	reasoning: true,
	supportedParameters: ["max_tokens", "reasoning", "include_reasoning"],
});

const payloadEntry = (
	id: string,
	specs: unknown = {
		context_window: 200_000,
		max_output_tokens: 20_000,
		modalities: { input: ["text", "image"], output: ["text"] },
		supports_reasoning: false,
	},
) => ({
	id,
	display_name: `Live ${id}`,
	pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
	specs,
});

const response = (...data: unknown[]) => new Response(JSON.stringify({ object: "list", data }));

afterEach(() => {
	for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("Prime Inference model catalog", () => {
	test.each([
		{ id: "internal/model", provider: "prime-inference", private: true },
		{ id: "INTERNAL/model", provider: "prime-inference", private: true },
		{ id: "dev/model", provider: "prime-inference", private: true },
		{ id: "vendor/model:deployment", provider: "prime-inference", private: true },
		{ id: "public/model", provider: "prime-inference", private: false },
		{ id: "vendor/model:deployment", provider: "openrouter", private: false },
	])("isPrivatePrimeInferenceModel($id, $provider) is $private", ({ id, provider, ...expected }) => {
		expect(isPrivatePrimeInferenceModel(model(id, provider))).toBe(expected.private);
	});

	test("drops private routes from the public catalog and rejects thin coverage", () => {
		const bundled = [model("one"), model("two"), model("three")];
		expect(
			buildPrimeInferenceModels(bundled, [
				entry("internal/private"),
				entry("dev/private"),
				entry("poolside/model:deployment"),
				entry("one"),
			]),
		).toBeUndefined();
		expect(
			buildPrimeInferenceModels(bundled, [entry("new/one"), entry("new/two"), entry("new/three")]),
		).toBeUndefined();
	});

	it.each([
		{
			name: "effort route",
			entry: () => effortEntry("z-ai/glm-5.3"),
			thinkingFormat: undefined,
			supportsReasoningEffort: true,
			map: { off: null, minimal: null, low: "low", medium: null, high: "high", xhigh: null, max: "max" },
		},
		{
			name: "toggle route",
			entry: () => toggleEntry("z-ai/glm-4.7"),
			thinkingFormat: "openrouter",
			supportsReasoningEffort: false,
			map: { minimal: null, low: null, medium: null, high: "high", xhigh: null, max: null },
		},
		{
			name: "effort route without declared efforts keeps the template map",
			entry: () => ({
				...entry("z-ai/glm-5.3"),
				reasoning: true,
				supportedParameters: ["max_tokens", "reasoning", "reasoning_effort"],
				reasoningMandatory: false,
			}),
			thinkingFormat: undefined,
			supportsReasoningEffort: true,
			map: { high: "high" },
		},
		{
			name: "no-parameter route keeps the stale template",
			entry: () => entry("z-ai/glm-5.3"),
			thinkingFormat: "zai",
			supportsReasoningEffort: undefined,
			map: { high: "high" },
		},
		{
			name: "reasoning-free route drops the stale template",
			entry: () => ({ ...entry("qwen/qwen3-coder"), supportedParameters: ["max_tokens"] }),
			thinkingFormat: undefined,
			supportsReasoningEffort: false,
			map: undefined,
		},
	])("$name", ({ entry: makeEntry, ...expected }) => {
		const liveEntry = makeEntry();
		const template = model(liveEntry.id);
		const stale = { ...template, compat: { ...template.compat, thinkingFormat: "zai" as const } };
		const [live] = buildPrimeInferenceModels([stale], [liveEntry], { minimumModels: 0 }) ?? [];
		expect(live?.compat?.thinkingFormat).toBe(expected.thinkingFormat);
		expect(live?.compat?.supportsReasoningEffort).toBe(expected.supportsReasoningEffort);
		if (expected.map) expect(live?.thinkingLevelMap).toEqual(expected.map);
		else expect(live?.thinkingLevelMap).toBeUndefined();
	});

	test("gives new live models the conservative default compat plus declared controls", () => {
		const models =
			buildPrimeInferenceModels([], [effortEntry("vendor/new"), entry("vendor/plain")], { minimumModels: 0 }) ?? [];
		expect(models.map((m) => m.compat?.supportsReasoningEffort)).toEqual([true, false]);
	});

	test("caches valid responses and falls back to the cache when the fetch fails", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-models-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("vendor/model")];
		const fetched = await refreshPrimeInferenceModels(cachePath, bundled, {
			fetchFn: vi.fn(async () => response(payloadEntry("vendor/model"))),
		});
		expect(fetched?.[0]?.name).toBe("Live vendor/model");
		expect(JSON.parse(readFileSync(cachePath, "utf8")).payload.data).toHaveLength(1);
		const fallback = await refreshPrimeInferenceModels(cachePath, bundled, {
			fetchFn: vi.fn(async () => {
				throw new Error("offline");
			}),
		});
		expect(fallback?.[0]?.name).toBe("Live vendor/model");
	});

	test("uses a valid flat legacy cache when the new cache has insufficient coverage", () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-models-legacy-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "prime-inference-models-cache.json");
		mkdirSync(join(directory, "models"));
		const bundled = [model("vendor/model")];
		writeFileSync(cachePath, JSON.stringify({ object: "list", data: [payloadEntry("unrelated/model")] }));
		writeFileSync(
			join(directory, "prime-inference-models-cache.json"),
			JSON.stringify({ object: "list", data: [payloadEntry("vendor/model")] }),
		);
		expect(readCachedPrimeInferenceModels(cachePath, bundled)?.[0]?.name).toBe("Live vendor/model");
	});

	test("keeps authenticated private routes with complete metadata and sends the auth headers", async () => {
		const fetchFn = vi.fn(async (_url: string | URL | Request, init?: RequestInit) => {
			expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer secret");
			expect(new Headers(init?.headers).get("X-Prime-Team-ID")).toBe("team");
			return response(
				payloadEntry("public/model"),
				payloadEntry("internal/model"),
				payloadEntry("dev/model"),
				payloadEntry("poolside/model:deployment"),
				payloadEntry("internal/incomplete", null),
				{ id: "internal/glm-5.2-fast" },
			);
		});
		const models = await fetchAuthorizedPrivatePrimeInferenceModels(
			"secret",
			{ "X-Prime-Team-ID": "team" },
			new Set(["public/model"]),
			fetchFn,
		);
		expect(models.map(({ id }) => id)).toEqual([
			"internal/model",
			"dev/model",
			"poolside/model:deployment",
			"internal/glm-5.2-fast",
		]);
	});

	test("treats rejected authenticated requests as no private access", async () => {
		const models = await fetchAuthorizedPrivatePrimeInferenceModels(
			"bad",
			{ "X-Prime-Team-ID": "team" },
			new Set(),
			vi.fn(async () => new Response(null, { status: 403 })),
		);
		expect(models).toEqual([]);
	});

	test("reports offline, cached fallback, auth rejection, and partial catalog failure", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-outcomes-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one"), model("two"), model("three")];
		const fetchFn = vi.fn(async () => response(payloadEntry("one"), payloadEntry("two")));
		const updated = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, { fetchFn, force: true });
		expect(updated.status).toBe("updated");
		expect(updated.models?.map(({ id }) => id)).toEqual(["one", "two"]);
		expect((await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, { offline: true })).status).toBe(
			"offline",
		);
		expect(fetchFn).toHaveBeenCalledTimes(1);
		const partial = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			fetchFn: vi.fn(async () => response(payloadEntry("three"))),
		});
		expect(partial.status).toBe("cached-fallback");
		expect(partial.models?.map(({ id }) => id)).toEqual(["one", "two"]);
		const denied = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			fetchFn: vi.fn(async () => new Response(null, { status: 403 })),
		});
		expect(denied.status).toBe("auth-rejected");
		expect(denied.models?.map(({ id }) => id)).toEqual(["one", "two"]);
		expect(
			(
				await refreshPrimeInferenceModelsWithOutcome(join(directory, "empty.json"), bundled, {
					fetchFn: vi.fn(async () => {
						throw new Error("network failure");
					}),
				})
			).status,
		).toBe("failed");
		expect(JSON.parse(readFileSync(cachePath, "utf8")).payload.data).toHaveLength(2);
	});

	test("different headers cannot join or overwrite a same-path in-flight request", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-auth-swap-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one")];
		let releaseOld: ((value: Response) => void) | undefined;
		let oldStarted: (() => void) | undefined;
		const started = new Promise<void>((resolve) => {
			oldStarted = resolve;
		});
		const fetchFn = vi.fn(async (_url: string | URL | Request, init?: RequestInit): Promise<Response> => {
			if (new Headers(init?.headers).get("authorization") === "Bearer old") {
				oldStarted?.();
				return new Promise<Response>((resolve) => {
					releaseOld = resolve;
				});
			}
			return response(payloadEntry("one"), payloadEntry("internal/secret"));
		});
		const old = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			headers: { Authorization: "Bearer old" },
			fetchFn,
		});
		await started;
		const newer = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			headers: { Authorization: "Bearer new" },
			fetchFn,
		});
		expect(newer.status).toBe("updated");
		expect(newer.models?.map(({ id }) => id)).toEqual(["one"]);
		releaseOld?.(response({ ...payloadEntry("one"), display_name: "Stale one" }));
		expect((await old).status).toBe("superseded");
		expect(fetchFn).toHaveBeenCalledTimes(2);
		expect(readCachedPrimeInferenceModels(cachePath, bundled)?.[0]?.name).toBe("Live one");
	});

	test("forced refresh supersedes pending fetch, and cancellation leaves the cache alone", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-force-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one")];
		let release: ((value: Response) => void) | undefined;
		let started: (() => void) | undefined;
		const ready = new Promise<void>((resolve) => {
			started = resolve;
		});
		const old = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			fetchFn: vi.fn(async () => {
				started?.();
				return new Promise<Response>((resolve) => {
					release = resolve;
				});
			}),
		});
		await ready;
		const joined = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			fetchFn: vi.fn(async () => {
				throw new Error("must share in-flight request");
			}),
		});
		const newer = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			headers: { "X-Prime-Team-ID": "new-team" },
			fetchFn: vi.fn(async () => response(payloadEntry("one"))),
		});
		expect(newer.status).toBe("updated");
		release?.(response(payloadEntry("old/model")));
		expect((await old).status).toBe("superseded");
		expect((await joined).status).toBe("superseded");
		const controller = new AbortController();
		controller.abort();
		const fetchFn = vi.fn(async () => response(payloadEntry("one")));
		const cancelled = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			signal: controller.signal,
			fetchFn,
		});
		expect(cancelled.status).toBe("cancelled");
		expect(fetchFn).not.toHaveBeenCalled();
		expect(JSON.parse(readFileSync(cachePath, "utf8")).payload.data[0].id).toBe("one");
	});

	test("cancellation during a fetch does not publish its late response", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-abort-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one")];
		let release: ((value: Response) => void) | undefined;
		let started: (() => void) | undefined;
		const ready = new Promise<void>((resolve) => {
			started = resolve;
		});
		const controller = new AbortController();
		const pending = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			signal: controller.signal,
			fetchFn: vi.fn(async () => {
				started?.();
				return new Promise<Response>((resolve) => {
					release = resolve;
				});
			}),
		});
		await ready;
		controller.abort();
		expect((await pending).status).toBe("cancelled");
		const fresh = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			fetchFn: vi.fn(async () => response({ ...payloadEntry("one"), display_name: "Fresh one" })),
		});
		expect(fresh.status).toBe("updated");
		release?.(response({ ...payloadEntry("one"), display_name: "Stale one" }));
		await nextTurn();
		expect(readCachedPrimeInferenceModels(cachePath, bundled)?.[0]?.name).toBe("Fresh one");
	});

	it.each(["first", "second"])("one same-scope %s waiter aborts while the other succeeds", async (which) => {
		const directory = mkdtempSync(join(tmpdir(), "prime-shared-abort-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one")];
		let release: ((value: Response) => void) | undefined;
		const fetchFn = vi.fn(
			async (_url: string | URL | Request, _init?: RequestInit) =>
				new Promise<Response>((resolve) => {
					release = resolve;
				}),
		);
		const firstController = new AbortController();
		const secondController = new AbortController();
		const first = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			signal: firstController.signal,
			fetchFn,
		});
		const second = refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			signal: secondController.signal,
			fetchFn,
		});
		const aborted = which === "first" ? first : second;
		const survivor = which === "first" ? second : first;
		(which === "first" ? firstController : secondController).abort();
		expect((await aborted).status).toBe("cancelled");
		expect(fetchFn).toHaveBeenCalledTimes(1);
		expect(fetchFn.mock.calls[0]?.[1]?.signal?.aborted).toBe(false);
		release?.(response(payloadEntry("one")));
		expect((await survivor).status).toBe("updated");
		expect(readCachedPrimeInferenceModels(cachePath, bundled)?.[0]?.name).toBe("Live one");
	});

	test("a second reader sees a durable checkedAt without refetching, while legacy raw caches lack it", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-checked-at-"));
		directories.push(directory);
		const cachePath = join(directory, "models", "cache.json");
		const bundled = [model("one")];
		const since = Date.now() - 1;
		const fetchFn = vi.fn(async () => response(payloadEntry("one")));
		expect((await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, { force: true, fetchFn })).status).toBe(
			"updated",
		);
		// A fresh independent reader has only the persisted file, not the leader's in-memory status.
		const follower = readCachedPrimeInferenceModelsWithStatus(cachePath, bundled);
		expect(follower?.models[0]?.name).toBe("Live one");
		expect(follower?.checkedAt).toBeGreaterThanOrEqual(since);
		const followerFetch = vi.fn(async () => {
			throw new Error("second network call");
		});
		if (!follower?.checkedAt || follower.checkedAt < since) {
			await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, { force: true, fetchFn: followerFetch });
		}
		expect(followerFetch).not.toHaveBeenCalled();
		expect(
			(await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, { offline: true })).models?.[0]?.name,
		).toBe("Live one");
		writeFileSync(cachePath, JSON.stringify({ object: "list", data: [payloadEntry("one")] }));
		expect(readCachedPrimeInferenceModelsWithStatus(cachePath, bundled)?.checkedAt).toBeUndefined();
		expect(readCachedPrimeInferenceModelsWithStatus(cachePath, bundled)?.models[0]?.name).toBe("Live one");
	});

	test("invalid payload and invalid timestamps cannot advertise a checkedAt", () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-invalid-metadata-"));
		directories.push(directory);
		const cachePath = join(directory, "cache.json");
		const bundled = [model("one")];
		for (const [payload, checkedAt] of [
			[{ object: "list", data: [payloadEntry("other")] }, Date.now()],
			[{ object: "list", data: [payloadEntry("one")] }, Date.now() + 100_000],
			[{ object: "list", data: [payloadEntry("one")] }, "future"],
		] as const) {
			writeFileSync(cachePath, JSON.stringify({ format: "prime-inference-catalog-v1", payload, checkedAt }));
			expect(readCachedPrimeInferenceModelsWithStatus(cachePath, bundled)?.checkedAt).toBeUndefined();
		}
	});

	test("cache write failure returns memory-only rather than a durable update", async () => {
		const directory = mkdtempSync(join(tmpdir(), "prime-write-failure-"));
		directories.push(directory);
		const cachePath = join(directory, "cache.json");
		mkdirSync(cachePath);
		const bundled = [model("one")];
		const result = await refreshPrimeInferenceModelsWithOutcome(cachePath, bundled, {
			force: true,
			fetchFn: vi.fn(async () => response(payloadEntry("one"))),
		});
		expect(result.status).toBe("memory-only");
		expect(result.models?.[0]?.name).toBe("Live one");
		expect(readCachedPrimeInferenceModelsWithStatus(cachePath, bundled)).toBeUndefined();
	});
});
