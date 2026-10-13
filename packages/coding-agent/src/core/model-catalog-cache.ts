import { Buffer } from "node:buffer";
import { randomUUID } from "node:crypto";
import { closeSync, mkdirSync, openSync, readFileSync, rmSync, statSync } from "node:fs";
import { dirname } from "node:path";
import { performance } from "node:perf_hooks";
import { writeFileAtomicSync } from "../utils/atomic-file.js";

export const CATALOG_REFRESH_INTERVAL_MS = 60 * 60_000;
const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;

export function isCatalogOffline(): boolean {
	return /^(1|true|yes)$/i.test(process.env.PI_OFFLINE ?? "");
}

export class CatalogRequestError extends Error {
	constructor(readonly status: number) {
		super(`Catalog request failed with status ${status}`);
	}
}

interface Snapshot<T> {
	scope: string;
	fetchedAt: number;
	checkedAt?: number;
	revision?: string;
	etag?: string;
	payload: unknown;
	models: T;
}

async function readJsonResponse(response: Response): Promise<unknown> {
	if (!response.ok) throw new CatalogRequestError(response.status);
	if (Number(response.headers.get("content-length")) > MAX_RESPONSE_BYTES) throw new Error("Catalog is too large");
	if (!response.body) throw new Error("Catalog body is empty");
	const reader = response.body.getReader();
	const chunks: Uint8Array[] = [];
	let bytes = 0;
	try {
		while (true) {
			const { done, value } = await reader.read();
			if (done) break;
			bytes += value.byteLength;
			if (bytes > MAX_RESPONSE_BYTES) {
				await reader.cancel();
				throw new Error("Catalog is too large");
			}
			chunks.push(value);
		}
	} finally {
		reader.releaseLock();
	}
	return JSON.parse(Buffer.concat(chunks, bytes).toString("utf8")) as unknown;
}

export interface CatalogCacheStatus {
	state: "idle" | "refreshing" | "fresh" | "stale" | "error";
	source: "none" | "disk" | "legacy" | "network" | "memory";
	fetchedAt?: number;
	checkedAt?: number;
	lastAttempt?: number;
	error?: string;
}

/** Result of this caller's refresh attempt. `models` is last-good data when available. */
export type CatalogRefreshOutcome<T> =
	| { kind: "updated" | "not-modified"; models: T }
	| { kind: "cached" | "offline"; models: T | undefined }
	| { kind: "error"; models: T | undefined; error: string }
	| { kind: "memory-only"; models: T; error: "Catalog cache persistence failed" }
	| { kind: "revoked"; status: 401 | 403; models: undefined }
	| { kind: "cancelled"; models: undefined };

export interface CatalogRefreshOptions {
	force?: boolean;
	headers?: Record<string, string>;
	fetchFn?: typeof fetch;
	isCurrent?: () => boolean;
	signal?: AbortSignal;
	/** Merge validated partial records before persisting the provider-owned payload. */
	mergePayload?: (payload: unknown, previousPayload: unknown | undefined) => unknown;
}

/** One last-good snapshot per source. Changing scope discards the previous account's view. */
export class CatalogCache<T> {
	private snapshot?: Snapshot<T>;
	private scope?: string;
	private readonly pending = new Map<string, Promise<CatalogRefreshOutcome<T>>>();
	private source: CatalogCacheStatus["source"] = "none";
	private error?: string;
	private lastAttempt?: number;
	private generation = 0;

	constructor(
		private readonly url: string,
		private readonly cachePath: string | undefined,
		private readonly parse: (payload: unknown, scope: string) => T,
		/**
		 * Older cache locations for the same source. Read-only fallbacks: a snapshot
		 * found at a legacy path is used as-is (and superseded on the next write to
		 * the primary path), so upgrading never costs a cold fetch and downgrading
		 * keeps working against files the old version can still read.
		 */
		private readonly legacyCachePaths: readonly string[] = [],
	) {}

	get(scope: string): T | undefined {
		if (this.scope === scope) return this.snapshot?.models;
		this.scope = scope;
		this.generation++;
		this.pending.clear();
		this.source = "none";
		this.error = undefined;
		this.snapshot = undefined;
		this.lastAttempt = undefined;
		if (!this.cachePath) return undefined;
		this.snapshot = this.readSnapshot(this.cachePath, scope);
		if (this.snapshot) this.source = "disk";
		else {
			this.snapshot = this.readLegacySnapshot(scope);
			if (this.snapshot) this.source = "legacy";
		}
		return this.snapshot?.models;
	}

