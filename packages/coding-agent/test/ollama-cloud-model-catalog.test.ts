import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { CatalogCache } from "../src/core/model-catalog-cache.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import {
	mergeOllamaCloudCatalog,
	ollamaCloudFetch,
	parseOllamaCloudCatalog,
	pinnedOllamaCloudModels,
} from "../src/core/ollama-cloud-model-catalog.js";

const servers: Server[] = [];
const dirs: string[] = [];
afterEach(async () => {
	vi.unstubAllEnvs();
	vi.unstubAllGlobals();
	await Promise.all(servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))));
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});
async function endpoint(
	handler: (path: string, auth: string, id: string) => { status?: number; body: unknown },
): Promise<string> {
	const server = createServer(async (req, res) => {
		const data: Uint8Array[] = [];
		for await (const chunk of req) data.push(chunk);
		const id = req.url === "/api/show" ? (JSON.parse(Buffer.concat(data).toString())?.model as string) : "";
		const result = handler(req.url ?? "", req.headers.authorization ?? "", id);
		res.writeHead(result.status ?? 200, { "content-type": "application/json" });
		res.end(JSON.stringify(result.body));
	});
	servers.push(server);
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	const address = server.address();
	if (!address || typeof address === "string") throw new Error("Missing local endpoint address");
	return `http://127.0.0.1:${address.port}`;
}
const show = (capabilities: string[] = ["tools"], context = 262144) => ({
	capabilities,
	model_info: { "test.context_length": context },
	baseUrl: "https://evil.invalid",
	apiKey: "stolen",
});
function cache(base: string) {
	const dir = mkdtempSync(join(tmpdir(), "ollama-catalog-test-"));
	dirs.push(dir);
	return new CatalogCache(`${base}/v1/models`, join(dir, "catalog.json"), (payload) =>
		parseOllamaCloudCatalog(payload, base),
	);
}
function refresh(catalog: ReturnType<typeof cache>, base: string, key: string, force = true) {
	return catalog.refresh(key, {
		force,
		headers: { Authorization: `Bearer ${key}` },
		fetchFn: ollamaCloudFetch(base),
		mergePayload: mergeOllamaCloudCatalog,
	});
}

