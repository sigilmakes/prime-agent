import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Api, Model, OpenAICompletionsCompat } from "@earendil-works/pi-ai";
import { getApiProvider, getModels } from "@earendil-works/pi-ai";
import { getOAuthProvider, registerOAuthProvider } from "@earendil-works/pi-ai/oauth";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { getBundledModels } from "../src/core/bundled-model-catalog.js";
import { ModelRegistry, type ProviderConfigInput } from "../src/core/model-registry.js";

describe("ModelRegistry", () => {
	let tempDir: string;
	let modelsJsonPath: string;
	let authStorage: AuthStorage;

	beforeEach(() => {
		tempDir = join(tmpdir(), `pi-test-model-registry-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
		modelsJsonPath = join(tempDir, "models.json");
		vi.stubEnv("PRIME_AGENT_CODING_AGENT_DIR", tempDir);
		authStorage = AuthStorage.create(join(tempDir, "auth.json"));
	});

	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
		vi.unstubAllEnvs();
		if (tempDir && existsSync(tempDir)) {
			rmSync(tempDir, { recursive: true });
		}
	});

	function providerConfig(
		baseUrl: string,
		models: Array<{ id: string; name?: string }>,
		api: string = "anthropic-messages",
	): ProviderConfigInput {
		return {
			baseUrl,
			apiKey: "TEST_KEY",
			api: api as Api,
			models: models.map((m) => ({
				id: m.id,
				name: m.name ?? m.id,
				reasoning: false,
				input: ["text"],
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
				contextWindow: 100000,
				maxTokens: 8000,
			})),
		};
	}

	function writeModelsJson(providers: Record<string, ReturnType<typeof providerConfig>>) {
		writeFileSync(modelsJsonPath, JSON.stringify({ providers }));
	}

	function getModelsForProvider(registry: ModelRegistry, provider: string) {
		return registry.getAll().filter((m) => m.provider === provider);
	}

	function toShPath(value: string): string {
		// Single pass: backslashes become separators and quotes are escaped, so no
		// escape sequence can be produced and then re-escaped by a later pass.
		return value.replace(/[\\"]/g, (ch) => (ch === "\\" ? "/" : '\\"'));
	}

	function overrideConfig(baseUrl: string, headers?: Record<string, string>) {
		return { baseUrl, ...(headers && { headers }) };
	}

	function writeRawModelsJson(providers: Record<string, unknown>) {
		writeFileSync(modelsJsonPath, JSON.stringify({ providers }));
	}

	describe("baseUrl override (no custom models)", () => {
		test("overriding baseUrl keeps every built-in model and rewrites only that provider", () => {
			writeRawModelsJson({
				anthropic: overrideConfig("https://my-proxy.example.com/v1"),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const anthropicModels = getModelsForProvider(registry, "anthropic");

			expect(anthropicModels.length).toBeGreaterThan(1);
			expect(anthropicModels.some((m) => m.id.includes("claude"))).toBe(true);
			expect(anthropicModels.every((m) => m.baseUrl === "https://my-proxy.example.com/v1")).toBe(true);
			expect(getModelsForProvider(registry, "google")[0].baseUrl).not.toBe("https://my-proxy.example.com/v1");
		});

		test.each<[string, Record<string, unknown>]>([
			[
				"a baseUrl plus headers override",
				overrideConfig("https://my-proxy.example.com/v1", { "X-Custom-Header": "custom-value" }),
			],
			["a headers-only override", { headers: { "X-Custom-Header": "custom-value" } }],
		])("%s resolves headers at request time", async (_name, config) => {
			writeRawModelsJson({ anthropic: config });

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			expect(registry.getError()).toBeUndefined();

			for (const model of getModelsForProvider(registry, "anthropic")) {
				await expect(registry.getApiKeyAndHeaders(model)).resolves.toMatchObject({
					ok: true,
					headers: { "X-Custom-Header": "custom-value" },
				});
			}
		});

		test("prime inference requests include selected Prime Agent team header", async () => {
			const primeAuthStorage = AuthStorage.inMemory({
				"prime-inference": {
					type: "api_key",
					key: "agent-key",
					primeTeam: { teamId: "team-1", name: "Research" },
				},
			});
			const registry = ModelRegistry.create(primeAuthStorage, modelsJsonPath);
			const model = getModelsForProvider(registry, "prime-inference")[0];
			expect(model).toBeDefined();

			const auth = await registry.getApiKeyAndHeaders(model!);

			expect(auth).toEqual({
				ok: true,
				apiKey: "agent-key",
				headers: { "X-Prime-Team-ID": "team-1" },
			});
		});

		test("refresh() picks up baseUrl override changes", () => {
			writeRawModelsJson({
				anthropic: overrideConfig("https://first-proxy.example.com/v1"),
			});
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(getModelsForProvider(registry, "anthropic")[0].baseUrl).toBe("https://first-proxy.example.com/v1");

			writeRawModelsJson({
				anthropic: overrideConfig("https://second-proxy.example.com/v1"),
			});
			registry.refresh();

			expect(getModelsForProvider(registry, "anthropic")[0].baseUrl).toBe("https://second-proxy.example.com/v1");
		});
	});

	describe("custom models merge behavior", () => {
		function demoProvider(providerFields: Record<string, unknown>, modelFields: Record<string, unknown> = {}) {
			return {
				baseUrl: "https://example.com/v1",
				apiKey: "DEMO_KEY",
				api: "openai-completions",
				...providerFields,
				models: [
					{
						id: "demo-model",
						reasoning: true,
						input: ["text"],
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
						contextWindow: 1000,
						maxTokens: 100,
						...modelFields,
					},
				],
			};
		}

		test("built-in provider models inherit api and baseUrl while unknown providers must declare them", () => {
			writeRawModelsJson({
				openrouter: {
					models: [{ id: "fake-provider/fake-model", name: "Fake model", reasoning: true, input: ["text"] }],
				},
			});
			const inherited = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(inherited.getError()).toBeUndefined();
			expect(inherited.find("openrouter", "fake-provider/fake-model")).toMatchObject({
				api: "openai-completions",
				baseUrl: "https://openrouter.ai/api/v1",
			});

			writeRawModelsJson({
				"my-custom-provider": {
					models: [{ id: "my-model", api: "openai-completions", reasoning: false, input: ["text"] }],
				},
			});

			expect(ModelRegistry.create(authStorage, modelsJsonPath).getError()).toContain("baseUrl");
		});

		test("custom models merge into a built-in provider, replacing built-ins by id", () => {
			writeModelsJson({
				anthropic: providerConfig("https://merged-proxy.example.com/v1", [{ id: "claude-custom" }]),
				openrouter: providerConfig(
					"https://my-proxy.example.com/v1",
					[{ id: "anthropic/claude-sonnet-4" }],
					"openai-completions",
				),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const anthropicModels = getModelsForProvider(registry, "anthropic");

			expect(anthropicModels.length).toBeGreaterThan(1);
			expect(anthropicModels.some((m) => m.id === "claude-custom")).toBe(true);
			expect(anthropicModels.some((m) => m.id.includes("claude-"))).toBe(true);
			// Provider-level baseUrl reaches built-in and custom models alike.
			expect(anthropicModels.every((m) => m.baseUrl === "https://merged-proxy.example.com/v1")).toBe(true);
			// A custom model with a built-in id replaces it instead of duplicating it.
			expect(
				getModelsForProvider(registry, "openrouter").filter((m) => m.id === "anthropic/claude-sonnet-4"),
			).toHaveLength(1);
			// Unrelated providers keep their built-in catalogs.
			expect(getModelsForProvider(registry, "google").length).toBeGreaterThan(0);
		});

		const streamingCompat = { supportsUsageInStreaming: false, maxTokensField: "max_tokens" };
		const completionCompat = { supportsUsageInStreaming: true, maxTokensField: "max_completion_tokens" };
		const anthropicCompat = { supportsEagerToolInputStreaming: false, supportsLongCacheRetention: false };
		const thinkingModel = {
			thinkingLevelMap: { minimal: null, high: "max" },
			compat: { supportsStrictMode: false, cacheControlFormat: "anthropic" },
		};

		test.each<[string, Record<string, unknown>, Record<string, unknown>, Record<string, unknown>]>([
			[
				"provider-level compat applies to custom models",
				{ compat: streamingCompat },
				{},
				{ compat: streamingCompat },
			],
			[
				"model-level compat overrides provider-level compat",
				{ compat: streamingCompat },
				{ compat: completionCompat },
				{ compat: completionCompat },
			],
			[
				"the schema accepts thinkingLevelMap plus strict-mode and cache-control compat",
				{},
				thinkingModel,
				thinkingModel,
			],
			[
				"the schema accepts the Anthropic streaming and cache-retention flags",
				{ api: "anthropic-messages", compat: anthropicCompat },
				{},
				{ compat: anthropicCompat },
			],
		])("%s", (_name, providerFields, modelFields, expected) => {
			writeRawModelsJson({ demo: demoProvider(providerFields, modelFields) });

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.getError()).toBeUndefined();
			expect(registry.find("demo", "demo-model")).toMatchObject(expected);
		});

		test("provider-level compat applies to built-in models", () => {
			writeRawModelsJson({
				openrouter: { compat: { supportsUsageInStreaming: false, supportsStrictMode: false } },
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const models = getModelsForProvider(registry, "openrouter");

			expect(models.length).toBeGreaterThan(0);
			for (const model of models) {
				const compat = model.compat as OpenAICompletionsCompat | undefined;
				expect(compat?.supportsUsageInStreaming).toBe(false);
				expect(compat?.supportsStrictMode).toBe(false);
			}
		});

		test("model-level baseUrl overrides provider-level baseUrl for custom models", () => {
			writeRawModelsJson({
				"opencode-go": {
					baseUrl: "https://opencode.ai/zen/go/v1",
					apiKey: "TEST_KEY",
					models: [
						{
							id: "minimax-m2.5",
							api: "anthropic-messages",
							baseUrl: "https://opencode.ai/zen/go",
							reasoning: true,
							input: ["text"],
							cost: { input: 0.3, output: 1.2, cacheRead: 0.03, cacheWrite: 0 },
							contextWindow: 204800,
							maxTokens: 131072,
						},
						{
							id: "glm-5",
							api: "openai-completions",
							reasoning: true,
							input: ["text"],
							cost: { input: 1, output: 3.2, cacheRead: 0.2, cacheWrite: 0 },
							contextWindow: 204800,
							maxTokens: 131072,
						},
					],
				},
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.find("opencode-go", "minimax-m2.5")?.baseUrl).toBe("https://opencode.ai/zen/go");
			expect(registry.find("opencode-go", "glm-5")?.baseUrl).toBe("https://opencode.ai/zen/go/v1");
		});

		test("refresh() reloads merged custom models and restores built-ins when they are removed", () => {
			writeModelsJson({
				anthropic: providerConfig("https://first-proxy.example.com/v1", [{ id: "claude-custom" }]),
			});
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			expect(getModelsForProvider(registry, "anthropic").some((m) => m.id === "claude-custom")).toBe(true);

			writeModelsJson({});
			registry.refresh();

			const anthropicModels = getModelsForProvider(registry, "anthropic");
			expect(anthropicModels.some((m) => m.id === "claude-custom")).toBe(false);
			expect(anthropicModels.some((m) => m.id.includes("claude"))).toBe(true);
		});
	});

	describe("live Prime Inference models", () => {
		test("loads the cache without replacing external providers and applies local overrides", () => {
			const bundled = getModels("prime-inference") as Model<"openai-completions">[];
			const catalogEntries = bundled.map((model) => ({
				id: model.id,
				display_name: `Live ${model.name}`,
				pricing: { input_usd_per_mtok: model.cost.input, output_usd_per_mtok: model.cost.output },
				specs: {
					context_window: model.contextWindow,
					max_output_tokens: model.maxTokens,
					modalities: { input: model.input, output: ["text"] },
					supports_reasoning: model.reasoning,
				},
			}));
			catalogEntries.push({
				id: "test/live-added",
				display_name: "Live Added",
				pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
				specs: {
					context_window: 200_000,
					max_output_tokens: 20_000,
					modalities: { input: ["text"], output: ["text"] },
					supports_reasoning: false,
				},
			});
			mkdirSync(join(tempDir, "models"), { recursive: true });
			writeFileSync(
				join(tempDir, "models", "prime-inference-models-cache.json"),
				JSON.stringify({ object: "list", data: catalogEntries }),
			);
			writeRawModelsJson({
				"prime-inference": {
					baseUrl: "https://local-proxy.example.com/v1",
					modelOverrides: { "test/live-added": { name: "Local Added", contextWindow: 123_456 } },
				},
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			expect(registry.find("prime-inference", "test/live-added")).toMatchObject({
				name: "Local Added",
				baseUrl: "https://local-proxy.example.com/v1",
				contextWindow: 123_456,
				cost: { input: 1, output: 2 },
			});
			expect(getModelsForProvider(registry, "openrouter")).toHaveLength(
				getBundledModels().filter((model) => model.provider === "openrouter").length,
			);
		});

		test("restores cached private metadata only for matching credentials and team", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			const privateRoute = {
				id: "vendor/model:deployment",
				display_name: "Private Deployment",
				pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
				specs: {
					context_window: 200_000,
					max_output_tokens: 20_000,
					modalities: { input: ["text"], output: ["text"] },
					supports_reasoning: false,
				},
			};
			const credential = {
				type: "api_key" as const,
				key: "prime-key",
				primeTeam: { teamId: "research-team", name: "Research" },
			};
			authStorage.set("prime-inference", credential);
			vi.stubGlobal(
				"fetch",
				vi.fn(
					async (_url: string | URL | Request, init?: RequestInit) =>
						new Response(
							JSON.stringify({ data: new Headers(init?.headers).has("Authorization") ? [privateRoute] : [] }),
						),
				),
			);
			const firstRegistry = ModelRegistry.create(authStorage, modelsJsonPath);
			expect(
				(await firstRegistry.refreshAvailableModels()).find((model) => model.id === privateRoute.id),
			).toMatchObject({ name: "Private Deployment", contextWindow: 200_000 });

			vi.stubGlobal(
				"fetch",
				vi.fn(async () => {
					throw new Error("offline");
				}),
			);
			vi.stubEnv("PI_OFFLINE", "1");
			const restoredRegistry = ModelRegistry.create(AuthStorage.create(join(tempDir, "auth.json")), modelsJsonPath);
			expect(
				(await restoredRegistry.refreshAvailableModels()).find((model) => model.id === privateRoute.id),
			).toMatchObject({ name: "Private Deployment", contextWindow: 200_000 });

			for (const changed of [
				{ ...credential, key: "different-prime-key" },
				{ ...credential, primeTeam: { teamId: "other-team", name: "Other" } },
			]) {
				authStorage.set("prime-inference", changed);
				const changedRegistry = ModelRegistry.create(authStorage, modelsJsonPath);
				expect((await changedRegistry.refreshAvailableModels()).some((model) => model.id === privateRoute.id)).toBe(
					false,
				);
			}

			authStorage.set("prime-inference", credential);
			const cachePath = join(tempDir, "prime-inference-private-models.json");
			const cache = JSON.parse(readFileSync(cachePath, "utf8"));
			// Pre-HMAC SHA256("prime-key\0research-team") cache entries must miss safely on upgrade.
			cache.fingerprint = "9ffd3740e055c8cc8923a1d2653c6d02a4a9c95e6ab151bc179aeaa94dd046b4";
			writeFileSync(cachePath, JSON.stringify(cache));
			const legacyRegistry = ModelRegistry.create(authStorage, modelsJsonPath);
			expect((await legacyRegistry.refreshAvailableModels()).some((model) => model.id === privateRoute.id)).toBe(
				false,
			);
		});
	});

	describe("modelOverrides (per-model customization)", () => {
		const sonnetId = "anthropic/claude-sonnet-4";
		const opusId = "anthropic/claude-opus-4.5";

		function withOverrides(modelOverrides: Record<string, unknown>, providerFields: Record<string, unknown> = {}) {
			writeRawModelsJson({ openrouter: { ...providerFields, modelOverrides } });
			return ModelRegistry.create(authStorage, modelsJsonPath);
		}

		test.each<[string, Record<string, unknown>, Record<string, unknown>]>([
			["renames one built-in model", { name: "Custom Sonnet Name" }, { name: "Custom Sonnet Name" }],
			[
				"deep merges compat routing",
				{ compat: { openRouterRouting: { only: ["amazon-bedrock"] } } },
				{ compat: { openRouterRouting: { only: ["amazon-bedrock"] } } },
			],
			["changes cost fields partially", { cost: { input: 99 } }, { cost: { input: 99 } }],
		])("%s and leaves sibling models alone", (_name, override, expected) => {
			const registry = withOverrides({ [sonnetId]: override });

			expect(registry.find("openrouter", sonnetId)).toMatchObject(expected);
			expect(registry.find("openrouter", opusId)).not.toMatchObject(expected);
		});

		test("overrides combine with a provider baseUrl, add request headers, and ignore unknown ids", async () => {
			const registry = withOverrides(
				{
					[sonnetId]: { name: "Proxied Sonnet", headers: { "X-Custom-Model-Header": "value" } },
					"nonexistent/model-id": { name: "This should not appear" },
				},
				{ baseUrl: "https://my-proxy.example.com/v1" },
			);
			const sonnet = registry.find("openrouter", sonnetId);

			expect(registry.getError()).toBeUndefined();
			expect(sonnet).toMatchObject({ name: "Proxied Sonnet", baseUrl: "https://my-proxy.example.com/v1" });
			expect(registry.find("openrouter", opusId)).toMatchObject({ baseUrl: "https://my-proxy.example.com/v1" });
			expect(registry.find("openrouter", "nonexistent/model-id")).toBeUndefined();
			await expect(registry.getApiKeyAndHeaders(sonnet!)).resolves.toMatchObject({
				ok: true,
				headers: { "X-Custom-Model-Header": "value" },
			});
		});

		test("refresh() picks up changed overrides and restores built-in values once removed", () => {
			const registry = withOverrides({ [sonnetId]: { name: "First Name" } });
			expect(registry.find("openrouter", sonnetId)?.name).toBe("First Name");

			writeRawModelsJson({ openrouter: { modelOverrides: { [sonnetId]: { name: "Second Name" } } } });
			registry.refresh();
			expect(registry.find("openrouter", sonnetId)?.name).toBe("Second Name");

			writeRawModelsJson({});
			registry.refresh();
			expect(registry.find("openrouter", sonnetId)?.name).not.toBe("Second Name");
		});

		test("modelOverrides still apply when the provider also defines models", () => {
			writeRawModelsJson({
				openrouter: {
					baseUrl: "https://my-proxy.example.com/v1",
					apiKey: "OPENROUTER_API_KEY",
					api: "openai-completions",
					models: [
						{
							id: "custom/openrouter-model",
							name: "Custom OpenRouter Model",
							reasoning: false,
							input: ["text"],
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
							contextWindow: 128000,
							maxTokens: 16384,
						},
					],
					modelOverrides: { [sonnetId]: { name: "Overridden Built-in Sonnet" } },
				},
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.find("openrouter", "custom/openrouter-model")).toBeDefined();
			expect(registry.find("openrouter", sonnetId)?.name).toBe("Overridden Built-in Sonnet");
		});
	});

	describe("dynamic provider lifecycle", () => {
		const demoModels = () =>
			providerConfig("https://provider.test/v1", [{ id: "demo-model", name: "Demo Model" }], "openai-completions");
		const demoOAuth = (name: string) => ({
			name,
			login: async () => ({ access: "access", refresh: "refresh", expires: Date.now() + 60_000 }),
			refreshToken: async (credentials: { access: string; refresh: string; expires: number }) => credentials,
			getApiKey: (credentials: { access: string }) => credentials.access,
		});

		test("getProviderDisplayName resolves registered, OAuth, built-in, and fallback names", () => {
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.getProviderDisplayName("openai")).toBe("OpenAI");
			expect(registry.getProviderDisplayName("github-copilot")).toBe("GitHub Copilot");
			expect(registry.getProviderDisplayName("unknown-provider")).toBe("unknown-provider");

			registry.registerProvider("named-provider", { ...demoModels(), name: "Named Provider" });
			expect(registry.getProviderDisplayName("named-provider")).toBe("Named Provider");

			registry.registerProvider("oauth-provider", { ...demoModels(), oauth: demoOAuth("OAuth Provider") });
			expect(registry.getProviderDisplayName("oauth-provider")).toBe("OAuth Provider");
		});

		test("failed registerProvider does not persist invalid streamSimple config", () => {
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(() =>
				registry.registerProvider("broken-provider", {
					streamSimple: (() => {
						throw new Error("should not run");
					}) as ProviderConfigInput["streamSimple"],
				}),
			).toThrow('Provider broken-provider: "api" is required when registering streamSimple.');

			expect(() => registry.refresh()).not.toThrow();
		});

		test("failed registerProvider does not remove existing provider models", () => {
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			registry.registerProvider("demo-provider", demoModels());
			expect(registry.find("demo-provider", "demo-model")).toBeDefined();

			expect(() =>
				registry.registerProvider("demo-provider", {
					baseUrl: "https://provider.test/v2",
					apiKey: "TEST_KEY",
					models: [
						{
							id: "broken-model",
							name: "Broken Model",
							reasoning: false,
							input: ["text"],
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
							contextWindow: 128000,
							maxTokens: 4096,
						},
					],
				}),
			).toThrow('Provider demo-provider, model broken-model: no "api" specified.');

			expect(registry.find("demo-provider", "demo-model")).toBeDefined();
			expect(() => registry.refresh()).not.toThrow();
			expect(registry.find("demo-provider", "demo-model")).toBeDefined();
		});

		test("unregisterProvider restores the built-in OAuth provider", () => {
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const builtInOAuthProvider = getOAuthProvider("anthropic");
			expect(builtInOAuthProvider).toBeDefined();

			registry.registerProvider("anthropic", { oauth: demoOAuth("Custom Anthropic OAuth") });
			expect(getOAuthProvider("anthropic")?.name).toBe("Custom Anthropic OAuth");

			registry.unregisterProvider("anthropic");

			expect(getOAuthProvider("anthropic")).toBe(builtInOAuthProvider);
		});

		test("scheduled catalog refresh preserves other sessions' OAuth providers", async () => {
			vi.useFakeTimers();
			vi.stubEnv("PI_OFFLINE", "1");
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			await registry.refreshModelCatalog();
			const providerId = `sentinel-oauth-${Date.now()}-${Math.random().toString(36).slice(2)}`;
			registerOAuthProvider({
				id: providerId,
				name: "Sentinel OAuth",
				async login() {
					throw new Error("Not used in this test");
				},
				async refreshToken(credentials: { access: string; refresh: string; expires: number }) {
					return credentials;
				},
				getApiKey(credentials: { access: string }) {
					return credentials.access;
				},
			});
			expect(getOAuthProvider(providerId)?.name).toBe("Sentinel OAuth");

			await vi.advanceTimersByTimeAsync(60 * 60_000);
			await Promise.resolve();

			expect(getOAuthProvider(providerId)?.name).toBe("Sentinel OAuth");
		});

		test("unregisterProvider restores the built-in API stream handler", () => {
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const builtInApiProvider = getApiProvider("openai-completions");
			expect(builtInApiProvider).toBeDefined();
			const customStreamSimple = () => {
				throw new Error("custom streamSimple override");
			};

			registry.registerProvider("stream-override-provider", {
				api: "openai-completions",
				streamSimple: customStreamSimple,
			});
			const customApiProvider = getApiProvider("openai-completions");
			expect(customApiProvider).toBeDefined();
			expect(customApiProvider?.streamSimple).not.toBe(builtInApiProvider?.streamSimple);

			registry.unregisterProvider("stream-override-provider");

			const restoredApiProvider = getApiProvider("openai-completions");
			expect(restoredApiProvider).toBeDefined();
			expect(restoredApiProvider?.streamSimple).not.toBe(customApiProvider?.streamSimple);
			expect(restoredApiProvider?.streamSimple.name).toBe(builtInApiProvider?.streamSimple.name);
			expect(restoredApiProvider?.stream.name).toBe(builtInApiProvider?.stream.name);
		});

		describe("dynamic provider override persistence", () => {
			const customProvider = (): ProviderConfigInput =>
				providerConfig("https://custom.test/v1", [{ id: "custom-a" }, { id: "custom-b" }], "openai-completions");
			const customAnthropic = (): ProviderConfigInput => ({
				...providerConfig("https://custom.test/anthropic", [{ id: "custom-claude" }], "anthropic-messages"),
				baseUrl: "https://custom.test/anthropic",
			});

			test.each<[string, string, ProviderConfigInput[], string[] | null, string]>([
				[
					"a baseUrl-only override keeps built-in models",
					"anthropic",
					[{ baseUrl: "https://proxy.test/anthropic" }],
					null,
					"https://proxy.test/anthropic",
				],
				[
					"a models-only override replaces built-in models",
					"anthropic",
					[customAnthropic()],
					["custom-claude"],
					"https://custom.test/anthropic",
				],
				[
					"models plus a later baseUrl override replace built-in models",
					"anthropic",
					[customAnthropic(), { baseUrl: "https://proxy.test/anthropic" }],
					["custom-claude"],
					"https://proxy.test/anthropic",
				],
				[
					"a models-only custom provider registration survives",
					"custom-provider",
					[customProvider()],
					["custom-a", "custom-b"],
					"https://custom.test/v1",
				],
				[
					"a baseUrl-only override keeps custom provider models",
					"custom-provider",
					[customProvider(), { baseUrl: "https://proxy.test/custom" }],
					["custom-a", "custom-b"],
					"https://proxy.test/custom",
				],
			])("%s after refresh", (_name, provider, registrations, expectedIds, expectedBaseUrl) => {
				const registry = ModelRegistry.create(authStorage, modelsJsonPath);

				for (const config of registrations) {
					registry.registerProvider(provider, config);
				}
				registry.refresh();

				const models = getModelsForProvider(registry, provider);
				if (expectedIds) {
					expect(models.map((m) => m.id)).toEqual(expectedIds);
				} else {
					expect(models.length).toBeGreaterThan(1);
				}
				expect(models.every((m) => m.baseUrl === expectedBaseUrl)).toBe(true);
			});

			test("headers-only override keeps custom provider models after refresh", async () => {
				const registry = ModelRegistry.create(authStorage, modelsJsonPath);

				registry.registerProvider(
					"custom-provider",
					providerConfig("https://custom.test/v1", [{ id: "custom-a" }, { id: "custom-b" }], "openai-completions"),
				);
				registry.registerProvider("custom-provider", { headers: { "x-proxy": "enabled" } });
				registry.refresh();

				const models = getModelsForProvider(registry, "custom-provider");
				expect(models.map((m) => m.id)).toEqual(["custom-a", "custom-b"]);
				expect(models.every((m) => m.baseUrl === "https://custom.test/v1")).toBe(true);
				expect(await registry.getApiKeyAndHeaders(models[0])).toMatchObject({
					ok: true,
					headers: { "x-proxy": "enabled" },
				});
			});
		});
	});

	describe("issue #17 explicit refresh", () => {
		test("offline refresh never fetches and reports stale cached sources", async () => {
			vi.stubEnv("PI_OFFLINE", "1");
			authStorage.set("anthropic", { type: "api_key", key: "test-key" });
			const fetchSpy = vi.fn(async () => {
				throw new Error("offline must not fetch");
			});
			vi.stubGlobal("fetch", fetchSpy);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const original = registry.find(
				"anthropic",
				getBundledModels().find((model) => model.provider === "anthropic")!.id,
			);

			const report = await registry.forceRefreshModels();

			expect(fetchSpy).not.toHaveBeenCalled();
			expect(report.startedAt).toBeLessThanOrEqual(report.finishedAt);
			expect(report.sources.find((source) => source.source === "anthropic")).toMatchObject({
				status: "stale",
				reason: "offline",
			});
			expect(registry.find("anthropic", original!.id)).toEqual(original);
		});

		test("an older same-key entitlement response cannot overwrite forced authorization", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("PRIME_API_KEY", "");
			authStorage.set("prime-inference", {
				type: "api_key",
				key: "test-key",
				primeTeam: { teamId: "team-a", name: "Test" },
			});
			const model = (id: string) => ({
				id,
				display_name: id,
				pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
				specs: {
					context_window: 32000,
					max_output_tokens: 4096,
					modalities: { input: ["text"], output: ["text"] },
					supports_reasoning: false,
				},
			});
			let release!: (response: Response) => void;
			let entered!: () => void;
			const held = new Promise<Response>((resolve) => {
				release = resolve;
			});
			const started = new Promise<void>((resolve) => {
				entered = resolve;
			});
			let authenticated = 0;
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
					if (
						String(input) === "https://api.pinference.ai/api/v1/models" &&
						new Headers(init?.headers).has("Authorization")
					) {
						authenticated++;
						if (authenticated === 2) {
							entered();
							return held;
						}
						return new Response(JSON.stringify({ data: [model("internal/forced")] }));
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const pending = registry.refreshAvailableModels();
			try {
				await started;
				const report = await registry.forceRefreshModels({ sources: ["prime-inference-private"] });
				expect(report.sources.find((source) => source.source === "prime-inference-private")?.status).toBe(
					"updated",
				);
				release(new Response(JSON.stringify({ data: [model("internal/old")] })));
				await pending;
				expect(registry.find("prime-inference", "internal/forced")).toBeDefined();
				expect(registry.find("prime-inference", "internal/old")).toBeUndefined();
			} finally {
				release(new Response("not found", { status: 404 }));
			}
		});

		test.each([
			["rejected", 401, false, "failed", "auth-rejected"],
			["malformed", 200, false, "failed", undefined],
			["network", 0, true, "stale", undefined],
		] as const)("Ollama %s with cache=%s reports honest status", async (_name, status, warm, expected, reason) => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("OLLAMA_API_BASE", "https://ollama.com");
			authStorage.set("ollama-cloud", { type: "api_key", key: "fake-key" });
			let fail = false;
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					if (String(input) === "https://ollama.com/v1/models") {
						if (fail) {
							if (status === 0) throw new Error("connection refused");
							return status === 401 ? new Response("denied", { status }) : new Response("malformed");
						}
						return new Response(JSON.stringify({ data: [{ id: "test-model" }] }));
					}
					if (String(input) === "https://ollama.com/api/show") {
						return new Response(
							JSON.stringify({ capabilities: ["tools"], model_info: { "test.context_length": 32000 } }),
						);
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			if (warm) await registry.forceRefreshModels({ sources: ["ollama-cloud"] });
			fail = true;

			const report = await registry.forceRefreshModels({ sources: ["ollama-cloud"] });

			expect(report.sources).toHaveLength(1);
			expect(report.sources[0]).toMatchObject({
				source: "ollama-cloud",
				status: expected,
				...(reason ? { reason } : {}),
			});
		});

		test("resident applies a newer public Prime disk snapshot by persisted timestamp", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			const bundled = getModels("prime-inference") as Model<"openai-completions">[];
			const entries = bundled.map((model) => ({
				id: model.id,
				display_name: `New ${model.name}`,
				pricing: { input_usd_per_mtok: model.cost.input, output_usd_per_mtok: model.cost.output },
				specs: {
					context_window: model.contextWindow,
					max_output_tokens: model.maxTokens,
					modalities: { input: model.input, output: ["text"] },
					supports_reasoning: model.reasoning,
				},
			}));
			const fetchSpy = vi.fn(async (input: string | URL | Request) =>
				String(input) === "https://api.pinference.ai/api/v1/models"
					? new Response(JSON.stringify({ data: entries }))
					: new Response("not found", { status: 404 }),
			);
			vi.stubGlobal("fetch", fetchSpy);
			const resident = ModelRegistry.create(authStorage, modelsJsonPath);
			const writer = ModelRegistry.create(authStorage, modelsJsonPath);
			const written = await writer.forceRefreshModels({ sources: ["prime-inference-public"] });
			const count = fetchSpy.mock.calls.length;

			const applied = await resident.applyModelRefreshSnapshots({ since: written.startedAt });

			expect(applied.applied).toContain("prime-inference-public");
			expect(resident.find("prime-inference", bundled[0]!.id)?.name).toBe(`New ${bundled[0]!.name}`);
			expect(fetchSpy).toHaveBeenCalledTimes(count);
		});

		test("targeted private uses a validated public snapshot without repeating public fetch", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("PRIME_API_KEY", "");
			authStorage.set("prime-inference", {
				type: "api_key",
				key: "test-key",
				primeTeam: { teamId: "team-a", name: "Test" },
			});
			const bundled = getModels("prime-inference") as Model<"openai-completions">[];
			const publicData = bundled.map((model) => ({
				id: model.id,
				pricing: { input_usd_per_mtok: model.cost.input, output_usd_per_mtok: model.cost.output },
				specs: {
					context_window: model.contextWindow,
					max_output_tokens: model.maxTokens,
					modalities: { input: model.input, output: ["text"] },
					supports_reasoning: model.reasoning,
				},
			}));
			let calls = 0;
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					if (String(input) !== "https://api.pinference.ai/api/v1/models")
						return new Response("not found", { status: 404 });
					calls++;
					return new Response(JSON.stringify({ data: calls === 1 ? publicData : [] }));
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			await registry.forceRefreshModels({ sources: ["prime-inference-public"] });

			const report = await registry.forceRefreshModels({ sources: ["prime-inference-private"] });

			expect(calls).toBe(2);
			expect(report.sources.map((entry) => entry.source)).toEqual(["prime-inference-private"]);
			expect(report.sources[0]).toMatchObject({ status: "updated", modelCount: 0 });
		});

		test("older public Prime response after key rotation cannot replace forced catalog", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("PRIME_API_KEY", "");
			authStorage.set("prime-inference", { type: "api_key", key: "old-key" });
			const bundled = getModels("prime-inference") as Model<"openai-completions">[];
			const entries = (label: string) =>
				bundled.map((model) => ({
					id: model.id,
					display_name: `${label} ${model.name}`,
					pricing: { input_usd_per_mtok: model.cost.input, output_usd_per_mtok: model.cost.output },
					specs: {
						context_window: model.contextWindow,
						max_output_tokens: model.maxTokens,
						modalities: { input: model.input, output: ["text"] },
						supports_reasoning: model.reasoning,
					},
				}));
			let release!: (response: Response) => void;
			let entered!: () => void;
			const held = new Promise<Response>((resolve) => {
				release = resolve;
			});
			const started = new Promise<void>((resolve) => {
				entered = resolve;
			});
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
					if (String(input) === "https://api.pinference.ai/api/v1/models") {
						const authorization = new Headers(init?.headers).get("Authorization");
						if (authorization === "Bearer old-key") {
							entered();
							return held;
						}
						return new Response(JSON.stringify({ data: entries("new") }));
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			await registry.refreshAvailableModels();
			try {
				await started;
				authStorage.set("prime-inference", { type: "api_key", key: "new-key" });
				const report = await registry.forceRefreshModels({ sources: ["prime-inference-public"] });
				expect(report.sources.find((source) => source.source === "prime-inference-public")?.status).toBe("updated");
				release(new Response(JSON.stringify({ data: entries("old") })));
				await registry.waitForPendingModelRefreshes(2_000);
				expect(registry.find("prime-inference", bundled[0]!.id)?.name).toBe(`new ${bundled[0]!.name}`);
			} finally {
				release(new Response("not found", { status: 404 }));
			}
		});

		test("older background entitlement cannot replace forced result for the same credential", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("PRIME_API_KEY", "");
			authStorage.set("prime-inference", {
				type: "api_key",
				key: "test-key",
				primeTeam: { teamId: "team-a", name: "Test" },
			});
			const entry = (id: string) => ({
				id,
				pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
				specs: {
					context_window: 32000,
					max_output_tokens: 4096,
					modalities: { input: ["text"], output: ["text"] },
					supports_reasoning: false,
				},
			});
			let release!: (response: Response) => void;
			let entered!: () => void;
			const held = new Promise<Response>((resolve) => {
				release = resolve;
			});
			const started = new Promise<void>((resolve) => {
				entered = resolve;
			});
			let requestCount = 0;
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
					if (
						String(input) === "https://api.pinference.ai/api/v1/models" &&
						new Headers(init?.headers).has("Authorization")
					) {
						requestCount++;
						if (requestCount === 4) {
							entered();
							return held;
						}
						return new Response(
							JSON.stringify({ data: [entry(requestCount <= 2 ? "internal/initial" : "internal/forced")] }),
						);
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			await registry.refreshAvailableModels();
			await registry.waitForPendingModelRefreshes(2_000);
			const cacheFile = join(tempDir, "prime-inference-private-models.json");
			const snapshot = JSON.parse(readFileSync(cacheFile, "utf8"));
			snapshot.refreshedAt = 0;
			writeFileSync(cacheFile, JSON.stringify(snapshot));
			await registry.refreshAvailableModels();
			try {
				await started;
				const report = await registry.forceRefreshModels({ sources: ["prime-inference-private"] });
				expect(report.sources.find((source) => source.source === "prime-inference-private")?.status).toBe(
					"updated",
				);
				release(new Response(JSON.stringify({ data: [entry("internal/old-background")] })));
				await registry.waitForPendingModelRefreshes(2_000);
				expect(registry.find("prime-inference", "internal/forced")).toBeDefined();
				expect(registry.find("prime-inference", "internal/old-background")).toBeUndefined();
			} finally {
				release(new Response("not found", { status: 404 }));
			}
		});

		test("private authorization rejection is not reported as an empty successful entitlement", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("PRIME_API_KEY", "");
			authStorage.set("prime-inference", {
				type: "api_key",
				key: "test-key",
				primeTeam: { teamId: "team-a", name: "Test" },
			});
			vi.stubGlobal(
				"fetch",
				vi.fn(async () => new Response("denied", { status: 401 })),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			const report = await registry.forceRefreshModels({ sources: ["prime-inference-private"] });

			expect(report.sources.find((source) => source.source === "prime-inference-private")).toMatchObject({
				status: "failed",
				reason: "auth-rejected",
			});
			expect(
				registry
					.getAvailable()
					.some((model) => model.provider === "prime-inference" && model.id.startsWith("internal/")),
			).toBe(false);
		});

		test("disk-only apply skips OAuth credentials without renewal or network", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			authStorage.set("anthropic", {
				type: "oauth",
				access: "expired",
				refresh: "refresh",
				expires: Date.now() - 1,
			});
			const fetchSpy = vi.fn(async () => {
				throw new Error("apply must never fetch");
			});
			vi.stubGlobal("fetch", fetchSpy);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const lookup = vi.spyOn(authStorage, "getApiKeyWithSourceToken");

			const report = await registry.applyModelRefreshSnapshots();

			expect(report.missing).toContain("anthropic");
			expect(lookup.mock.calls.some(([provider]) => provider === "anthropic")).toBe(false);
			expect(fetchSpy).not.toHaveBeenCalled();
		});

		test("nonpersistent provider refresh reports cache-write rather than durable update", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) =>
					String(input) === "https://openrouter.ai/api/v1/models"
						? new Response(
								JSON.stringify({
									data: [
										{
											id: "test/memory-only",
											name: "Memory Only",
											context_length: 32000,
											top_provider: { max_completion_tokens: 4096 },
											pricing: { prompt: "0.000001", completion: "0.000002" },
											architecture: { input_modalities: ["text"], output_modalities: ["text"] },
										},
									],
								}),
							)
						: new Response("not found", { status: 404 }),
				),
			);
			const registry = ModelRegistry.inMemory(authStorage);

			const report = await registry.forceRefreshModels({ sources: ["openrouter"] });

			expect(report.sources).toEqual([
				{ source: "openrouter", status: "failed", reason: "cache-write", modelCount: expect.any(Number) },
			]);
			expect(registry.find("openrouter", "test/memory-only")).toBeDefined();
		});

		test("disk apply does not execute command-backed models.json credentials", async () => {
			const marker = join(tempDir, "command-ran");
			writeRawModelsJson({
				anthropic: {
					apiKey: `!sh -c 'touch "${toShPath(marker)}"; echo fake-key'`,
					baseUrl: "https://api.anthropic.com",
				},
			});
			const fetchSpy = vi.fn(async () => {
				throw new Error("disk apply must not fetch");
			});
			vi.stubGlobal("fetch", fetchSpy);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			const report = await registry.applyModelRefreshSnapshots();

			expect(report.missing).toContain("anthropic");
			expect(existsSync(marker)).toBe(false);
			expect(fetchSpy).not.toHaveBeenCalled();
		});

		test("offline forced refresh never renews expired OAuth", async () => {
			vi.stubEnv("PI_OFFLINE", "1");
			authStorage.set("anthropic", {
				type: "oauth",
				access: "expired",
				refresh: "refresh",
				expires: Date.now() - 1,
			});
			const fetchSpy = vi.fn(async () => {
				throw new Error("offline must not fetch");
			});
			vi.stubGlobal("fetch", fetchSpy);
			const lookup = vi.spyOn(authStorage, "getApiKeyWithSourceToken");
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			const report = await registry.forceRefreshModels({ sources: ["anthropic"] });

			expect(report.sources).toEqual([{ source: "anthropic", status: "stale", reason: "offline" }]);
			expect(fetchSpy).not.toHaveBeenCalled();
			expect(lookup.mock.calls.some(([provider]) => provider === "anthropic")).toBe(false);
		});

		test("each resident OAuth worker forces its own missing scope after disk-only apply", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			authStorage.set("anthropic", {
				type: "oauth",
				access: "expired",
				refresh: "refresh",
				expires: Date.now() - 1,
			});
			const lookup = vi.spyOn(authStorage, "getApiKeyWithSourceToken");
			const original = lookup.getMockImplementation();
			lookup.mockImplementation(async (provider, options) =>
				provider === "anthropic"
					? { apiKey: "fake-oauth-key", credentialType: "oauth" as const }
					: original
						? original(provider, options)
						: { apiKey: undefined },
			);
			const requests: string[] = [];
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					requests.push(String(input));
					return new Response(
						JSON.stringify({
							data: [
								{
									id: getBundledModels().find((model) => model.provider === "anthropic")!.id,
									display_name: "Fresh",
								},
							],
						}),
					);
				}),
			);
			const residents = [
				ModelRegistry.create(authStorage, modelsJsonPath),
				ModelRegistry.create(authStorage, modelsJsonPath),
			];
			for (const resident of residents) {
				const applied = await resident.applyModelRefreshSnapshots({ since: Date.now() });
				expect(applied.missing).toContain("anthropic");
			}
			expect(requests).toEqual([]);
			for (const resident of residents) {
				const report = await resident.forceRefreshModels({ sources: ["anthropic"] });
				expect(report.sources.find((source) => source.source === "anthropic")?.status).toBe("updated");
			}
			expect(requests).toHaveLength(2);
		});

		test("targeted refresh fetches only requested source and apply marks old snapshots missing", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			const requests: string[] = [];
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					requests.push(String(input));
					return new Response(JSON.stringify({ data: [] }));
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const report = await registry.forceRefreshModels({ sources: ["openrouter"] });
			expect(report.sources.map((source) => source.source)).toEqual(["openrouter"]);
			expect(requests).toEqual(["https://openrouter.ai/api/v1/models"]);
			const applied = await registry.applyModelRefreshSnapshots({ since: Date.now() + 1_000 });
			expect(applied.missing).toContain("openrouter");
			expect(globalThis.fetch).toHaveBeenCalledTimes(1);
		});

		test("late forced discovery cannot overwrite models after a credential change", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("ANTHROPIC_API_KEY", "");
			authStorage.set("anthropic", { type: "api_key", key: "original-key" });
			const id = getBundledModels().find((candidate) => candidate.provider === "anthropic")!.id;
			let release!: (response: Response) => void;
			let entered!: () => void;
			const held = new Promise<Response>((resolve) => {
				release = resolve;
			});
			const started = new Promise<void>((resolve) => {
				entered = resolve;
			});
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
					if (String(input).startsWith("https://api.anthropic.com/v1/models")) {
						if (new Headers(init?.headers).get("x-api-key") === "original-key") {
							entered();
							return held;
						}
						return new Response(JSON.stringify({ data: [{ id, display_name: "new-key" }] }));
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const pending = registry.forceRefreshModels();
			try {
				await started;
				authStorage.set("anthropic", { type: "api_key", key: "new-key" });
				release(new Response(JSON.stringify({ data: [{ id, display_name: "old-key" }] })));
				const report = await pending;
				expect(report.sources.find((entry) => entry.source === "anthropic")).toMatchObject({
					status: "stale",
					reason: "cancelled",
				});
				expect(registry.find("anthropic", id)?.name).not.toBe("old-key");
			} finally {
				release(new Response("not found", { status: 404 }));
			}
		});

		test("apply-only loads a sibling's same-scope disk snapshot without fetching", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					if (String(input) === "https://openrouter.ai/api/v1/models") {
						return new Response(
							JSON.stringify({
								data: [
									{
										id: "test/sibling",
										name: "Sibling Model",
										context_length: 32000,
										top_provider: { max_completion_tokens: 4096 },
										pricing: { prompt: "0.000001", completion: "0.000002" },
										architecture: { input_modalities: ["text"], output_modalities: ["text"] },
									},
								],
							}),
						);
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const resident = ModelRegistry.create(authStorage, modelsJsonPath);
			const writer = ModelRegistry.create(authStorage, modelsJsonPath);
			expect(resident.find("openrouter", "test/sibling")).toBeUndefined();
			await writer.forceRefreshModels();
			const requests = vi.mocked(globalThis.fetch).mock.calls.length;

			const applied = await resident.applyModelRefreshSnapshots();
			expect(applied.applied).toContain("openrouter");

			expect(globalThis.fetch).toHaveBeenCalledTimes(requests);
			expect(resident.find("openrouter", "test/sibling")?.name).toBe("Sibling Model");
		});

		test("force refresh bypasses fresh discovery cache and applies local overrides", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			writeRawModelsJson({ openrouter: { modelOverrides: { "test/discovered": { name: "Local Name" } } } });
			let revision = "first";
			const requests: string[] = [];
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) => {
					const url = String(input);
					if (url === "https://openrouter.ai/api/v1/models") {
						requests.push(revision);
						return new Response(
							JSON.stringify({
								data: [
									{
										id: "test/discovered",
										name: revision,
										context_length: revision === "first" ? 32000 : 64000,
										top_provider: { max_completion_tokens: 4096 },
										pricing: { prompt: "0.000001", completion: "0.000002" },
										architecture: { input_modalities: ["text"], output_modalities: ["text"] },
									},
								],
							}),
						);
					}
					return new Response("not found", { status: 404 });
				}),
			);
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			await registry.forceRefreshModels();
			const selected = registry.find("openrouter", "test/discovered")!;
			const selectedSnapshot = structuredClone(selected);
			expect(selected.name).toBe("Local Name");
			revision = "second";
			const second = await registry.forceRefreshModels();
			expect(requests).toEqual(["first", "second"]);
			expect(second.sources.find((entry) => entry.source === "openrouter")?.status).toBe("updated");
			expect(registry.find("openrouter", "test/discovered")?.name).toBe("Local Name");
			expect(selected).toEqual(selectedSnapshot);
			expect(registry.find("openrouter", "test/discovered")?.contextWindow).toBe(64000);
			expect(selected.contextWindow).toBe(32000);
		});
	});

	describe("issue #5 credential-scoped discovery", () => {
		test("never loads another credential's cached models after key rotation or logout", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("ANTHROPIC_API_KEY", "");
			const requests: string[] = [];
			const knownId = getBundledModels().find((model) => model.provider === "anthropic")!.id;
			const server = createServer((request, response) => {
				const token = request.headers["x-api-key"];
				requests.push(String(token));
				response.setHeader("Content-Type", "application/json");
				response.end(JSON.stringify({ data: [{ id: knownId, display_name: String(token) }] }));
			});
			await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
			const address = server.address();
			if (!address || typeof address === "string") throw new Error("Missing test server address");
			const nativeFetch = globalThis.fetch;
			vi.stubGlobal("fetch", (input: string | URL | Request, init?: RequestInit) => {
				if (String(input).startsWith("https://api.anthropic.com/v1/models"))
					return nativeFetch(`http://127.0.0.1:${address.port}/models`, init);
				return Promise.resolve(new Response("not found", { status: 404 }));
			});
			try {
				authStorage.set("anthropic", { type: "api_key", key: "key-a" });
				const registry = ModelRegistry.create(authStorage, modelsJsonPath);
				await registry.refreshAvailableModels();
				await registry.waitForPendingModelRefreshes(2_000);
				expect(registry.find("anthropic", knownId)?.name).toBe("key-a");
				expect(requests).toEqual(["key-a"]);
				authStorage.set("anthropic", { type: "api_key", key: "key-b" });
				expect(registry.find("anthropic", knownId)?.name).not.toBe("key-a");
				await registry.refreshAvailableModels();
				await registry.waitForPendingModelRefreshes(2_000);
				expect(registry.find("anthropic", knownId)?.name).toBe("key-b");
				expect(registry.find("anthropic", knownId)?.name).not.toBe("key-a");
				expect(requests).toEqual(["key-a", "key-b"]);
				authStorage.logout("anthropic");
				expect(registry.find("anthropic", knownId)?.name).not.toBe("key-b");
			} finally {
				server.closeAllConnections();
				await new Promise<void>((resolve) => server.close(() => resolve()));
			}
		});
	});

	describe("issue #5 auth lookup ordering", () => {
		test("a delayed old credential cannot replace the rotated provider scope", async () => {
			vi.stubEnv("PI_OFFLINE", "0");
			vi.stubEnv("ANTHROPIC_API_KEY", "");
			const knownId = getBundledModels().find((model) => model.provider === "anthropic")!.id;
			authStorage.set("anthropic", { type: "api_key", key: "old-key" });
			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			let release!: () => void;
			let entered!: () => void;
			const held = new Promise<void>((resolve) => {
				release = resolve;
			});
			const started = new Promise<void>((resolve) => {
				entered = resolve;
			});
			const original = authStorage.getApiKeyWithSourceToken.bind(authStorage);
			let delayOnce = true;
			const lookup = vi
				.spyOn(authStorage, "getApiKeyWithSourceToken")
				.mockImplementation(async (provider, options) => {
					const result = await original(provider, options);
					if (provider === "anthropic" && delayOnce) {
						delayOnce = false;
						entered();
						await held;
					}
					return result;
				});
			const requests: string[] = [];
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
					if (String(input).startsWith("https://api.anthropic.com/v1/models")) {
						const key = new Headers(init?.headers).get("x-api-key")!;
						requests.push(key);
						return new Response(JSON.stringify({ data: [{ id: knownId, display_name: key }] }));
					}
					return new Response("missing", { status: 404 });
				}),
			);
			try {
				const old = registry.refreshModelCatalog();
				await started;
				authStorage.set("anthropic", { type: "api_key", key: "new-key" });
				release();
				await old;
				await registry.refreshAvailableModels();
				await registry.waitForPendingModelRefreshes(2_000);
				expect(requests).not.toContain("old-key");
				expect(registry.find("anthropic", knownId)?.name).toBe("new-key");
			} finally {
				release();
				lookup.mockRestore();
			}
		});
	});

	describe("issue #5 provider-owned discovery", () => {
		test.each([
			["output-only audio", { output_modalities: ["audio"] }],
			["input-only image", { input_modalities: ["image"] }],
		] as const)(
			"uses provider endpoint, retains partial data, and revokes %s across offline restart",
			async (_name, revokedArchitecture) => {
				vi.stubEnv("PI_OFFLINE", "0");
				let payload: unknown = {
					data: [
						{
							id: "test/discovered",
							name: "First",
							context_length: 32000,
							top_provider: { max_completion_tokens: 4096 },
							pricing: { prompt: "0.000001", completion: "0.000002" },
							architecture: { input_modalities: ["text"], output_modalities: ["text"] },
						},
					],
				};
				const seen: string[] = [];
				const server = createServer((request, response) => {
					seen.push(request.url ?? "");
					response.setHeader("Content-Type", "application/json");
					response.end(JSON.stringify(payload));
				});
				await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
				const address = server.address();
				if (!address || typeof address === "string") throw new Error("Missing test server address");
				const localUrl = `http://127.0.0.1:${address.port}`;
				const nativeFetch = globalThis.fetch;
				vi.stubGlobal("fetch", (input: string | URL | Request, init?: RequestInit) => {
					if (String(input) === "https://openrouter.ai/api/v1/models")
						return nativeFetch(`${localUrl}/models`, init);
					return Promise.resolve(new Response("not found", { status: 404 }));
				});
				try {
					const registry = ModelRegistry.create(authStorage, modelsJsonPath);
					const pinned = getModelsForProvider(registry, "openrouter")[0];
					await registry.refreshAvailableModels();
					await registry.waitForPendingModelRefreshes(2_000);
					expect(seen).toEqual(["/models"]);
					expect(registry.find("openrouter", "test/discovered")).toMatchObject({
						name: "First",
						contextWindow: 32000,
					});
					expect(registry.find("openrouter", pinned!.id)).toBeDefined();
					expect(registry.getProviderCatalogStatus("openrouter").source).toBe("network");
					payload = {
						data: [
							{
								id: "test/other",
								name: "Other",
								context_length: 64000,
								top_provider: { max_completion_tokens: 8192 },
								pricing: { prompt: "0.000003", completion: "0.000004" },
								architecture: { input_modalities: ["text"], output_modalities: ["text"] },
							},
						],
					};
					const cachePath = join(tempDir, "models", "openrouter-models.v1.json");
					const snapshot = JSON.parse(readFileSync(cachePath, "utf8"));
					snapshot.fetchedAt = Date.now() - 60 * 60_000 - 1;
					snapshot.checkedAt = snapshot.fetchedAt;
					writeFileSync(cachePath, JSON.stringify(snapshot));
					const restored = ModelRegistry.create(authStorage, modelsJsonPath);
					expect(restored.find("openrouter", "test/discovered")?.name).toBe("First");
					await restored.refreshAvailableModels();
					await restored.waitForPendingModelRefreshes(2_000);
					expect(restored.find("openrouter", "test/other")?.name).toBe("Other");
					expect(restored.find("openrouter", "test/discovered")?.name).toBe("First");
					vi.stubEnv("PI_OFFLINE", "1");
					const offline = ModelRegistry.create(authStorage, modelsJsonPath);
					expect(offline.find("openrouter", "test/other")?.name).toBe("Other");
					expect(offline.find("openrouter", "test/discovered")?.name).toBe("First");
					vi.stubEnv("PI_OFFLINE", "0");
					payload = {
						data: [{ id: "test/discovered", architecture: revokedArchitecture }],
					};
					const stale = JSON.parse(readFileSync(cachePath, "utf8"));
					stale.fetchedAt = Date.now() - 60 * 60_000 - 1;
					stale.checkedAt = stale.fetchedAt;
					writeFileSync(cachePath, JSON.stringify(stale));
					const revoked = ModelRegistry.create(authStorage, modelsJsonPath);
					expect(revoked.find("openrouter", "test/discovered")).toBeDefined();
					await revoked.refreshAvailableModels();
					await revoked.waitForPendingModelRefreshes(2_000);
					expect(revoked.find("openrouter", "test/discovered")).toBeUndefined();
					vi.stubEnv("PI_OFFLINE", "1");
					expect(
						ModelRegistry.create(authStorage, modelsJsonPath).find("openrouter", "test/discovered"),
					).toBeUndefined();
				} finally {
					server.closeAllConnections();
					await new Promise<void>((resolve) => server.close(() => resolve()));
				}
			},
		);
	});

	describe("auth refresh across processes", () => {
		test("refreshAvailableModels returns while provider catalog refresh is still pending", async () => {
			vi.stubEnv("OPENAI_API_KEY", "test-openai-key");
			const registry = ModelRegistry.inMemory(authStorage);
			let providerCatalogRequested = false;
			vi.stubGlobal(
				"fetch",
				vi.fn((input: string | URL | Request) => {
					if (String(input).includes("openrouter.ai/api/v1/models")) providerCatalogRequested = true;
					return new Promise<Response>(() => {});
				}),
			);

			// Deterministic non-blocking proof: refreshAvailableModels resolves before the
			// pending fetch settles, and the fetch was started. A deferred promise stands in
			// for the network; no wall-clock timer is involved.
			const models = await registry.refreshAvailableModels();
			expect(models.length).toBeGreaterThan(0);
			expect(providerCatalogRequested).toBe(true);
			await registry.waitForPendingModelRefreshes(1_000).catch(() => undefined);
		});

		test("model catalog includes unauthenticated public models and hides private Prime routes", async () => {
			const savedPrimeApiKey = process.env.PRIME_API_KEY;
			const savedOpenAiApiKey = process.env.OPENAI_API_KEY;
			delete process.env.PRIME_API_KEY;
			delete process.env.OPENAI_API_KEY;
			try {
				const registry = ModelRegistry.create(authStorage, modelsJsonPath);

				const unauthenticated = await registry.refreshModelCatalog();
				expect(unauthenticated.models.some((model) => model.provider === "openai")).toBe(true);
				expect(unauthenticated.configuredProviders).not.toContain("openai");
				expect(
					unauthenticated.models.some(
						(model) => model.provider === "prime-inference" && model.id.startsWith("internal/"),
					),
				).toBe(false);

				authStorage.setRuntimeApiKey("openai", "test-key");
				const authenticated = await registry.refreshModelCatalog();
				expect(authenticated.configuredProviders).toContain("openai");
			} finally {
				if (savedPrimeApiKey !== undefined) {
					process.env.PRIME_API_KEY = savedPrimeApiKey;
				}
				if (savedOpenAiApiKey !== undefined) {
					process.env.OPENAI_API_KEY = savedOpenAiApiKey;
				}
			}
		});

		test("refresh() picks up credentials written by another process", () => {
			const savedEnvKey = process.env.PRIME_API_KEY;
			delete process.env.PRIME_API_KEY;
			try {
				const registry = ModelRegistry.create(authStorage, modelsJsonPath);
				expect(registry.getAvailable().some((m) => m.provider === "prime-inference")).toBe(false);

				const otherProcessAuth = AuthStorage.create(join(tempDir, "auth.json"));
				otherProcessAuth.set("prime-inference", { type: "api_key", key: "test-key" });

				registry.refresh();
				expect(registry.getAvailable().some((m) => m.provider === "prime-inference")).toBe(true);
			} finally {
				if (savedEnvKey !== undefined) {
					process.env.PRIME_API_KEY = savedEnvKey;
				}
			}
		});
	});

	describe("API key resolution and stale auth", () => {
		function providerWithApiKey(apiKey: string) {
			return {
				baseUrl: "https://example.com/v1",
				apiKey,
				api: "anthropic-messages",
				models: [
					{
						id: "test-model",
						name: "Test Model",
						reasoning: false,
						input: ["text"],
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
						contextWindow: 100000,
						maxTokens: 8000,
					},
				],
			};
		}

		test.each<[string, string, Record<string, unknown>]>([
			[
				"an environment variable name",
				"TEST_API_KEY_STATUS_TEST_98765",
				{ configured: true, source: "environment", label: "TEST_API_KEY_STATUS_TEST_98765" },
			],
			["a literal value", "literal_api_key_value", { configured: true, source: "models_json_key" }],
		])("provider auth status reports %s from models.json", (_name, apiKey, expected) => {
			vi.stubEnv("TEST_API_KEY_STATUS_TEST_98765", "status-test-key");
			writeRawModelsJson({ "custom-provider": providerWithApiKey(apiKey) });

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.getProviderAuthStatus("custom-provider")).toEqual(expected);
		});

		test("provider auth status reports command apiKey values without executing them", () => {
			const counterFile = join(tempDir, "status-counter");
			writeFileSync(counterFile, "0");
			const counterPath = toShPath(counterFile);
			writeRawModelsJson({
				"custom-provider": providerWithApiKey(`!sh -c 'echo 1 > "${counterPath}"; echo key-value'`),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: true,
				source: "models_json_command",
			});
			expect(readFileSync(counterFile, "utf-8")).toBe("0");
		});

		test("provider auth status reports stale command auth without executing it", () => {
			const counterFile = join(tempDir, "stale-status-counter");
			writeFileSync(counterFile, "0");
			const counterPath = toShPath(counterFile);
			const command = `!sh -c 'count=$(cat "${counterPath}"); echo $((count + 1)) > "${counterPath}"; echo key-value'`;
			writeRawModelsJson({
				"custom-provider": providerWithApiKey(command),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			expect(registry.markProviderAuthStale("custom-provider")).toBe(true);
			expect(readFileSync(counterFile, "utf-8").trim()).toBe("1");
			writeFileSync(counterFile, "0");

			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: false,
				source: "stale",
				label: "expired",
			});
			expect(readFileSync(counterFile, "utf-8").trim()).toBe("0");
		});

		test("provider auth status reports models.json auth when stored auth is stale", async () => {
			authStorage.setRuntimeApiKey("custom-provider", "stale-runtime-key");
			expect(authStorage.markAuthStale("custom-provider")).toBe(true);
			writeRawModelsJson({
				"custom-provider": providerWithApiKey("literal_api_key_value"),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const model = registry.find("custom-provider", "test-model");
			expect(model).toBeDefined();

			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: true,
				source: "models_json_key",
			});
			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("literal_api_key_value");
			await expect(registry.getApiKeyAndHeaders(model!)).resolves.toMatchObject({
				ok: true,
				apiKey: "literal_api_key_value",
			});
		});

		test("stale marking uses the auth source resolved for the last request", async () => {
			const providerId = `test-oauth-fallback-${Date.now()}-${Math.random().toString(36).slice(2)}`;
			registerOAuthProvider({
				id: providerId,
				name: "Test OAuth Fallback",
				async login() {
					throw new Error("Not used in this test");
				},
				async refreshToken() {
					throw new Error("refresh failed");
				},
				getApiKey(credentials) {
					return `Bearer ${credentials.access}`;
				},
			});
			authStorage.set(providerId, {
				type: "oauth",
				refresh: "refresh-token",
				access: "expired-access-token",
				expires: Date.now() - 10_000,
			});
			writeRawModelsJson({
				[providerId]: providerWithApiKey("literal_api_key_value"),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			await expect(registry.getApiKeyForProvider(providerId)).resolves.toBe("literal_api_key_value");
			const token = registry.getCurrentProviderAuthSourceToken(providerId);
			expect(token?.source).toBe("models_json_key");
			expect(token).toBeDefined();
			expect(registry.markProviderAuthSourceStale(token!)).toBe(true);

			await expect(registry.getApiKeyForProvider(providerId)).resolves.toBeUndefined();
			expect(authStorage.getAuthStatus(providerId)).toEqual({
				configured: true,
				source: "stored",
			});
		});

		test("changed literal models.json apiKey no longer matches stale provider marker", async () => {
			writeRawModelsJson({
				"custom-provider": providerWithApiKey("stale-key"),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("stale-key");
			expect(registry.markProviderAuthStale("custom-provider")).toBe(true);
			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: false,
				source: "stale",
				label: "expired",
			});
			expect(registry.getAvailable().some((model) => model.provider === "custom-provider")).toBe(false);

			writeRawModelsJson({
				"custom-provider": providerWithApiKey("fresh-key"),
			});
			registry.refresh();

			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: true,
				source: "models_json_key",
			});
			expect(registry.getAvailable().some((model) => model.provider === "custom-provider")).toBe(true);
			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("fresh-key");
		});

		test("clearProviderAuthStale restores availability for explicit model selection", async () => {
			writeRawModelsJson({
				"custom-provider": providerWithApiKey("literal_api_key_value"),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("literal_api_key_value");
			expect(registry.markProviderAuthStale("custom-provider")).toBe(true);
			expect(registry.getAvailable().some((model) => model.provider === "custom-provider")).toBe(false);

			registry.clearProviderAuthStale("custom-provider");

			expect(registry.getAvailable().some((model) => model.provider === "custom-provider")).toBe(true);
			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("literal_api_key_value");
		});

		test.each([
			"team change",
			"logout",
			"missing team",
			"missing credentials",
			"active credentials",
			"rotated credentials",
		] as const)("recovers stale Prime Agent private-model access but invalidates it after %s", async (change) => {
			vi.stubEnv("PRIME_API_KEY", "");
			vi.stubEnv("PRIME_TEAM_ID", "");
			vi.stubEnv("PI_OFFLINE", "0");
			const configPath = join(tempDir, "prime-config.json");
			writeFileSync(
				configPath,
				JSON.stringify({ api_key: "dev-key", team_id: "dev-team", base_url: "http://localhost:8000" }),
			);
			const agentAuth = AuthStorage.inMemory(
				{
					"prime-inference": {
						type: "api_key",
						key: "prime-test-key",
						primeTeam: { teamId: "team-a", name: "Research" },
					},
				},
				{ primeCliConfigPath: configPath, usePrimeCliConfig: true },
			);
			const registry = ModelRegistry.create(agentAuth, modelsJsonPath);
			const modelId = "internal/live-private-model";
			const fetchSpy = vi.spyOn(globalThis, "fetch").mockImplementation(
				async () =>
					new Response(
						JSON.stringify({
							data: [
								{
									id: modelId,
									pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
									specs: {
										context_window: 200_000,
										max_output_tokens: 20_000,
										supports_reasoning: true,
										modalities: { input: ["text"], output: ["text"] },
									},
								},
							],
						}),
					),
			);
			try {
				registry.registerProvider("unrelated-extension", { baseUrl: "https://unused.invalid" });
				const model = (await registry.refreshAvailableModels()).find((candidate) => candidate.id === modelId)!;
				await registry.waitForPendingModelRefreshes(1000);
				expect(model).toBeDefined();
				const request = fetchSpy.mock.calls.find(([, init]) => new Headers(init?.headers).has("Authorization"));
				expect(String(request?.[0])).toBe("https://api.pinference.ai/api/v1/models");
				expect(new Headers(request?.[1]?.headers).get("Authorization")).toBe("Bearer prime-test-key");
				writeFileSync(
					configPath,
					JSON.stringify({
						api_key: "changed-dev-key",
						team_id: "changed-dev-team",
						base_url: "http://localhost:9000",
					}),
				);
				await expect(agentAuth.getApiKey("prime-inference")).resolves.toBe("prime-test-key");
				expect(agentAuth.getProviderHeaders("prime-inference")).toEqual({ "X-Prime-Team-ID": "team-a" });
				const authorizedFetchCount = fetchSpy.mock.calls.filter(([, init]) =>
					new Headers(init?.headers).has("Authorization"),
				).length;
				expect(authorizedFetchCount).toBeGreaterThan(0);
				expect(registry.markProviderAuthStale("prime-inference")).toBe(true);

				registry.unregisterProvider("unrelated-extension");
				await expect(registry.canUseModel(model, { assumeAuthConfigured: true })).resolves.toBe(true);
				await Promise.all([registry.refreshAvailableModels(), registry.refreshAvailableModels()]);
				expect(registry.find("prime-inference", modelId)).toEqual(model);

				expect(agentAuth.getProviderHeaders("prime-inference")).toEqual({ "X-Prime-Team-ID": "team-a" });
				expect(registry.hasConfiguredAuth(model)).toBe(false);
				await expect(agentAuth.getApiKey("prime-inference")).resolves.toBeUndefined();
				await expect(registry.canUseModel(model, { assumeAuthConfigured: true })).resolves.toBe(true);
				registry.clearProviderAuthStale("prime-inference");
				await expect(registry.canUseModel(model)).resolves.toBe(true);
				await expect(agentAuth.getApiKey("prime-inference")).resolves.toBe("prime-test-key");

				const invalidationFetchCount = fetchSpy.mock.calls.filter(([, init]) =>
					new Headers(init?.headers).has("Authorization"),
				).length;
				expect(registry.markProviderAuthStale("prime-inference")).toBe(true);
				switch (change) {
					case "team change":
						agentAuth.setPrimeInferenceTeamSelection({ teamId: "team-b", name: "Other team" });
						break;
					case "logout":
						agentAuth.logout("prime-inference");
						break;
					case "missing team":
						agentAuth.setPrimeInferenceTeamSelection(null);
						break;
					case "missing credentials":
						agentAuth.remove("prime-inference");
						break;
					case "active credentials":
						registry.clearProviderAuthStale("prime-inference");
						break;
					case "rotated credentials":
						agentAuth.setPrimeInferenceApiKey("rotated-key", { teamId: "team-a", name: "Research" });
						break;
				}
				registry.refresh();
				expect(registry.find("prime-inference", modelId)).toBeUndefined();
				await expect(registry.canUseModel(model, { assumeAuthConfigured: true })).resolves.toBe(false);
				expect(
					fetchSpy.mock.calls.filter(([, init]) => new Headers(init?.headers).has("Authorization")),
				).toHaveLength(invalidationFetchCount);
			} finally {
				fetchSpy.mockRestore();
				vi.unstubAllEnvs();
			}
		});

		test("resolves rotated environment and command credentials without caching", async () => {
			const envKey = "TEST_API_KEY_ROTATION_98765";
			const tokenFile = join(tempDir, "rotating-models-json-token");
			const tokenPath = toShPath(tokenFile);
			vi.stubEnv(envKey, "env-key-1");
			writeFileSync(tokenFile, "command-key-1");
			writeRawModelsJson({
				"env-provider": providerWithApiKey(envKey),
				"command-provider": {
					...providerWithApiKey(`!sh -c 'cat "${tokenPath}"'`),
					authHeader: true,
				},
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);
			const commandModel = registry.find("command-provider", "test-model");
			expect(commandModel).toBeDefined();
			await expect(registry.getApiKeyForProvider("env-provider")).resolves.toBe("env-key-1");
			await expect(registry.getApiKeyAndHeaders(commandModel!)).resolves.toEqual({
				ok: true,
				apiKey: "command-key-1",
				headers: { Authorization: "Bearer command-key-1" },
			});

			vi.stubEnv(envKey, "env-key-2");
			writeFileSync(tokenFile, "command-key-2");

			await expect(registry.getApiKeyForProvider("env-provider")).resolves.toBe("env-key-2");
			await expect(registry.getApiKeyAndHeaders(commandModel!)).resolves.toEqual({
				ok: true,
				apiKey: "command-key-2",
				headers: { Authorization: "Bearer command-key-2" },
			});
		});

		test("changed command-backed apiKey no longer matches stale models.json marker", async () => {
			const tokenFile = join(tempDir, "models-json-token");
			writeFileSync(tokenFile, "stale-key");
			const tokenPath = toShPath(tokenFile);

			writeRawModelsJson({
				"custom-provider": providerWithApiKey(`!sh -c 'cat "${tokenPath}"'`),
			});

			const registry = ModelRegistry.create(authStorage, modelsJsonPath);

			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("stale-key");
			expect(registry.markProviderAuthStale("custom-provider")).toBe(true);
			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBeUndefined();
			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: false,
				source: "stale",
				label: "expired",
			});

			writeFileSync(tokenFile, "fresh-key");

			await expect(registry.getApiKeyForProvider("custom-provider")).resolves.toBe("fresh-key");
			expect(registry.getProviderAuthStatus("custom-provider")).toEqual({
				configured: true,
				source: "models_json_command",
			});
		});
	});
});

