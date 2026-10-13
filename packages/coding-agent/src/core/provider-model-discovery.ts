import type { Api, Model } from "@earendil-works/pi-ai";

/** Only these provider-owned APIs publish a usable model-list endpoint. */
export const PROVIDER_DISCOVERY_URLS = {
	openrouter: "https://openrouter.ai/api/v1/models",
	openai: "https://api.openai.com/v1/models",
	anthropic: "https://api.anthropic.com/v1/models?limit=1000",
	google: "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000",
} as const;

export type DiscoverableProvider = keyof typeof PROVIDER_DISCOVERY_URLS;
const providers = Object.keys(PROVIDER_DISCOVERY_URLS) as DiscoverableProvider[];
export const DISCOVERABLE_PROVIDERS: readonly DiscoverableProvider[] = providers;

export function discoveryHeaders(provider: DiscoverableProvider, apiKey?: string): Record<string, string> | undefined {
	if (provider !== "openrouter" && !apiKey) return undefined;
	if (provider === "google") return { "x-goog-api-key": apiKey! };
	if (provider === "anthropic") return { "x-api-key": apiKey!, "anthropic-version": "2023-06-01" };
	return apiKey ? { Authorization: `Bearer ${apiKey}` } : {};
}

function record(value: unknown): Record<string, unknown> | undefined {
	return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}
function positive(value: unknown): number | undefined {
	return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : undefined;
}
function price(value: unknown): number | undefined {
	if (typeof value !== "number" && typeof value !== "string") return undefined;
	const parsed = Number(value);
	const perMillion = parsed * 1_000_000;
	return Number.isFinite(parsed) && parsed >= 0 && Number.isFinite(perMillion) ? perMillion : undefined;
}

function explicitlyNonChat(architecture: Record<string, unknown> | undefined): boolean {
	const inputs = architecture?.input_modalities;
	const outputs = architecture?.output_modalities;
	return (Array.isArray(inputs) && !inputs.includes("text")) || (Array.isArray(outputs) && !outputs.includes("text"));
}

