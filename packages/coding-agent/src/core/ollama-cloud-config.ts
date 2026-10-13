import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** Values allowed in Ollama Cloud's OpenAI-compatible request body. */
export interface OllamaCloudInferenceParams {
	temperature?: number;
	top_p?: number;
	top_k?: number;
	min_p?: number;
	typical_p?: number;
	tfs_z?: number;
	repeat_penalty?: number;
	repeat_last_n?: number;
	seed?: number;
	num_predict?: number;
	num_ctx?: number;
	num_gpu?: number;
	num_thread?: number;
	mirostat?: number;
	mirostat_tau?: number;
	mirostat_eta?: number;
	frequency_penalty?: number;
	presence_penalty?: number;
	max_tokens?: number;
	stop?: string | string[];
}

export interface OllamaCloudModelOverrides {
	contextWindow?: number;
	maxTokens?: number;
}

export interface OllamaCloudConfig {
	inferenceParams?: OllamaCloudInferenceParams;
	models?: Record<string, OllamaCloudInferenceParams>;
	modelOverrides?: Record<string, OllamaCloudModelOverrides>;
}

const NUMERIC_INFERENCE_KEYS = [
	"temperature",
	"top_p",
	"top_k",
	"min_p",
	"typical_p",
	"tfs_z",
	"repeat_penalty",
	"repeat_last_n",
	"seed",
	"num_predict",
	"num_ctx",
	"num_gpu",
	"num_thread",
	"mirostat",
	"mirostat_tau",
	"mirostat_eta",
	"frequency_penalty",
	"presence_penalty",
	"max_tokens",
] as const satisfies readonly (keyof OllamaCloudInferenceParams)[];

function isRecord(value: unknown): value is Record<string, unknown> {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
	return typeof value === "number" && Number.isFinite(value);
}

function sanitizeInferenceParams(raw: Record<string, unknown>): OllamaCloudInferenceParams {
	const result: OllamaCloudInferenceParams = {};
	for (const key of NUMERIC_INFERENCE_KEYS) {
		if (isFiniteNumber(raw[key])) result[key] = raw[key] as number;
	}
	if (
		typeof raw.stop === "string" ||
		(Array.isArray(raw.stop) && raw.stop.every((item) => typeof item === "string"))
	) {
		result.stop = raw.stop;
	}
	return result;
}

function sanitizePerModel<T extends object>(
	raw: unknown,
	sanitize: (value: Record<string, unknown>) => T,
): Record<string, T> | undefined {
	if (!isRecord(raw)) return undefined;
	const entries = Object.entries(raw)
		.filter(([, value]) => isRecord(value))
		.map(([id, value]) => [id, sanitize(value as Record<string, unknown>)] as const)
		.filter(([, value]) => Object.keys(value).length > 0);
	return entries.length ? Object.fromEntries(entries) : undefined;
}

function sanitizeConfig(raw: Record<string, unknown>): OllamaCloudConfig {
	const config: OllamaCloudConfig = {};
	if (isRecord(raw.inferenceParams)) {
		const params = sanitizeInferenceParams(raw.inferenceParams);
		if (Object.keys(params).length) config.inferenceParams = params;
	}
	config.models = sanitizePerModel(raw.models, sanitizeInferenceParams);
	config.modelOverrides = sanitizePerModel(raw.modelOverrides, (value) => {
		const result: OllamaCloudModelOverrides = {};
		if (
			typeof value.contextWindow === "number" &&
			Number.isSafeInteger(value.contextWindow) &&
			value.contextWindow > 0
		)
			result.contextWindow = value.contextWindow;
		if (typeof value.maxTokens === "number" && Number.isSafeInteger(value.maxTokens) && value.maxTokens > 0)
			result.maxTokens = value.maxTokens;
		return result;
	});
	return config;
}

function readConfig(path: string): OllamaCloudConfig {
	if (!existsSync(path)) return {};
	try {
		const parsed: unknown = JSON.parse(readFileSync(path, "utf-8"));
		return isRecord(parsed) ? sanitizeConfig(parsed) : {};
	} catch (error) {
		console.error(`[prime-ollama-cloud] Failed to load config from ${path}: ${error}`);
		return {};
	}
}

function mergePerModel<T extends object>(
	base?: Record<string, T>,
	override?: Record<string, T>,
): Record<string, T> | undefined {
	if (!base && !override) return undefined;
	const ids = new Set([...Object.keys(base ?? {}), ...Object.keys(override ?? {})]);
	return Object.fromEntries([...ids].map((id) => [id, { ...base?.[id], ...override?.[id] }])) as Record<string, T>;
}

/** Load global then project config. Model entries merge by model ID and parameter key. */
export function loadOllamaCloudConfig(cwd: string, agentDir: string): OllamaCloudConfig {
	const globalConfig = readConfig(join(agentDir, "ollama-cloud.json"));
	const projectConfig = readConfig(join(cwd, ".prime", "agent", "ollama-cloud.json"));
	const inferenceParams = { ...globalConfig.inferenceParams, ...projectConfig.inferenceParams };
	return {
		inferenceParams: Object.keys(inferenceParams).length ? inferenceParams : undefined,
		models: mergePerModel(globalConfig.models, projectConfig.models),
		modelOverrides: mergePerModel(globalConfig.modelOverrides, projectConfig.modelOverrides),
	};
}

/** Per-model inference parameters take precedence over shared defaults. */
export function resolveOllamaCloudInferenceParams(
	config: OllamaCloudConfig,
	modelId: string,
): OllamaCloudInferenceParams {
	return { ...config.inferenceParams, ...config.models?.[modelId] };
}

/** Return a new outgoing body, or undefined when the request must not be changed. */
export function overlayOllamaCloudRequest(
	payload: unknown,
	config: OllamaCloudConfig,
	modelId: string,
): Record<string, unknown> | undefined {
	if (!isRecord(payload)) return undefined;
	const params = resolveOllamaCloudInferenceParams(config, modelId);
	if (!Object.keys(params).length) return undefined;
	return { ...payload, ...params };
}

/** Apply registration metadata overrides without mutating catalog models. */
export function applyOllamaCloudModelOverrides<T extends { id: string; contextWindow: number; maxTokens: number }>(
	models: T[],
	overrides?: Record<string, OllamaCloudModelOverrides>,
): T[] {
	if (!overrides) return models;
	return models.map((model) => {
		const override = overrides[model.id];
		return override
			? {
					...model,
					contextWindow: override.contextWindow ?? model.contextWindow,
					maxTokens: override.maxTokens ?? model.maxTokens,
				}
			: model;
	});
}
