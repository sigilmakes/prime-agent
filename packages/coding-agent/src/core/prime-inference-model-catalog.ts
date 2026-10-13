import { Buffer } from "node:buffer";
import { createHmac, randomBytes } from "node:crypto";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import {
	type Api,
	getPrimeInferenceReasoningControls,
	isPrivatePrimeInferenceModelId,
	type Model,
	type OpenAICompletionsCompat,
	type PrimeInferenceCatalogEntry,
	parsePrimeInferenceModelCatalog,
} from "@earendil-works/pi-ai";

import { writeFileAtomicSync } from "../utils/atomic-file.js";

export const PRIME_INFERENCE_BASE_URL = "https://api.pinference.ai/api/v1";
const FETCH_TIMEOUT_MS = 5_000;
const MAX_RESPONSE_BYTES = 2 * 1024 * 1024;
const MIN_CATALOG_COVERAGE = 0.5;
export type PrimeInferenceCatalogRefreshOutcome = {
	status:
		| "updated"
		| "memory-only"
		| "offline"
		| "cached-fallback"
		| "failed"
		| "auth-rejected"
		| "cancelled"
		| "superseded";
	models?: Model<"openai-completions">[];
};

type PendingRefresh = {
	scope: string;
	promise: Promise<PrimeInferenceCatalogRefreshOutcome>;
	controller: AbortController;
	waiters: number;
};

const pendingRefreshes = new Map<string, PendingRefresh>();
const scopeSecret = randomBytes(32);

function requestScope(headers?: Record<string, string>): string {
	const normalized = [...new Headers(headers).entries()].sort(([a], [b]) => a.localeCompare(b));
	return createHmac("sha256", scopeSecret).update(JSON.stringify(normalized)).digest("hex");
}

function waitForRefresh(
	cachePath: string,
	pending: PendingRefresh,
	cached: Model<"openai-completions">[] | undefined,
	signal?: AbortSignal,
): Promise<PrimeInferenceCatalogRefreshOutcome> {
	pending.waiters++;
	return new Promise((resolve) => {
		let settled = false;
		const finish = (outcome: PrimeInferenceCatalogRefreshOutcome) => {
			if (settled) return;
			settled = true;
			signal?.removeEventListener("abort", onAbort);
			pending.waiters--;
			if (pending.waiters === 0 && signal?.aborted && pendingRefreshes.get(cachePath) === pending) {
				pendingRefreshes.delete(cachePath);
				pending.controller.abort();
			}
			resolve(outcome);
		};
		const onAbort = () => finish({ status: "cancelled", models: cached });
		signal?.addEventListener("abort", onAbort, { once: true });
		void pending.promise.then(finish);
		if (signal?.aborted) onAbort();
	});
}

const DEFAULT_COMPAT: OpenAICompletionsCompat = {
	supportsStore: false,
	supportsDeveloperRole: false,
	// Routes that do not describe their reasoning controls get no unconfirmed
	// reasoning_effort parameter; live supported_parameters drive the override.
	supportsReasoningEffort: false,
	maxTokensField: "max_tokens",
	supportsStrictMode: false,
};

function cacheCosts(entry: PrimeInferenceCatalogEntry, template?: Model<"openai-completions">) {
	const anthropic = entry.id.toLowerCase().startsWith("anthropic/");
	return {
		cacheRead: entry.cacheRead ?? template?.cost.cacheRead ?? (anthropic ? entry.input * 0.1 : 0),
		cacheWrite: entry.cacheWrite ?? template?.cost.cacheWrite ?? (anthropic ? entry.input * 1.25 : 0),
	};
}

