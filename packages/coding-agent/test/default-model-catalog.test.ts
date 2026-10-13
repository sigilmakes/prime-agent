import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Api, Model } from "@earendil-works/pi-ai";
import { getModels } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

const cacheRoots: string[] = [];

beforeEach(() => {
	vi.resetModules();
});

function isolatedCache(): void {
	const root = mkdtempSync(join(tmpdir(), "default-model-catalog-"));
	cacheRoots.push(root);
	vi.stubEnv("PRIME_AGENT_CODING_AGENT_DIR", join(root, "agent"));
}

function defaultResponse(id: string, etag?: string): Response {
	return new Response(JSON.stringify({ schemaVersion: 1, defaultModel: id }), {
		status: 200,
		headers: etag ? { etag } : undefined,
	});
}

function model(provider: string, id: string): Model<Api> {
	const base = getModels("openai")[0]!;
	return { ...base, provider, id, name: `${provider}/${id}` } as unknown as Model<Api>;
}

afterEach(() => {
	vi.unstubAllGlobals();
	vi.unstubAllEnvs();
	for (const dir of cacheRoots.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("catalog-defined default model", () => {
	test("prefers the catalog default over the compiled constants", async () => {
		isolatedCache();

		vi.stubGlobal(
			"fetch",
			vi.fn(async (input: string | URL | Request) => {
				const url = String(input);
				if (url.endsWith("defaults.v1.json")) {
					return new Response(JSON.stringify({ schemaVersion: 1, defaultModel: "anthropic/claude-fable-5" }));
				}
				throw new Error(`unexpected fetch ${url}`);
			}),
		);

		const { refreshDefaultModelCatalog } = await import("../src/core/default-model-catalog.js");
		await refreshDefaultModelCatalog(true);

		const { getPreferredDefaultModelId, resolvePreferredDefaultModel } = await import(
			"../src/core/default-model-catalog.js"
		);
		expect(getPreferredDefaultModelId()).toBe("anthropic/claude-fable-5");
		const resolved = resolvePreferredDefaultModel(getPreferredDefaultModelId(), [
			model("anthropic", "claude-fable-5"),
			model("openai", "gpt-4"),
		]);
		expect(resolved?.provider).toBe("anthropic");
		expect(resolved?.id).toBe("claude-fable-5");
	});

	test("reports 200, TTL cache, forced 304, and preserves the current selection (#17)", async () => {
		isolatedCache();
		const selected = model("openai", "gpt-4");
		let requests = 0;
		const fetchMock = vi.fn(async (_input: string | URL | Request, init?: RequestInit) => {
			if (++requests === 1) return defaultResponse("anthropic/claude-fable-5", '"v1"');
			expect(new Headers(init?.headers).get("If-None-Match")).toBe('"v1"');
			return new Response(null, { status: 304 });
		});
		vi.stubGlobal("fetch", fetchMock);
		const { getPreferredDefaultModelId, refreshDefaultModelCatalogWithOutcome, refreshDefaultModelCatalog } =
			await import("../src/core/default-model-catalog.js");
		await expect(refreshDefaultModelCatalogWithOutcome()).resolves.toEqual({
			kind: "updated",
			models: "anthropic/claude-fable-5",
		});
		await expect(refreshDefaultModelCatalogWithOutcome()).resolves.toEqual({
			kind: "cached",
			models: "anthropic/claude-fable-5",
		});
		expect(fetchMock).toHaveBeenCalledTimes(1);
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toEqual({
			kind: "not-modified",
			models: "anthropic/claude-fable-5",
		});
		expect(fetchMock).toHaveBeenCalledTimes(2);
		expect(getPreferredDefaultModelId()).toBe("anthropic/claude-fable-5");
		expect(selected).toMatchObject({ provider: "openai", id: "gpt-4" });
		await expect(refreshDefaultModelCatalog()).resolves.toBe("anthropic/claude-fable-5");
	});

	test("reports last-good fallback on network failure without leaking response details (#17)", async () => {
		isolatedCache();
		const fetchMock = vi
			.fn()
			.mockResolvedValueOnce(defaultResponse("openai/gpt-4"))
			.mockRejectedValueOnce(new Error("secret-token"))
			.mockResolvedValueOnce(new Response("private body secret-token", { status: 503 }));
		vi.stubGlobal("fetch", fetchMock);
		const { getPreferredDefaultModelId, refreshDefaultModelCatalogWithOutcome } = await import(
			"../src/core/default-model-catalog.js"
		);
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toMatchObject({ kind: "updated" });
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toEqual({
			kind: "error",
			models: "openai/gpt-4",
			error: "Catalog refresh failed",
		});
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toEqual({
			kind: "error",
			models: "openai/gpt-4",
			error: "Catalog request failed with status 503",
		});
		expect(getPreferredDefaultModelId()).toBe("openai/gpt-4");
	});

	test("reports offline with and without a last-good selector, even when forced (#17)", async () => {
		isolatedCache();
		const fetchMock = vi.fn(async () => defaultResponse("openai/gpt-4"));
		vi.stubGlobal("fetch", fetchMock);
		const { refreshDefaultModelCatalogWithOutcome } = await import("../src/core/default-model-catalog.js");
		vi.stubEnv("PI_OFFLINE", "1");
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toEqual({
			kind: "offline",
			models: undefined,
		});
		vi.stubEnv("PI_OFFLINE", "0");
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toMatchObject({ kind: "updated" });
		vi.stubEnv("PI_OFFLINE", "1");
		await expect(refreshDefaultModelCatalogWithOutcome(true)).resolves.toEqual({
			kind: "offline",
			models: "openai/gpt-4",
		});
		expect(fetchMock).toHaveBeenCalledTimes(1);
	});

	test("reloads an external default and prevents an older in-flight refresh from overwriting it (#17)", async () => {
		isolatedCache();
		const initialFetch = vi.fn(async () => defaultResponse("openai/old"));
		vi.stubGlobal("fetch", initialFetch);
		const local = await import("../src/core/default-model-catalog.js");
		await expect(local.refreshDefaultModelCatalogWithOutcome(true)).resolves.toMatchObject({
			kind: "updated",
			models: "openai/old",
		});
		expect(local.getDefaultModelCatalogStatus()).toMatchObject({ state: "fresh", source: "network" });

		let completeLateFetch!: (value: Response) => void;
		const lateResponse = new Promise<Response>((resolve) => {
			completeLateFetch = resolve;
		});
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => lateResponse),
		);
		const pending = local.refreshDefaultModelCatalogWithOutcome(true);
		expect(local.getDefaultModelCatalogStatus().state).toBe("refreshing");

		vi.resetModules(); // A separate process writes the same on-disk cache.
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => defaultResponse("anthropic/new")),
		);
		const external = await import("../src/core/default-model-catalog.js");
		await expect(external.refreshDefaultModelCatalogWithOutcome(true)).resolves.toMatchObject({
			kind: "updated",
			models: "anthropic/new",
		});
		expect(local.getPreferredDefaultModelId()).toBe("openai/old");
		expect(local.reloadDefaultModelCatalogFromDisk()).toBe("anthropic/new");
		expect(local.getDefaultModelCatalogStatus()).toMatchObject({ state: "fresh", source: "disk" });
		completeLateFetch(defaultResponse("openai/late"));
		await expect(pending).resolves.toEqual({ kind: "cancelled", models: undefined });
		expect(local.getPreferredDefaultModelId()).toBe("anthropic/new");
		expect(external.reloadDefaultModelCatalogFromDisk()).toBe("anthropic/new");
	});

	test("falls back to undefined offline; resolver ignores unknown selectors", async () => {
		const { resolvePreferredDefaultModel } = await import("../src/core/default-model-catalog.js");
		expect(resolvePreferredDefaultModel(undefined, [])).toBeUndefined();
		expect(resolvePreferredDefaultModel("anthropic/gpt-4", [model("openai", "gpt-4")])).toBeUndefined();
	});
});
