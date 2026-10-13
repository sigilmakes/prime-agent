import { Buffer } from "node:buffer";
import type { Api, Model } from "@earendil-works/pi-ai";

export const OLLAMA_CLOUD_PROVIDER = "ollama-cloud";
export const OLLAMA_CLOUD_BASE = "https://ollama.com";

const DEFAULT_THINKING = {
	off: "none",
	minimal: null,
	low: "low",
	medium: "medium",
	high: "high",
	xhigh: "max",
} as const;
const NO_OFF = { ...DEFAULT_THINKING, off: null } as const;
const GPT_OSS = { ...NO_OFF, xhigh: null } as const;
const QWEN3 = { off: "none", minimal: null, low: null, medium: "medium", high: null, xhigh: null } as const;

function object(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}
function validId(value: unknown): value is string {
	return typeof value === "string" && value.length > 0 && value.length <= 200 && !/[\x00-\x1f]/.test(value);
}
function details(value: unknown): { capabilities: string[]; model_info: Record<string, unknown> } | undefined {
	const data = object(value);
	if (!Array.isArray(data?.capabilities) || !data.capabilities.every((item) => typeof item === "string"))
		return undefined;
	return { capabilities: data.capabilities, model_info: object(data.model_info) ?? {} };
}
function rows(payload: unknown): Record<string, unknown> {
	const result = object(object(payload)?.models);
	if (!result) throw new Error("Invalid Ollama Cloud catalog");
	return result;
}
function thinking(id: string, capabilities: string[]) {
	if (!capabilities.includes("thinking")) return undefined;
	if (id.startsWith("gpt-oss")) return GPT_OSS;
	if (id.startsWith("qwen3-vl") || id === "kimi-k2-thinking" || id.startsWith("minimax")) return NO_OFF;
	if (id.startsWith("qwen3")) return QWEN3;
	return DEFAULT_THINKING;
}

/** Discovery supplies model metadata only. Never accept network-supplied transport or auth fields. */
export function parseOllamaCloudCatalog(payload: unknown, baseUrl: string): Model<Api>[] {
	const models: Model<Api>[] = [];
	for (const [id, raw] of Object.entries(rows(payload))) {
		if (!validId(id)) continue;
		const data = details(raw);
		if (!data?.capabilities.includes("tools")) continue;
		const context = Object.entries(data.model_info).find(
			([key, value]) =>
				key.endsWith(".context_length") && typeof value === "number" && Number.isFinite(value) && value > 0,
		)?.[1];
		models.push({
			id,
			name: id,
			provider: OLLAMA_CLOUD_PROVIDER,
			api: "openai-completions",
			baseUrl: `${baseUrl}/v1`,
			reasoning: data.capabilities.includes("thinking"),
			thinkingLevelMap: thinking(id, data.capabilities),
			input: data.capabilities.includes("vision") ? ["text", "image"] : ["text"],
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: typeof context === "number" ? context : 128000,
			maxTokens: 32768,
			compat: {
				supportsDeveloperRole: false,
				supportsReasoningEffort: true,
				supportsStore: false,
				maxTokensField: "max_tokens",
				supportsUsageInStreaming: true,
				requiresToolResultName: false,
				requiresAssistantAfterToolResult: false,
				requiresThinkingAsText: false,
				requiresReasoningContentOnAssistantMessages: false,
				thinkingFormat: "openai",
				supportsStrictMode: false,
				sendSessionAffinityHeaders: false,
				supportsLongCacheRetention: false,
				zaiToolStream: false,
				openRouterRouting: {},
				vercelGatewayRouting: {},
			},
		});
	}
	if (!models.length) throw new Error("Ollama Cloud catalog contains no tool-capable models");
	return models;
}

/** Keep last-good details when /api/show fails for individual IDs. */
export function mergeOllamaCloudCatalog(payload: unknown, previous: unknown): unknown {
	const next = rows(payload);
	const prior = previous ? rows(previous) : {};
	const listed = object(payload)?.listedIds;
	const denied = object(payload)?.deniedIds;
	if (!Array.isArray(listed) || !listed.every(validId) || !Array.isArray(denied) || !denied.every(validId))
		throw new Error("Invalid Ollama Cloud model listing metadata");
	const live = new Set(listed);
	const revoked = new Set(denied);
	const merged: Record<string, unknown> = {};
	for (const [id, value] of Object.entries(prior)) {
		if (live.has(id) && !revoked.has(id) && details(value)) merged[id] = details(value);
	}
	for (const [id, value] of Object.entries(next)) {
		if (live.has(id) && !revoked.has(id) && details(value)) merged[id] = details(value);
	}
	return { models: merged, listedIds: listed, deniedIds: denied };
}

const MAX_LIST_BYTES = 2 * 1024 * 1024;
const MAX_SHOW_BYTES = 512 * 1024;
const MAX_CATALOG_BYTES = 5 * 1024 * 1024;