export function buildPrimeInferenceModels(
	bundledModels: readonly Model<"openai-completions">[],
	entries: readonly PrimeInferenceCatalogEntry[],
	options: { includePrivate?: boolean; minimumModels?: number } = {},
): Model<"openai-completions">[] | undefined {
	const bundled = new Map(bundledModels.map((model) => [model.id.toLowerCase(), model]));
	const models: Model<"openai-completions">[] = [];
	for (const entry of entries) {
		if (!options.includePrivate && isPrivatePrimeInferenceModelId(entry.id)) continue;
		const template = bundled.get(entry.id.toLowerCase());
		if (!template && (!entry.contextWindow || !entry.maxTokens || entry.reasoning === undefined)) continue;
		const contextWindow = entry.contextWindow ?? template?.contextWindow ?? 0;
		const maxTokens = Math.min(entry.maxTokens ?? template?.maxTokens ?? 0, contextWindow);
		const compat = structuredClone(template?.compat ?? DEFAULT_COMPAT);
		const controls = getPrimeInferenceReasoningControls(entry);
		if (controls) {
			// The live catalog is authoritative for which reasoning parameters the
			// route accepts; never emit one it does not declare.
			compat.supportsReasoningEffort = controls.supportsReasoningEffort;
			if (controls.thinkingFormat) compat.thinkingFormat = controls.thinkingFormat;
			else delete compat.thinkingFormat;
		}
		// The live map is authoritative when present; no reported parameter
		// support keeps the whole stale template. A route that declares
		// reasoning_effort but no supported efforts keeps the template's map:
		// the param is accepted but its values are unknown, and the template
		// bounds the selection better than the raw default levels. A route that
		// declares supported parameters but no reasoning controls drops the
		// template map entirely.
		const thinkingLevelMap = !controls
			? template?.thinkingLevelMap
			: (controls.thinkingLevelMap ?? (controls.supportsReasoningEffort ? template?.thinkingLevelMap : undefined));
		// Anthropic models cache with explicit breakpoints, not automatic
		// server-side prefix caching; cacheControlFormat makes the provider add
		// anthropic-style cache_control markers for these entries. The catalog
		// already prices anthropic/* with Anthropic cache economics (10% cache
		// reads, 125% cache writes), so the wire format follows the pricing.
		if (entry.id.toLowerCase().startsWith("anthropic/")) {
			compat.cacheControlFormat = "anthropic";
		}
		models.push({
			id: entry.id,
			name: entry.name ?? template?.name ?? entry.id,
			api: "openai-completions",
			provider: "prime-inference",
			baseUrl: PRIME_INFERENCE_BASE_URL,
			reasoning: entry.reasoning ?? template?.reasoning ?? false,
			...(thinkingLevelMap ? { thinkingLevelMap: { ...thinkingLevelMap } } : {}),
			input: (entry.vision ?? template?.input.includes("image")) ? ["text", "image"] : ["text"],
			cost: { input: entry.input, output: entry.output, ...cacheCosts(entry, template) },
			contextWindow,
			maxTokens,
			...(template?.featured ? { featured: true } : {}),
			compat,
		});
	}
	const minimumModels = options.minimumModels ?? Math.ceil(bundledModels.length * MIN_CATALOG_COVERAGE);
	const coveredBundledModels = models.filter((model) => bundled.has(model.id.toLowerCase())).length;
	return coveredBundledModels >= minimumModels ? models : undefined;
}

export function mergePrimeInferenceModels(
	bundledModels: readonly Model<Api>[],
	livePrimeInferenceModels?: readonly Model<"openai-completions">[],
): Model<Api>[] {
	if (!livePrimeInferenceModels) return [...bundledModels];
	return [...bundledModels.filter((model) => model.provider !== "prime-inference"), ...livePrimeInferenceModels];
}

export function readCachedPrimeInferenceModels(
	cachePath: string,
	bundledModels: readonly Model<"openai-completions">[],
): Model<"openai-completions">[] | undefined {
	return readCachedPrimeInferenceModelsWithStatus(cachePath, bundledModels)?.models;
}

const CACHE_FORMAT = "prime-inference-catalog-v1";

export function readCachedPrimeInferenceModelsWithStatus(
	cachePath: string,
	bundledModels: readonly Model<"openai-completions">[],
): { models: Model<"openai-completions">[]; checkedAt?: number } | undefined {
	// Legacy flat and catalog paths still contain a raw response without a timestamp.
	const flat = join(dirname(cachePath), "..", basename(cachePath));
	const catalog = join(dirname(cachePath), "..", "catalog", basename(cachePath));
	for (const candidate of [cachePath, flat, catalog]) {
		if (!existsSync(candidate)) continue;
		try {
			const value: unknown = JSON.parse(readFileSync(candidate, "utf8"));
			const envelope =
				value !== null && typeof value === "object" && "format" in value && value.format === CACHE_FORMAT
					? value
					: undefined;
			const payload = envelope && "payload" in envelope ? envelope.payload : value;
			const models = buildPrimeInferenceModels(bundledModels, parsePrimeInferenceModelCatalog(payload));
			if (!models) continue;
			const checkedAt = envelope && "checkedAt" in envelope ? envelope.checkedAt : undefined;
			return {
				models,
				...(typeof checkedAt === "number" &&
				Number.isSafeInteger(checkedAt) &&
				checkedAt > 0 &&
				checkedAt <= Date.now()
					? { checkedAt }
					: {}),
			};
		} catch {
			// Try the next candidate location.
		}
	}
	return undefined;
}

function writeCache(cachePath: string, payload: unknown, checkedAt: number): boolean {
	try {
		mkdirSync(dirname(cachePath), { recursive: true });
		writeFileAtomicSync(cachePath, JSON.stringify({ format: CACHE_FORMAT, checkedAt, payload }), { mode: 0o600 });
		return true;
	} catch {
		return false;
	}
}