	/** Replace the in-memory view with the validated on-disk snapshot, without fetching. */
	reloadFromDisk(scope: string): T | undefined {
		this.scope = undefined;
		return this.get(scope);
	}

	private readSnapshot(path: string, scope: string): Snapshot<T> | undefined {
		try {
			const cached = JSON.parse(readFileSync(path, "utf8")) as Partial<Snapshot<T>> & { url?: unknown };
			const revoked = this.readRevocation(path);
			if (revoked?.scope === scope && revoked.revision === cached.revision && revoked.fetchedAt === cached.fetchedAt)
				return undefined;
			if (
				cached.url !== this.url ||
				cached.scope !== scope ||
				typeof cached.fetchedAt !== "number" ||
				!Number.isFinite(cached.fetchedAt)
			)
				return undefined;
			return {
				scope,
				fetchedAt: cached.fetchedAt,
				revision: typeof cached.revision === "string" ? cached.revision : undefined,
				checkedAt:
					typeof cached.checkedAt === "number" && Number.isFinite(cached.checkedAt)
						? cached.checkedAt
						: cached.fetchedAt,
				etag: typeof cached.etag === "string" ? cached.etag : undefined,
				payload: cached.payload,
				models: this.parse(cached.payload, scope),
			};
		} catch {
			// Invalid or missing disk state falls back to the next candidate.
			return undefined;
		}
	}

	private readRevocation(path: string): { scope: string; revision?: string; fetchedAt: number } | undefined {
		try {
			return JSON.parse(readFileSync(`${path}.revoked`, "utf8")) as {
				scope: string;
				revision?: string;
				fetchedAt: number;
			};
		} catch {
			return undefined;
		}
	}

	private revokeCurrentSnapshot(scope: string): boolean {
		if (!this.cachePath) return true;
		const snapshot = this.readSnapshot(this.cachePath, scope);
		if (!snapshot) return true;
		try {
			writeFileAtomicSync(
				`${this.cachePath}.revoked`,
				JSON.stringify({ scope, revision: snapshot.revision, fetchedAt: snapshot.fetchedAt }),
				{ mode: 0o600 },
			);
			return true;
		} catch {
			return false;
		}
	}

	private readLegacySnapshot(scope: string): Snapshot<T> | undefined {
		for (const path of this.legacyCachePaths) {
			const snapshot = this.readSnapshot(path, scope);
			if (snapshot) return snapshot;
		}
		return undefined;
	}

	private releaseLock(lock: string, descriptor: number): void {
		closeSync(descriptor);
		rmSync(lock, { force: true });
	}

	private async acquireLock(path: string): Promise<number | undefined> {
		const lock = `${path}.lock`;
		try {
			mkdirSync(dirname(path), { recursive: true });
		} catch {
			return undefined;
		}
		const deadline = performance.now() + 1_000;
		do {
			try {
				return openSync(lock, "wx", 0o600);
			} catch (error) {
				if ((error as NodeJS.ErrnoException).code !== "EEXIST") return undefined;
				try {
					if (Date.now() - statSync(lock).mtimeMs > 30_000) rmSync(lock, { force: true });
				} catch {
					/* Another process released the lock. */
				}
				await new Promise<void>((resolve) => setTimeout(resolve, 20));
			}
		} while (performance.now() < deadline);
		return undefined;
	}

	private async invalidateDisk(scope: string): Promise<boolean> {
		if (!this.cachePath) return true;
		const descriptor = await this.acquireLock(this.cachePath);
		if (descriptor === undefined) return false;
		try {
			if (this.readSnapshot(this.cachePath, scope)) rmSync(this.cachePath, { force: true });
			return true;
		} catch {
			return false;
		} finally {
			this.releaseLock(`${this.cachePath}.lock`, descriptor);
		}
	}