async function boundedJson(response: Response, maxBytes: number): Promise<unknown> {
	if (Number(response.headers.get("content-length")) > maxBytes) {
		await response.body?.cancel();
		throw new Error("Ollama Cloud response is too large");
	}
	if (!response.body) throw new Error("Ollama Cloud response body is empty");
	const reader = response.body.getReader();
	const chunks: Uint8Array[] = [];
	let total = 0;
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			total += value.byteLength;
			if (total > maxBytes) {
				await reader.cancel();
				throw new Error("Ollama Cloud response is too large");
			}
			chunks.push(value);
		}
	} finally {
		reader.releaseLock();
	}
	return JSON.parse(Buffer.concat(chunks, total).toString("utf8")) as unknown;
}

/** Fetch the two provider-owned endpoints. CatalogCache owns timeout, freshness, status and persistence. */
export function ollamaCloudFetch(baseUrl: string, fetchFn: typeof fetch = fetch): typeof fetch {
	return async (_url, init) => {
		const headers = new Headers(init?.headers);
		const list = await fetchFn(`${baseUrl}/v1/models`, { headers, signal: init?.signal, redirect: "error" });
		if (!list.ok) return list;
		const body: unknown = await boundedJson(list, MAX_LIST_BYTES);
		const entries = object(body)?.data;
		if (!Array.isArray(entries)) throw new Error("Invalid Ollama Cloud model list");
		const ids = [...new Set(entries.map((row) => object(row)?.id).filter(validId))];
		if (!ids.length) throw new Error("Empty Ollama Cloud model list");
		const models: Record<string, unknown> = {};
		const deniedIds: string[] = [];
		let bytes = 0;
		let index = 0;
		await Promise.all(
			Array.from({ length: Math.min(8, ids.length) }, async () => {
				while (index < ids.length) {
					const id = ids[index++];
					try {
						const response = await fetchFn(`${baseUrl}/api/show`, {
							method: "POST",
							headers: new Headers({ ...Object.fromEntries(headers), "content-type": "application/json" }),
							body: JSON.stringify({ model: id }),
							signal: init?.signal,
							redirect: "error",
						});
						if (response.status === 401 || response.status === 403) deniedIds.push(id);
						if (!response.ok) continue;
						const value: unknown = await boundedJson(response, MAX_SHOW_BYTES);
						const safe = details(value);
						if (safe) {
							const size = Buffer.byteLength(JSON.stringify(safe), "utf8") + Buffer.byteLength(id, "utf8") + 8;
							if (bytes + size <= MAX_CATALOG_BYTES) {
								bytes += size;
								models[id] = safe;
							}
						}
					} catch {
						/* Preserve last-good metadata on partial failures. */
					}
				}
			}),
		);
		if (!Object.keys(models).length && deniedIds.length > 0) return new Response(null, { status: 401 });
		if (!Object.keys(models).length) throw new Error("Ollama Cloud model details unavailable");
		return new Response(JSON.stringify({ models, listedIds: ids, deniedIds }), {
			status: 200,
			headers: { "content-type": "application/json" },
		});
	};
}

/** Pinned first-use fallback from the Nix-managed extension catalog (2026-09-14). */
const PINNED_MODELS: readonly (readonly [string, number, boolean, boolean])[] = [
	["deepseek-v4-flash:0731", 1048576, false, true],
	["deepseek-v4-pro:0813", 1048576, false, true],
	["deepseek-v4.1-flash", 1048576, true, true],
	["gemma4:31b", 262144, true, true],
	["glm-5.1", 202752, false, true],
	["glm-5.2", 1048576, false, true],
	["glm-5.3", 1048576, false, true],
	["glm-5.3-flash", 1048576, true, true],
	["gpt-oss:120b", 131072, false, true],
	["gpt-oss:20b", 131072, false, true],
	["kimi-k2.6", 262144, true, true],
	["kimi-k2.7-code", 262144, true, true],
	["kimi-k3", 1048576, true, true],
	["minimax-m2.7", 196608, false, true],
	["minimax-m3", 512000, true, true],
	["mistral-large-3:675b", 262144, true, false],
	["nemotron-3-nano:30b", 262144, false, true],
	["nemotron-3-super", 262144, false, true],
	["nemotron-3-ultra", 262144, false, true],
	["qwen3.5:397b", 262144, true, true],
];

export function pinnedOllamaCloudModels(baseUrl: string): Model<Api>[] {
	return parseOllamaCloudCatalog(
		{
			models: Object.fromEntries(
				PINNED_MODELS.map(([id, context, vision, reasoning]) => [
					id,
					{
						capabilities: ["tools", ...(vision ? ["vision"] : []), ...(reasoning ? ["thinking"] : [])],
						model_info: { "pinned.context_length": context },
					},
				]),
			),
		},
		baseUrl,
	);
}