export class PrimeInferenceCatalogRequestError extends Error {
	constructor(readonly status: number) {
		super(`Prime Inference model catalog request failed with status ${status}`);
	}
}

async function readResponse(response: Response): Promise<unknown> {
	if (!response.ok) throw new PrimeInferenceCatalogRequestError(response.status);
	const contentLength = Number(response.headers.get("content-length"));
	if (Number.isFinite(contentLength) && contentLength > MAX_RESPONSE_BYTES) throw new Error("Response is too large");
	if (!response.body) throw new Error("Response body is empty");
	const reader = response.body.getReader();
	const chunks: Uint8Array[] = [];
	let bytesRead = 0;
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			bytesRead += value.byteLength;
			if (bytesRead > MAX_RESPONSE_BYTES) {
				await reader.cancel().catch(() => {});
				throw new Error("Response is too large");
			}
			chunks.push(value);
		}
	} finally {
		reader.releaseLock();
	}
	return JSON.parse(Buffer.concat(chunks, bytesRead).toString("utf8")) as unknown;
}

export async function fetchPrimeInferenceModelCatalog(
	options: {
		fetchFn?: typeof fetch;
		headers?: Record<string, string>;
		timeoutMs?: number;
		allowEmpty?: boolean;
		signal?: AbortSignal;
	} = {},
): Promise<{ payload: unknown; entries: PrimeInferenceCatalogEntry[] }> {
	const response = await (options.fetchFn ?? fetch)(`${PRIME_INFERENCE_BASE_URL}/models`, {
		headers: { accept: "application/json", ...options.headers },
		signal: options.signal
			? AbortSignal.any([options.signal, AbortSignal.timeout(options.timeoutMs ?? FETCH_TIMEOUT_MS)])
			: AbortSignal.timeout(options.timeoutMs ?? FETCH_TIMEOUT_MS),
	});
	const payload = await readResponse(response);
	return { payload, entries: parsePrimeInferenceModelCatalog(payload, { allowEmpty: options.allowEmpty }) };
}

export async function refreshPrimeInferenceModelsWithOutcome(
	cachePath: string,
	bundledModels: readonly Model<"openai-completions">[],
	options: {
		fetchFn?: typeof fetch;
		headers?: Record<string, string>;
		offline?: boolean;
		force?: boolean;
		signal?: AbortSignal;
	} = {},
): Promise<PrimeInferenceCatalogRefreshOutcome> {
	const cached = readCachedPrimeInferenceModels(cachePath, bundledModels);
	if (options.offline) return { status: "offline", models: cached };
	if (options.signal?.aborted) return { status: "cancelled", models: cached };
	const scope = requestScope(options.headers);
	const existing = pendingRefreshes.get(cachePath);
	// Force skips any future freshness policy, but an in-flight request for the
	// same scope is already a fresh request and can be shared.
	if (existing?.scope === scope) return waitForRefresh(cachePath, existing, cached, options.signal);
	const pending = { scope, controller: new AbortController(), waiters: 0 } as PendingRefresh;
	const promise = (async (): Promise<PrimeInferenceCatalogRefreshOutcome> => {
		try {
			const { payload, entries } = await fetchPrimeInferenceModelCatalog({
				fetchFn: options.fetchFn,
				headers: options.headers,
				signal: pending.controller.signal,
			});
			if (pendingRefreshes.get(cachePath) !== pending) return { status: "superseded" };
			if (pending.controller.signal.aborted) return { status: "cancelled", models: cached };
			const models = buildPrimeInferenceModels(bundledModels, entries);
			if (!models) return { status: cached ? "cached-fallback" : "failed", models: cached };
			return { status: writeCache(cachePath, payload, Date.now()) ? "updated" : "memory-only", models };
		} catch (error) {
			if (pendingRefreshes.get(cachePath) !== pending) return { status: "superseded" };
			if (pending.controller.signal.aborted) return { status: "cancelled", models: cached };
			if (error instanceof PrimeInferenceCatalogRequestError && (error.status === 401 || error.status === 403)) {
				return { status: "auth-rejected", models: cached };
			}
			return { status: cached ? "cached-fallback" : "failed", models: cached };
		}
	})();
	pending.promise = promise;
	pendingRefreshes.set(cachePath, pending);
	void promise.finally(() => {
		if (pendingRefreshes.get(cachePath) === pending) pendingRefreshes.delete(cachePath);
	});
	return waitForRefresh(cachePath, pending, cached, options.signal);
}

export async function refreshPrimeInferenceModels(
	cachePath: string,
	bundledModels: readonly Model<"openai-completions">[],
	options: { fetchFn?: typeof fetch; headers?: Record<string, string>; offline?: boolean } = {},
): Promise<Model<"openai-completions">[] | undefined> {
	return (await refreshPrimeInferenceModelsWithOutcome(cachePath, bundledModels, options)).models;
}