	clear(scope: string): void {
		if (this.scope !== scope) return;
		this.generation++;
		this.pending.delete(scope);
		this.snapshot = undefined;
		this.source = "none";
		this.error = undefined;
		this.lastAttempt = undefined;
		// Public clear remains synchronous for current callers. Auth rejection awaits the locked variant.
		if (!this.cachePath) return;
		try {
			const descriptor = openSync(`${this.cachePath}.lock`, "wx", 0o600);
			try {
				if (this.readSnapshot(this.cachePath, scope)) rmSync(this.cachePath, { force: true });
			} finally {
				this.releaseLock(`${this.cachePath}.lock`, descriptor);
			}
		} catch {
			/* A held lock is handled by the async auth invalidation path. */
		}
	}

	status(scope: string): CatalogCacheStatus {
		const snapshot = this.get(scope);
		const checkedAt = this.snapshot?.checkedAt ?? this.snapshot?.fetchedAt;
		const state = this.pending.has(scope)
			? "refreshing"
			: this.error
				? "error"
				: !snapshot
					? "idle"
					: checkedAt !== undefined && Date.now() - checkedAt < CATALOG_REFRESH_INTERVAL_MS
						? "fresh"
						: "stale";
		return {
			state,
			source: this.source,
			fetchedAt: this.snapshot?.fetchedAt,
			checkedAt,
			lastAttempt: this.lastAttempt,
			...(this.error ? { error: this.error } : {}),
		};
	}

	private async persist(
		snapshot: Snapshot<T>,
		startedAt: number,
		previous: Snapshot<T> | undefined,
		isCurrent: () => boolean,
	): Promise<{ snapshot: Snapshot<T>; persisted: boolean }> {
		if (!this.cachePath || !isCurrent()) return { snapshot, persisted: false };
		const path = this.cachePath;
		const descriptor = await this.acquireLock(path);
		if (descriptor === undefined) return { snapshot, persisted: false };
		try {
			if (!isCurrent()) return { snapshot, persisted: false };
			const existing = this.readSnapshot(path, snapshot.scope);
			let diskTime: number | undefined;
			let diskRevision: string | undefined;
			try {
				const raw = JSON.parse(readFileSync(path, "utf8")) as Partial<Snapshot<T>> & { url?: unknown };
				if (raw.url === this.url && typeof raw.fetchedAt === "number") {
					diskTime = typeof raw.checkedAt === "number" ? raw.checkedAt : raw.fetchedAt;
					diskRevision = typeof raw.revision === "string" ? raw.revision : undefined;
				}
			} catch {
				/* No usable disk metadata. */
			}
			if (
				diskTime !== undefined &&
				diskTime >= startedAt &&
				(diskRevision === undefined || diskRevision !== previous?.revision)
			)
				return existing ? { snapshot: existing, persisted: true } : { snapshot, persisted: false };
			const { models: _models, ...stored } = snapshot;
			writeFileAtomicSync(path, JSON.stringify({ url: this.url, ...stored }), { mode: 0o600 });
			return { snapshot, persisted: true };
		} catch {
			/* Keep validated in-memory data on disk failure. */
			return { snapshot, persisted: false };
		} finally {
			this.releaseLock(`${path}.lock`, descriptor);
		}
	}

	refresh(scope: string, options: CatalogRefreshOptions = {}): Promise<T | undefined> {
		// Keep the legacy abort rejection contract while the typed API returns an outcome.
		if (options.signal?.aborted) return Promise.reject(new DOMException("Catalog wait aborted", "AbortError"));
		return this.refreshWithOutcome(scope, options).then((outcome) => {
			if (outcome.kind === "cancelled" && options.signal?.aborted)
				throw new DOMException("Catalog wait aborted", "AbortError");
			return outcome.models;
		});
	}

