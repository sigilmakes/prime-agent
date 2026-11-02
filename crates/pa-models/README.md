# pa-models

The live model catalog subsystem: fetch layer, no-cold-start chain, strict
schemas, transport pinning, and the credentialed Prime Inference catalog.
Port of the TS catalog client (branch `feat/catalog-client`:
`model-catalog-cache.ts`, `model-catalog.ts`, `model-compat-schema.ts`,
`provider-model-catalog.ts`, `bundled-model-catalog.ts`,
`prime-inference-model-catalog.ts`).

## Scope

- Fetch layer: unauthenticated GET on the two catalog URLs; 5 s timeout,
  8 MiB cap (content-length AND streaming), redirects refused, ETag
  `If-None-Match` with 304 snapshot reuse; `PI_OFFLINE` disables the network.
- No-cold-start chain: validated last-good disk cache -> build-time bundled
  snapshot -> compiled fallback (42 transport tuples + 110 offline Prime
  Inference entries).
- Strict schemas: serde `deny_unknown_fields`, version gates (models
  `schemaVersion == 1`), skip-invalid for remote model refresh, compat
  validation per `api`. Unsupported future versions keep last-good
  silently, forever.
- Transport pinning: every remote model must match a compiled
  `(provider, api, baseUrl)` tuple; catalog data never carries request
  headers.
- Prime Inference: live credentialed fetch, scope-keyed disk cache
  (HMAC-SHA256 of key over team), 401/403 clears only that scope.
- Refresh: hourly + startup + picker open + auth change; fire-and-forget;
  coalesced per source; a mid-session refresh never retargets the active
  model.

## Non-goals

- The plugins/MCP catalog serde + resolution (the plugins lane; this crate
  hands it the fetcher, the generic `CatalogCache`, and the bundled asset
  bytes).
- The build-time asset generator + release packer (the build lane).
- Session auth storage and `models.json` parsing (`pa-core`). The caller
  supplies trusted seed models and explicit discovery descriptors. The catalog
  composes those seeds with the baseline before projecting optional metadata.
  Discovery cannot alter transport, endpoint, or headers. Unknown IDs require
  explicit defaults; missing or malformed metadata preserves local definitions.
- The catalog repo's sync/export tooling (lives with the data).

## Public API

- `ModelCatalog::snapshot(&CatalogInputs)` reads a local no-cold-start snapshot.
- `ModelCatalog::refresh(&CatalogInputs, &RefreshRequest)` publishes selected
  sources and returns a snapshot plus source reports.
- `ModelCatalog::adopt(&CatalogInputs, SnapshotRequirement)` adopts validated
  disk publications without network work.
- `CatalogInputs` contains current trusted local seed models and authorized
  `ResolvedSource` descriptors. Each source names its format, exact endpoint,
  complete request headers, optional explicit defaults, and publication guard.
  These inputs come from pa-core; pa-models never opens auth or config files.
- Discovery formats cover OpenAI, OpenRouter, Anthropic, Google, Codex, Ollama,
  and separate public/private Prime Inference sources. Private authorization
  appears separately in `CatalogSnapshot::authorized_private_prime_models`;
  baseline definitions alone never grant access.
- `cache::CatalogCache<T>` (generic last-good cache; the plugins lane
  instantiates it with its own fail-closed parser),
  `cache::{RefreshOptions, PUBLIC_SCOPE}`
- `fetch::{CatalogFetcher, FetchOutcome, FetchError, MODEL_CATALOG_URL,
  MCP_SERVICE_CATALOG_URL}`
- `schema::{parse_model_catalog, InvalidEntries, ModelCatalogV1}`
- `compat::is_model_compat`
- `pinning::{parse_provider_model_catalog, pin_catalog_models, PinnedTemplates}`
- `transports::{compiled_models, compiled_transport_tuples,
  prime_inference_offline_entries}`
- `bundled::{BundledAssets, load_bundled_models, PACKAGED_MODEL_CATALOG_FILE,
  PACKAGED_MCP_CATALOG_FILE}`
- `prime_inference::{build_prime_inference_models, merge_prime_inference_models,
  parse_prime_inference_model_catalog, is_private_prime_inference_model_id,
  scope_key, PRIME_INFERENCE_BASE_URL}`
- `offline::is_catalog_offline`

## Dependencies (direction compliance)

Depends on `pa-ai` (compiled transport templates: `models_generated`) and
`pa-types` (shared `Model`). Nothing else in the workspace; no crate below
`pa-ai` is touched. Consumers live at or above `pa-agent` (registry wiring
in `pa-core`). The plugins lane must not place its catalog code in `pa-ai`
(pa-models already depends on it).

## Discovery admission and cache privacy

Unknown IDs require either explicit configured full defaults or complete provider
metadata, including input, output, cache-read, and cache-write prices. Missing
rates do not mean zero. This conservative limit may exclude models whose API
omits cache prices. Known IDs still accept partial metadata updates.

Discovery snapshots and revocations store only an opaque endpoint identity.
The actual endpoint, including its query, is used only by the fetch closure.
Discovery therefore does not reuse ETags; the curated/default caches still do.