describe("subagent Prime Inference discovery", () => {
	test("finds a newly fetched public Prime Inference model without opening the picker", async () => {
		const directory = mkdtempSync(join(tmpdir(), "pi-subagent-models-"));
		try {
			const auth = AuthStorage.create(join(directory, "auth.json"));
			auth.set("prime-inference", { type: "api_key", key: "prime-key" });
			const registry = ModelRegistry.create(auth, join(directory, "models.json"));
			const bundled = getModels("prime-inference") as Model<"openai-completions">[];
			const entries = bundled.map((model) => ({
				id: model.id,
				display_name: model.name,
				pricing: { input_usd_per_mtok: model.cost.input, output_usd_per_mtok: model.cost.output },
				specs: {
					context_window: model.contextWindow,
					max_output_tokens: model.maxTokens,
					modalities: { input: model.input, output: ["text"] },
					supports_reasoning: model.reasoning,
				},
			}));
			entries.push({
				id: "test/new-public-model",
				display_name: "New public model",
				pricing: { input_usd_per_mtok: 1, output_usd_per_mtok: 2 },
				specs: {
					context_window: 200_000,
					max_output_tokens: 20_000,
					modalities: { input: ["text"], output: ["text"] },
					supports_reasoning: false,
				},
			});
			vi.stubGlobal(
				"fetch",
				vi.fn(async (input: string | URL | Request) =>
					String(input).includes("api.pinference.ai/api/v1/models")
						? new Response(JSON.stringify({ object: "list", data: entries }))
						: new Response("not found", { status: 404 }),
				),
			);
			expect(
				(await registry.getExecutableModels()).some(
					(model) => model.provider === "prime-inference" && model.id === "test/new-public-model",
				),
			).toBe(true);
		} finally {
			vi.unstubAllGlobals();
			rmSync(directory, { recursive: true, force: true });
		}
	});
});