	refreshWithOutcome(scope: string, options: CatalogRefreshOptions = {}): Promise<CatalogRefreshOutcome<T>> {
		const wait = (promise: Promise<CatalogRefreshOutcome<T>>): Promise<CatalogRefreshOutcome<T>> => {
			const signal = options.signal;
			if (!signal) return promise;
			if (signal.aborted) return Promise.resolve({ kind: "cancelled", models: undefined });
			return new Promise((resolve, reject) => {
				const aborted = () => resolve({ kind: "cancelled", models: undefined });
				signal.addEventListener("abort", aborted, { once: true });
				void promise.then(resolve, reject).finally(() => signal.removeEventListener("abort", aborted));
			});
		};
		if (options.signal?.aborted) return Promise.resolve({ kind: "cancelled", models: undefined });
		const cached = this.get(scope);
		if (isCatalogOffline()) return wait(Promise.resolve({ kind: "offline", models: cached }));
		const active = this.pending.get(scope);
		if (active) return wait(active);
		const checkedAt = this.lastAttempt ?? this.snapshot?.checkedAt ?? this.snapshot?.fetchedAt;
		if (
			!options.force &&
			checkedAt !== undefined &&
			Date.now() >= checkedAt &&
			Date.now() - checkedAt < CATALOG_REFRESH_INTERVAL_MS
		)
			return wait(Promise.resolve({ kind: "cached", models: cached }));
		this.lastAttempt = Date.now();
		const startedAt = this.lastAttempt;
		this.error = undefined;
		const previous = this.snapshot;
		const generation = this.generation;
		const isCurrent = () => this.scope === scope && generation === this.generation && (options.isCurrent?.() ?? true);
		const promise = (async () => {
			try {
				const response = await (options.fetchFn ?? fetch)(this.url, {
					headers: {
						accept: "application/json",
						"cache-control": "no-cache",
						...options.headers,
						...(previous?.etag ? { "If-None-Match": previous.etag } : {}),
					},
					signal: AbortSignal.timeout(5_000),
					redirect: "error",
				});
				const received = response.status === 304 && previous ? previous.payload : await readJsonResponse(response);
				if (response.status !== 304 || !previous) this.parse(received, scope);
				const payload =
					response.status === 304 && previous
						? received
						: (options.mergePayload?.(received, previous?.payload) ?? received);
				if (Buffer.byteLength(JSON.stringify(payload), "utf8") > MAX_RESPONSE_BYTES)
					throw new Error("Merged catalog is too large");
				const models = response.status === 304 && previous ? previous.models : this.parse(payload, scope);
				if (!isCurrent()) return { kind: "cancelled", models: undefined } as const;
				const next: Snapshot<T> = {
					scope,
					fetchedAt: response.status === 304 && previous ? previous.fetchedAt : Date.now(),
					checkedAt: Date.now(),
					revision: randomUUID(),
					etag: response.headers.get("etag") ?? (response.status === 304 ? previous?.etag : undefined),
					payload,
					models,
				};
				const result = await this.persist(next, startedAt, previous, isCurrent);
				if (!isCurrent()) return { kind: "cancelled", models: undefined } as const;
				this.snapshot = result.snapshot;
				this.source = result.snapshot === next ? "network" : "disk";
				if (!result.persisted)
					return {
						kind: "memory-only",
						models: result.snapshot.models,
						error: "Catalog cache persistence failed",
					} as const;
				if (result.snapshot !== next) return { kind: "cached", models: result.snapshot.models } as const;
				return {
					kind: response.status === 304 && previous ? "not-modified" : "updated",
					models: result.snapshot.models,
				} as const;
			} catch (error) {
				if (!isCurrent()) return { kind: "cancelled", models: undefined } as const;
				this.error =
					error instanceof CatalogRequestError
						? `Catalog request failed with status ${error.status}`
						: error instanceof SyntaxError
							? "Invalid catalog JSON"
							: "Catalog refresh failed";
				if (
					scope !== "public" &&
					error instanceof CatalogRequestError &&
					(error.status === 401 || error.status === 403)
				) {
					const revoked = this.revokeCurrentSnapshot(scope);
					this.clear(scope);
					const revokedGeneration = this.generation;
					const deleted = await this.invalidateDisk(scope);
					if (this.scope !== scope || this.generation !== revokedGeneration)
						return { kind: "cancelled", models: undefined } as const;
					this.lastAttempt = startedAt;
					// An unwritable disk cannot inform a fresh process of this rejection.
					// Keep offline private caching, but expose the failed invalidation locally.
					this.error =
						`Catalog request failed with status ${error.status}` +
						(!revoked && !deleted ? "; private catalog disk invalidation failed" : "");
					return { kind: "revoked", status: error.status as 401 | 403, models: undefined } as const;
				}
				return { kind: "error", error: this.error, models: this.snapshot?.models } as const;
			}
		})();
		this.pending.set(scope, promise);
		void promise.finally(() => {
			if (this.pending.get(scope) === promise) this.pending.delete(scope);
		});
		return wait(promise);
	}
}