describe("Ollama Cloud provider-owned discovery", () => {
	it("pins a first-use tool-capable catalog with compatible thinking and transport", () => {
		const models = pinnedOllamaCloudModels("https://ollama.com");
		expect(models).toHaveLength(20);
		expect(models.find((model) => model.id === "gpt-oss:20b")?.thinkingLevelMap?.off).toBeNull();
		expect(
			models.every((model) => model.provider === "ollama-cloud" && model.baseUrl === "https://ollama.com/v1"),
		).toBe(true);
	});

	it("uses bearer auth on both endpoints, extracts capabilities and ignores untrusted transport", async () => {
		const requests: string[] = [];
		const base = await endpoint((path, auth, id) => {
			requests.push(`${path}:${auth}:${id}`);
			if (path === "/v1/models") return { body: { data: [{ id: "qwen3-vl:235b" }, { id: "without-tools" }] } };
			return { body: id === "without-tools" ? show(["vision"]) : show(["tools", "vision", "thinking"]) };
		});
		const models = await refresh(cache(base), base, "test-secret");
		expect(models).toHaveLength(1);
		expect(models?.[0]).toMatchObject({
			id: "qwen3-vl:235b",
			contextWindow: 262144,
			input: ["text", "image"],
			reasoning: true,
			baseUrl: `${base}/v1`,
		});
		expect(models?.[0].thinkingLevelMap?.off).toBeNull();
		expect(requests).toEqual(
			expect.arrayContaining(["/v1/models:Bearer test-secret:", "/api/show:Bearer test-secret:qwen3-vl:235b"]),
		);
		expect(JSON.stringify(models)).not.toContain("evil.invalid");
		expect(JSON.stringify(models)).not.toContain("stolen");
	});

	it("#6 scopes context and token caps by session cwd without mutating the registry", () => {
		const dir = mkdtempSync(join(tmpdir(), "ollama-registry-caps-"));
		dirs.push(dir);
		vi.stubEnv("PRIME_AGENT_CODING_AGENT_DIR", dir);
		const first = mkdtempSync(join(tmpdir(), "ollama-project-first-"));
		const second = mkdtempSync(join(tmpdir(), "ollama-project-second-"));
		dirs.push(first, second);
		for (const project of [first, second]) mkdirSync(join(project, ".prime", "agent"), { recursive: true });
		writeFileSync(
			join(dir, "ollama-cloud.json"),
			JSON.stringify({
				modelOverrides: { "gpt-oss:20b": { contextWindow: 120000, maxTokens: 4000 } },
			}),
		);
		writeFileSync(
			join(first, ".prime", "agent", "ollama-cloud.json"),
			JSON.stringify({
				modelOverrides: { "gpt-oss:20b": { maxTokens: 5000 } },
			}),
		);
		writeFileSync(
			join(second, ".prime", "agent", "ollama-cloud.json"),
			JSON.stringify({
				modelOverrides: { "gpt-oss:20b": { contextWindow: 85000 } },
			}),
		);
		const registry = ModelRegistry.create(AuthStorage.create(join(dir, "auth.json")), join(dir, "models.json"));
		const shared = registry.find("ollama-cloud", "gpt-oss:20b");
		expect(shared).toBeDefined();
		if (!shared) throw new Error("Pinned model missing");
		const one = registry.configureOllamaModel(shared, first);
		const two = registry.configureOllamaModel(shared, second);
		expect(one).toMatchObject({ contextWindow: 120000, maxTokens: 5000 });
		expect(two).toMatchObject({ contextWindow: 85000, maxTokens: 4000 });
		expect(registry.find("ollama-cloud", "gpt-oss:20b")).toBe(shared);
		expect(registry.configureOllamaModel(one, second)).toMatchObject({ contextWindow: 85000, maxTokens: 4000 });
		writeFileSync(join(first, ".prime", "agent", "ollama-cloud.json"), "{}");
		expect(registry.configureOllamaModel(one, first)).toMatchObject({ contextWindow: 120000, maxTokens: 4000 });
		writeFileSync(join(dir, "ollama-cloud.json"), "{}");
		expect(registry.configureOllamaModel(one, first)).toBe(shared);
		expect(shared).toMatchObject({ contextWindow: 131072, maxTokens: 32768 });
	});

	it("#6 integrates credential-scoped cache, fallback and status into ModelRegistry", async () => {
		const seen: string[] = [];
		const base = await endpoint((path, auth, _id) => {
			seen.push(`${path}:${auth}`);
			if (path === "/v1/models") return { body: { data: [{ id: "qwen3-vl:test" }] } };
			return { body: show(["tools", "vision", "thinking"], 456000) };
		});
		vi.stubEnv("OLLAMA_API_BASE", base);
		const nativeFetch = globalThis.fetch;
		vi.stubGlobal("fetch", ((input: Parameters<typeof fetch>[0], init?: RequestInit) => {
			const url = String(input);
			return url.startsWith(base)
				? nativeFetch(input, init)
				: Promise.reject(new Error("External network forbidden"));
		}) as typeof fetch);
		const dir = mkdtempSync(join(tmpdir(), "ollama-registry-test-"));
		dirs.push(dir);
		vi.stubEnv("PRIME_AGENT_CODING_AGENT_DIR", dir);
		const auth = AuthStorage.create(join(dir, "auth.json"));
		const registry = ModelRegistry.create(auth, join(dir, "models.json"));
		expect(registry.getAll().some((model) => model.provider === "ollama-cloud")).toBe(true);
		auth.set("ollama-cloud", { type: "api_key", key: "one" });
		await registry.refreshModelCatalog();
		expect(registry.find("ollama-cloud", "qwen3-vl:test")?.contextWindow).toBe(456000);
		expect(registry.getProviderCatalogStatus("ollama-cloud").state).toBe("fresh");
		expect(seen).toContain("/api/show:Bearer one");
		auth.set("ollama-cloud", { type: "api_key", key: "two" });
		await registry.refreshModelCatalog();
		expect(seen).toContain("/api/show:Bearer two");
		expect(registry.getProviderCatalogStatus("ollama-cloud").source).toBe("network");
	});

	it("#6 uses same-origin custom Authorization only, never leaking mismatched proxy credentials", async () => {
		const seen: string[] = [];
		const base = await endpoint((path, auth) => {
			seen.push(`${path}:${auth}`);
			return path === "/v1/models" ? { body: { data: [{ id: "cloud-model" }] } } : { body: show() };
		});
		vi.stubEnv("OLLAMA_API_BASE", base);
		const nativeFetch = globalThis.fetch;
		vi.stubGlobal("fetch", ((input: Parameters<typeof fetch>[0], init?: RequestInit) =>
			String(input).startsWith(base)
				? nativeFetch(input, init)
				: Promise.reject(new Error("External network forbidden"))) as typeof fetch);
		const dir = mkdtempSync(join(tmpdir(), "ollama-auth-origin-"));
		dirs.push(dir);
		vi.stubEnv("PRIME_AGENT_CODING_AGENT_DIR", dir);
		const modelConfig = join(dir, "models.json");
		writeFileSync(
			modelConfig,
			JSON.stringify({
				providers: {
					"ollama-cloud": {
						baseUrl: `${base}/v1`,
						headers: { Authorization: "Custom same-origin" },
					},
				},
			}),
		);
		const auth = AuthStorage.create(join(dir, "auth.json"));
		const registry = ModelRegistry.create(auth, modelConfig);
		await registry.refreshModelCatalog();
		expect(seen).toContain("/v1/models:Custom same-origin");
		expect(seen).toContain("/api/show:Custom same-origin");
		seen.length = 0;
		writeFileSync(
			modelConfig,
			JSON.stringify({
				providers: {
					"ollama-cloud": {
						baseUrl: "https://proxy.invalid/v1",
						headers: { Authorization: "Custom proxy-only" },
					},
				},
			}),
		);
		auth.set("ollama-cloud", { type: "api_key", key: "api-key" });
		registry.refresh();
		await registry.refreshModelCatalog();
		expect(seen).toEqual([]);
		expect(registry.find("ollama-cloud", "gpt-oss:20b")).toBeDefined();
		expect(registry.getProviderCatalogStatus("ollama-cloud").state).toBe("idle");
	});

	it("#6 bounds each streamed list and detail response before JSON parsing", async () => {
		for (const oversize of ["list", "show"]) {
			let cancelled = false;
			let pulls = 0;
			const huge = () =>
				new Response(
					new ReadableStream<Uint8Array>({
						pull(controller) {
							pulls++;
							controller.enqueue(new Uint8Array(64 * 1024));
						},
						cancel() {
							cancelled = true;
						},
					}),
				);
			const fetchFn = (async (url: Parameters<typeof fetch>[0]) =>
				String(url).endsWith("/v1/models")
					? oversize === "list"
						? huge()
						: new Response(JSON.stringify({ data: [{ id: "model" }] }))
					: huge()) as typeof fetch;
			const fetchCatalog = ollamaCloudFetch("http://127.0.0.1:1", fetchFn);
			await expect(fetchCatalog("http://127.0.0.1:1/v1/models")).rejects.toThrow(
				oversize === "list" ? "too large" : "details unavailable",
			);
			expect(cancelled).toBe(true);
			expect(pulls).toBeLessThan(50);
		}
	});

	it("#6 invalidates rejected credential cache without keeping private rows", async () => {
		let allowed = true;
		const base = await endpoint((path) => {
			if (!allowed) return { status: 401, body: { error: "revoked" } };
			return path === "/v1/models" ? { body: { data: [{ id: "private" }] } } : { body: show() };
		});
		const catalog = cache(base);
		expect(await refresh(catalog, base, "revoked-key")).toHaveLength(1);
		allowed = false;
		expect(await refresh(catalog, base, "revoked-key")).toBeUndefined();
		expect(catalog.status("revoked-key")).toMatchObject({ state: "error", source: "none" });
	});

	it("#6 fails closed when denied and transient detail failures have no successes", async () => {
		let reject = false;
		const base = await endpoint((path, _auth, id) => {
			if (path === "/v1/models") return { body: { data: [{ id: "revoked" }, { id: "flaky" }] } };
			if (reject) return id === "revoked" ? { status: 403, body: {} } : { status: 503, body: {} };
			return { body: show() };
		});
		const dir = mkdtempSync(join(tmpdir(), "ollama-denied-fail-closed-"));
		dirs.push(dir);
		const file = join(dir, "catalog.json");
		const create = () =>
			new CatalogCache(`${base}/v1/models`, file, (payload) => parseOllamaCloudCatalog(payload, base));
		const catalog = create();
		expect(await refresh(catalog, base, "account")).toHaveLength(2);
		reject = true;
		expect(await refresh(catalog, base, "account")).toBeUndefined();
		expect(catalog.status("account")).toMatchObject({ state: "error", source: "none" });
		vi.stubEnv("PI_OFFLINE", "1");
		expect(create().get("account")).toBeUndefined();
	});

	it("#6 removes unlisted and denied IDs but retains transient listed failures across disk restart", async () => {
		let phase: "initial" | "partial" | "removed" = "initial";
		const base = await endpoint((path, _auth, id) => {
			const ids = phase === "removed" ? ["good", "transient"] : ["good", "transient", "denied"];
			if (path === "/v1/models") return { body: { data: ids.map((item) => ({ id: item })) } };
			if (phase !== "initial" && id === "denied") return { status: 403, body: {} };
			if (phase !== "initial" && id === "transient") return { status: 503, body: {} };
			return { body: show() };
		});
		const dir = mkdtempSync(join(tmpdir(), "ollama-persist-"));
		dirs.push(dir);
		const file = join(dir, "catalog.json");
		const create = () =>
			new CatalogCache(`${base}/v1/models`, file, (payload) => parseOllamaCloudCatalog(payload, base));
		const first = create();
		expect((await refresh(first, base, "account"))?.map((item) => item.id)).toEqual(["good", "transient", "denied"]);
		phase = "partial";
		expect((await refresh(first, base, "account"))?.map((item) => item.id)).toEqual(["good", "transient"]);
		const reopened = create();
		expect(reopened.get("account")?.map((item) => item.id)).toEqual(["good", "transient"]);
		phase = "removed";
		expect((await refresh(reopened, base, "account"))?.map((item) => item.id)).toEqual(["good", "transient"]);
		// Now the listing drops a previously successful model; its cache row must disappear.
		phase = "removed";
		const listOnlyGood = await endpoint((path) =>
			path === "/v1/models" ? { body: { data: [{ id: "good" }] } } : { body: show() },
		);
		const fresh = create();
		const fetched = await fresh.refresh("account", {
			force: true,
			fetchFn: ollamaCloudFetch(listOnlyGood),
			mergePayload: mergeOllamaCloudCatalog,
		});
		expect(fetched?.map((item) => item.id)).toEqual(["good"]);
		expect(
			create()
				.get("account")
				?.map((item) => item.id),
		).toEqual(["good"]);
	});

	it("keeps last-good model details on partial or offline failure, with per-scope status and rotation", async () => {
		let broken = false;
		let key = "first";
		const base = await endpoint((path, auth, id) => {
			if (auth !== `Bearer ${key}`) return { status: 401, body: { error: "unauthorized" } };
			if (path === "/v1/models") return { body: { data: [{ id: "good" }, { id: "fragile" }] } };
			return id === "fragile" && broken ? { status: 503, body: {} } : { body: show() };
		});
		const catalog = cache(base);
		expect(await refresh(catalog, base, key)).toHaveLength(2);
		broken = true;
		expect(await refresh(catalog, base, key)).toHaveLength(2);
		expect(catalog.status(key).state).toBe("fresh");
		vi.stubEnv("PI_OFFLINE", "1");
		expect(await refresh(catalog, base, key)).toHaveLength(2);
		vi.unstubAllEnvs();
		key = "second";
		expect(catalog.get(key)).toBeUndefined();
		expect(catalog.status(key).state).toBe("idle");
		expect(await refresh(catalog, base, key)).toHaveLength(1);
		expect(catalog.status(key).source).toBe("network");
	});
});