/** Discovery supplies metadata, never transport, headers, auth, or compatibility flags. */
export function parseProviderDiscovery(
	provider: DiscoverableProvider,
	payload: unknown,
	bundled: readonly Model<Api>[],
	previous: readonly Model<Api>[] = [],
): Model<Api>[] {
	const body = record(payload);
	const rows = provider === "google" ? body?.models : body?.data;
	if (!Array.isArray(rows)) throw new Error("Invalid provider model list");
	// A partial response must not delete last-good records for other IDs.
	const prior = new Map(previous.map((model) => [model.id, model]));
	const templates = bundled.filter((model) => model.provider === provider);
	const known = new Map(templates.map((model) => [model.id, model]));
	const fallback = templates[0];
	if (!fallback) return [];
	let valid = 0;
	for (const item of rows) {
		const row = record(item);
		const rawId = provider === "google" ? row?.name : row?.id;
		const id = provider === "google" && typeof rawId === "string" ? rawId.replace(/^models\//, "") : rawId;
		if (typeof id !== "string" || !id || id.length > 200 || /[\x00-\x1f]/.test(id)) continue;
		if (
			provider === "google" &&
			(!Array.isArray(row?.supportedGenerationMethods) ||
				!row.supportedGenerationMethods.includes("generateContent"))
		)
			continue;
		const installed = known.get(id);
		const old = prior.get(id);
		const architecture = record(row?.architecture);
		if (provider === "openrouter" && !installed && explicitlyNonChat(architecture)) {
			// Explicit loss of text support revokes an earlier unknown model.
			// Missing modalities are only a partial listing, not a revocation.
			prior.delete(id);
			valid++;
			continue;
		}
		const pricing = record(row?.pricing);
		const input =
			provider === "openrouter" && Array.isArray(architecture?.input_modalities)
				? architecture.input_modalities.filter(
						(value): value is "text" | "image" => value === "text" || value === "image",
					)
				: undefined;
		const contextWindow =
			provider === "openrouter"
				? positive(row?.context_length)
				: provider === "google"
					? positive(row?.inputTokenLimit)
					: undefined;
		const maxTokens =
			provider === "openrouter"
				? positive(record(row?.top_provider)?.max_completion_tokens)
				: provider === "google"
					? positive(row?.outputTokenLimit)
					: undefined;
		const promptPrice = provider === "openrouter" ? price(pricing?.prompt) : undefined;
		const completionPrice = provider === "openrouter" ? price(pricing?.completion) : undefined;
		// ID-only endpoints cannot safely supply context or capabilities for new models.
		// Richer endpoints must provide enough metadata before a new ID is usable.
		if (
			!installed &&
			!old &&
			(provider === "openai" ||
				provider === "anthropic" ||
				provider === "google" ||
				!contextWindow ||
				!maxTokens ||
				(provider === "openrouter" &&
					(promptPrice === undefined ||
						completionPrice === undefined ||
						!input?.includes("text") ||
						!Array.isArray(architecture?.output_modalities) ||
						!architecture.output_modalities.includes("text"))))
		)
			continue;
		valid++;
		const basis = old ?? installed ?? fallback;
		const name = provider === "anthropic" ? row?.display_name : provider === "google" ? row?.displayName : row?.name;
		const model: Model<Api> = {
			...basis,
			id,
			provider,
			api: installed?.api ?? fallback.api,
			baseUrl: installed?.baseUrl ?? fallback.baseUrl,
			headers: installed?.headers,
			compat: installed?.compat,
			reasoning: old?.reasoning ?? installed?.reasoning ?? false,
			thinkingLevelMap: old?.thinkingLevelMap ?? installed?.thinkingLevelMap,
			name: typeof name === "string" && name.length > 0 ? name : (installed?.name ?? id),
			contextWindow: contextWindow ?? basis.contextWindow,
			maxTokens: maxTokens ?? basis.maxTokens,
			input: installed?.input ?? old?.input ?? (input?.length ? input : ["text"]),
			cost:
				provider === "openrouter"
					? {
							input: promptPrice ?? basis.cost.input,
							output: completionPrice ?? basis.cost.output,
							cacheRead:
								price(pricing?.input_cache_read) ?? old?.cost.cacheRead ?? installed?.cost.cacheRead ?? 0,
							cacheWrite:
								price(pricing?.input_cache_write) ?? old?.cost.cacheWrite ?? installed?.cost.cacheWrite ?? 0,
						}
					: (old?.cost ?? installed?.cost ?? { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }),
		};
		prior.set(id, model);
	}
	if (!valid) throw new Error("Provider model list contains no supported models");
	// Explicit installed models stay pinned when a provider omits them.
	for (const model of templates) if (!prior.has(model.id)) prior.set(model.id, model);
	return [...prior.values()];
}

/** Persist only provider metadata. Never cache untrusted transport/auth fields. */
export function mergeProviderDiscoveryPayload(
	provider: DiscoverableProvider,
	payload: unknown,
	previous: unknown,
	bundled: readonly Model<Api>[],
): unknown {
	const key = provider === "google" ? "models" : "data";
	const sanitize = (value: unknown): Record<string, unknown> | undefined => {
		const row = record(value);
		const rawId = provider === "google" ? row?.name : row?.id;
		if (typeof rawId !== "string" || !rawId || rawId.length > 200 || /[\x00-\x1f]/.test(rawId)) return undefined;
		const safe: Record<string, unknown> = provider === "google" ? { name: rawId } : { id: rawId };
		if (provider === "openrouter") {
			if (typeof row?.name === "string") safe.name = row.name;
			if (positive(row?.context_length)) safe.context_length = row?.context_length;
			const max = positive(record(row?.top_provider)?.max_completion_tokens);
			if (max) safe.top_provider = { max_completion_tokens: max };
			const pricing = record(row?.pricing);
			const safePricing: Record<string, string | number> = {};
			for (const field of ["prompt", "completion", "input_cache_read", "input_cache_write"]) {
				const value = pricing?.[field];
				if (price(value) !== undefined) safePricing[field] = value as string | number;
			}
			if (Object.keys(safePricing).length) safe.pricing = safePricing;
			const architecture = record(row?.architecture);
			const inputs = architecture?.input_modalities;
			const outputs = architecture?.output_modalities;
			if (Array.isArray(inputs) || Array.isArray(outputs))
				safe.architecture = {
					...(Array.isArray(inputs)
						? {
								input_modalities: inputs.filter((item) => item === "text" || item === "image"),
							}
						: {}),
					...(Array.isArray(outputs)
						? {
								output_modalities: outputs.filter((item) => item === "text"),
							}
						: {}),
				};
		} else if (provider === "google") {
			if (typeof row?.displayName === "string") safe.displayName = row.displayName;
			const inputLimit = positive(row?.inputTokenLimit);
			const outputLimit = positive(row?.outputTokenLimit);
			if (inputLimit) safe.inputTokenLimit = inputLimit;
			if (outputLimit) safe.outputTokenLimit = outputLimit;
			if (Array.isArray(row?.supportedGenerationMethods))
				safe.supportedGenerationMethods = row.supportedGenerationMethods.filter(
					(item) => item === "generateContent",
				);
		} else if (provider === "anthropic" && typeof row?.display_name === "string") {
			safe.display_name = row.display_name;
		}
		return safe;
	};
	const rows = new Map<string, Record<string, unknown>>();
	for (const [source, replace] of [
		[previous, false],
		[payload, true],
	] as const) {
		const entries = record(source)?.[key];
		if (!Array.isArray(entries)) continue;
		for (const value of entries) {
			const safe = sanitize(value);
			if (!safe) continue;
			const id = String(safe[provider === "google" ? "name" : "id"]);
			const old = rows.get(id);
			if (!old || !replace) {
				rows.set(id, safe);
			} else {
				rows.set(id, {
					...old,
					...safe,
					...(provider === "openrouter"
						? {
								pricing: { ...record(old.pricing), ...record(safe.pricing) },
								top_provider: { ...record(old.top_provider), ...record(safe.top_provider) },
								architecture: { ...record(old.architecture), ...record(safe.architecture) },
							}
						: {}),
				});
			}
		}
	}
	const accepted = new Set(
		parseProviderDiscovery(provider, { [key]: [...rows.values()] }, bundled).map((model) => model.id),
	);
	return {
		[key]: [...rows.values()].filter((row) => {
			const rawId = String(row[provider === "google" ? "name" : "id"]);
			return (
				accepted.has(provider === "google" ? rawId.replace(/^models\//, "") : rawId) ||
				(provider === "openrouter" && explicitlyNonChat(record(row.architecture)))
			);
		}),
	};
}
