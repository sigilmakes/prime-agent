import { type Api, type Model, parseModelCatalog } from "@earendil-works/pi-ai";

export const PROVIDER_MODEL_CATALOG_URL =
	"https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog/main/models/catalog.v1.json";

/** Catalog data can select installed transports, but cannot change where credentials are sent. */
export function parseProviderModelCatalog(payload: unknown, bundledModels: readonly Model<Api>[]): Model<Api>[] {
	const transports = new Map(
		bundledModels.map((model) => [JSON.stringify([model.provider, model.api, model.baseUrl]), model]),
	);
	const exact = new Map(bundledModels.map((model) => [JSON.stringify([model.provider, model.id]), model]));
	const catalog = parseModelCatalog(payload, { skipInvalidModels: true });
	const models: Model<Api>[] = [];
	for (const model of catalog.models) {
		if (model.provider === "prime-inference") continue;
		const transport = transports.get(JSON.stringify([model.provider, model.api, model.baseUrl]));
		if (!transport) continue;
		const template = exact.get(JSON.stringify([model.provider, model.id]));
		models.push({
			...model,
			api: transport.api,
			baseUrl: transport.baseUrl,
			headers: template?.headers ?? transport.headers,
		});
	}
	if (models.length === 0) throw new Error("Catalog has no models supported by this client");
	return models;
}

/** Validate a full trusted refresh before it can replace the built-in provider snapshot. */
export function parseTrustedProviderModelCatalog(payload: unknown, bundledModels: readonly Model<Api>[]): Model<Api>[] {
	// The packaged asset may be partially compatible with this client. A runtime
	// refresh must instead be atomic: even one bad record cannot hide a baseline ID.
	const catalog = parseModelCatalog(payload);
	const baseline = new Map(
		bundledModels
			.filter((model) => model.provider !== "prime-inference")
			.map((model) => [JSON.stringify([model.provider, model.id]), model]),
	);
	const approved = new Set(
		bundledModels
			.filter((model) => model.provider !== "prime-inference")
			.map((model) => JSON.stringify([model.provider, model.api, model.baseUrl])),
	);
	const covered = new Set<string>();
	for (const model of catalog.models) {
		if (model.provider === "prime-inference") continue;
		const id = JSON.stringify([model.provider, model.id]);
		const transport = JSON.stringify([model.provider, model.api, model.baseUrl]);
		if (!approved.has(transport)) throw new Error(`Unsupported provider catalog transport ${id}`);
		const original = baseline.get(id);
		if (original && (model.api !== original.api || model.baseUrl !== original.baseUrl)) {
			throw new Error(`Provider catalog changed installed model transport ${id}`);
		}
		covered.add(id);
	}
	for (const id of baseline.keys()) {
		if (!covered.has(id)) throw new Error(`Provider catalog is missing bundled model ${id}`);
	}
	return parseProviderModelCatalog(payload, bundledModels);
}