describe("issue #702 codex model discovery client version", () => {
	const originalFetch = globalThis.fetch;
	let codexTempDir: string;

	beforeEach(() => {
		codexTempDir = mkdtempSync(join(tmpdir(), "codex-client-version-"));
	});

	afterEach(() => {
		globalThis.fetch = originalFetch;
		rmSync(codexTempDir, { recursive: true, force: true });
	});

	function codexAccessToken(accountId: string): string {
		const payload = Buffer.from(
			JSON.stringify({ "https://api.openai.com/auth": { chatgpt_account_id: accountId } }),
		).toString("base64url");
		return `header.${payload}.signature`;
	}

	test("sends a Codex CLI client version on the discovery request instead of the package version", async () => {
		const authPath = join(codexTempDir, "auth.json");
		writeFileSync(
			authPath,
			JSON.stringify({
				"openai-codex": {
					type: "oauth",
					access: codexAccessToken("account-123"),
					refresh: "refresh-token",
					expires: Date.now() + 60 * 60 * 1000,
					accountId: "account-123",
				},
			}),
		);
		const registry = ModelRegistry.create(AuthStorage.create(authPath), join(codexTempDir, "models.json"));
		const codexModels = registry.getAvailable().filter((model) => model.provider === "openai-codex");
		expect(codexModels.length).toBeGreaterThan(0);
		const requestedUrls: string[] = [];
		globalThis.fetch = (async (input: Parameters<typeof globalThis.fetch>[0]) => {
			requestedUrls.push(input instanceof Request ? input.url : input.toString());
			return new Response(JSON.stringify({ models: codexModels.map((model) => ({ slug: model.id })) }), {
				status: 200,
				headers: { "content-type": "application/json" },
			});
		}) as typeof globalThis.fetch;

		const executable = await registry.getExecutableModels();

		const discoveryUrl = requestedUrls.find((url) => url.includes("/codex/models"));
		expect(discoveryUrl).toBeDefined();
		const clientVersion = new URL(discoveryUrl ?? "").searchParams.get("client_version");
		// Prime Agent's own version is 0.x well below this floor, so comparing against VERSION
		// would pass today and break silently once the package version reaches the pinned constant.
		expect(clientVersion).toMatch(/^\d+\.\d+\.\d+$/);
		const [major, minor] = (clientVersion ?? "0.0.0").split(".").map(Number);
		// 0.155.x is the floor at which ChatGPT discovery also lists GPT-6 Sol and Luna (discussion
		// #2544); 0.153.x only listed GPT-6 Astra (discussion #2062).
		expect((major ?? 0) > 0 || (minor ?? 0) >= 155).toBe(true);
		expect(executable.some((model) => model.provider === "openai-codex")).toBe(true);
	});
});
