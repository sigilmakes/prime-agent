import { closeSync, mkdirSync, mkdtempSync, openSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { CatalogCache } from "../src/core/model-catalog-cache.js";

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});
function cachePath(): string {
	const dir = mkdtempSync(join(tmpdir(), "catalog-cache-test-"));
	dirs.push(dir);
	return join(dir, "catalog.json");
}
function response(value: string): Response {
	return new Response(JSON.stringify({ value }));
}
function deferred<T>() {
	let resolve!: (value: T) => void;
	const promise = new Promise<T>((done) => {
		resolve = done;
	});
	return { promise, resolve };
}
const parse = (payload: unknown): string => {
	if (!payload || typeof payload !== "object" || !("value" in payload) || typeof payload.value !== "string")
		throw new Error("invalid catalog");
	return payload.value;
};

describe("CatalogCache shared foundation", () => {
	it("coalesces a scoped refresh without letting an aborted caller cancel another", async () => {
		const cache = new CatalogCache("https://example.test/catalog", cachePath(), parse);
		const gate = deferred<Response>();
		let calls = 0;
		const fetchFn = (async () => {
			calls++;
			return gate.promise;
		}) as typeof fetch;
		const controller = new AbortController();
		const cancelled = cache.refresh("public", { force: true, fetchFn, signal: controller.signal });
		const retained = cache.refresh("public", { force: true, fetchFn });
		controller.abort();
		await expect(cancelled).rejects.toMatchObject({ name: "AbortError" });
		gate.resolve(response("good"));
		await expect(retained).resolves.toBe("good");
		expect(calls).toBe(1);
	});

	it("does not replace newer disk state with a slower process result", async () => {
		const path = cachePath();
		const url = "https://example.test/catalog";
		const slow = new CatalogCache(url, path, parse);
		const fast = new CatalogCache(url, path, parse);
		const gate = deferred<Response>();
		const pending = slow.refresh("public", { force: true, fetchFn: (async () => gate.promise) as typeof fetch });
		await fast.refresh("public", { force: true, fetchFn: (async () => response("newer")) as typeof fetch });
		gate.resolve(response("older"));
		await pending;
		expect(new CatalogCache(url, path, parse).get("public")).toBe("newer");
	});

	it("does not let an old scope overwrite a newly selected account", async () => {
		const path = cachePath();
		const cache = new CatalogCache("https://example.test/catalog", path, parse);
		const gate = deferred<Response>();
		const old = cache.refresh("account-old", { force: true, fetchFn: (async () => gate.promise) as typeof fetch });
		await expect(
			cache.refresh("account-new", { force: true, fetchFn: (async () => response("new")) as typeof fetch }),
		).resolves.toBe("new");
		gate.resolve(response("old"));
		await expect(old).resolves.toBeUndefined();
		expect(cache.get("account-new")).toBe("new");
		expect(new CatalogCache("https://example.test/catalog", path, parse).get("account-new")).toBe("new");
		cache.clear("account-old");
		expect(new CatalogCache("https://example.test/catalog", path, parse).get("account-new")).toBe("new");
	});

	it("reports stale last-good data and error after a failed refresh", async () => {
		const cache = new CatalogCache("https://example.test/catalog", cachePath(), parse);
		await cache.refresh("public", { force: true, fetchFn: (async () => response("good")) as typeof fetch });
		await expect(
			cache.refresh("public", {
				force: true,
				fetchFn: (async () => {
					throw new Error("offline");
				}) as typeof fetch,
			}),
		).resolves.toBe("good");
		expect(cache.status("public")).toMatchObject({
			state: "error",
			source: "network",
			error: "Catalog refresh failed",
		});
	});

	it("does not persist a refresh after switching scope while the lock is held", async () => {
		const path = cachePath();
		const cache = new CatalogCache("https://example.test/catalog", path, parse);
		const descriptor = openSync(`${path}.lock`, "wx");
		try {
			const pending = cache.refresh("old", { force: true, fetchFn: (async () => response("old")) as typeof fetch });
			await Promise.resolve();
			cache.get("new");
			closeSync(descriptor);
			rmSync(`${path}.lock`);
			await expect(pending).resolves.toBeUndefined();
			expect(new CatalogCache("https://example.test/catalog", path, parse).get("old")).toBeUndefined();
		} finally {
			try {
				closeSync(descriptor);
			} catch {
				/* Already closed. */
			}
			rmSync(`${path}.lock`, { force: true });
		}
	});

	it("invalidates in-flight refresh on explicit clear", async () => {
		const path = cachePath();
		const cache = new CatalogCache("https://example.test/catalog", path, parse);
		const gate = deferred<Response>();
		const pending = cache.refresh("public", { force: true, fetchFn: (async () => gate.promise) as typeof fetch });
		cache.clear("public");
		gate.resolve(response("late"));
		await expect(pending).resolves.toBeUndefined();
		expect(cache.get("public")).toBeUndefined();
		expect(new CatalogCache("https://example.test/catalog", path, parse).get("public")).toBeUndefined();
	});

	it("revokes a private snapshot despite another process holding the lock", async () => {
		const path = cachePath();
		const url = "https://example.test/catalog";
		const cache = new CatalogCache(url, path, parse);
		await cache.refresh("private", { force: true, fetchFn: (async () => response("good")) as typeof fetch });
		const descriptor = openSync(`${path}.lock`, "wx");
		try {
			const pending = cache.refresh("private", {
				force: true,
				fetchFn: (async () => new Response("", { status: 401 })) as typeof fetch,
			});
			await Promise.resolve();
			// The revocation marker must hide disk data even before the held lock is released.
			await new Promise<void>((resolve) => setImmediate(resolve));
			expect(new CatalogCache(url, path, parse).get("private")).toBeUndefined();
			closeSync(descriptor);
			rmSync(`${path}.lock`);
			await expect(pending).resolves.toBeUndefined();
			expect(cache.status("private")).toMatchObject({
				state: "error",
				error: "Catalog request failed with status 401",
				lastAttempt: expect.any(Number),
			});
			expect(new CatalogCache(url, path, parse).get("private")).toBeUndefined();
		} finally {
			try {
				closeSync(descriptor);
			} catch {
				/* Already closed. */
			}
			rmSync(`${path}.lock`, { force: true });
		}
	});

	it("reports private 401 invalidation status while refusing the in-memory stale view", async () => {
		const path = cachePath();
		const url = "https://example.test/catalog";
		const cache = new CatalogCache(url, path, parse);
		await cache.refresh("private", { force: true, fetchFn: (async () => response("good")) as typeof fetch });
		const locked = openSync(`${path}.lock`, "wx");
		mkdirSync(`${path}.revoked`); // Force marker write failure without relying on filesystem permissions.
		try {
			await expect(
				cache.refresh("private", {
					force: true,
					fetchFn: (async () => new Response("", { status: 401 })) as typeof fetch,
				}),
			).resolves.toBeUndefined();
			expect(cache.get("private")).toBeUndefined();
			expect(cache.status("private")).toMatchObject({
				state: "error",
				error: "Catalog request failed with status 401; private catalog disk invalidation failed",
				lastAttempt: expect.any(Number),
			});
		} finally {
			closeSync(locked);
			rmSync(`${path}.lock`, { force: true });
			rmSync(`${path}.revoked`, { recursive: true });
		}
	});

	it("preserves newer disk data across instances when both checks share one millisecond", async () => {
		const path = cachePath();
		const url = "https://example.test/catalog";
		vi.spyOn(Date, "now").mockReturnValue(10_000);
		try {
			const slow = new CatalogCache(url, path, parse);
			const fast = new CatalogCache(url, path, parse);
			const gate = deferred<Response>();
			const pending = slow.refresh("public", { force: true, fetchFn: (async () => gate.promise) as typeof fetch });
			await fast.refresh("public", { force: true, fetchFn: (async () => response("new")) as typeof fetch });
			gate.resolve(response("old"));
			await pending;
			expect(new CatalogCache(url, path, parse).get("public")).toBe("new");
		} finally {
			vi.restoreAllMocks();
		}
	});

	it("validates and persists merged payload for an offline process", async () => {
		const path = cachePath();
		const url = "https://example.test/catalog";
		const parseIds = (payload: unknown): string[] => {
			if (
				!payload ||
				typeof payload !== "object" ||
				!("ids" in payload) ||
				!Array.isArray(payload.ids) ||
				!payload.ids.every((id) => typeof id === "string")
			)
				throw new Error("invalid model list");
			return payload.ids;
		};
		const cache = new CatalogCache(url, path, parseIds);
		const fetchFn = (ids: string[]) => (async () => new Response(JSON.stringify({ ids }))) as typeof fetch;
		const mergePayload = (payload: unknown, previous: unknown) => ({
			ids: [
				...new Set([
					...((previous as { ids?: string[] } | undefined)?.ids ?? []),
					...(payload as { ids: string[] }).ids,
				]),
			],
		});
		await cache.refresh("public", { force: true, fetchFn: fetchFn(["a"]), mergePayload });
		await cache.refresh("public", { force: true, fetchFn: fetchFn(["b"]), mergePayload });
		expect(cache.get("public")).toEqual(["a", "b"]);
		expect(new CatalogCache(url, path, parseIds).get("public")).toEqual(["a", "b"]);
		await cache.refresh("public", {
			force: true,
			fetchFn: fetchFn(["c"]),
			mergePayload: () => ({ ids: [null] }),
		});
		expect(new CatalogCache(url, path, parseIds).get("public")).toEqual(["a", "b"]);
	});

	it("keeps fetchedAt after a local-server 304 and records a later checkedAt", async () => {
		const server = createServer((request, reply) => {
			if (request.headers["if-none-match"] === '"v1"') {
				reply.writeHead(304).end();
			} else {
				reply
					.writeHead(200, { "content-type": "application/json", etag: '"v1"' })
					.end(JSON.stringify({ value: "good" }));
			}
		});
		try {
			await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
			const address = server.address();
			if (!address || typeof address === "string") throw new Error("missing local server port");
			const cache = new CatalogCache(`http://127.0.0.1:${address.port}/catalog`, cachePath(), parse);
			await expect(cache.refresh("public", { force: true })).resolves.toBe("good");
			const first = cache.status("public");
			await expect(cache.refresh("public", { force: true })).resolves.toBe("good");
			const second = cache.status("public");
			expect(second).toMatchObject({ state: "fresh", source: "network", fetchedAt: first.fetchedAt });
			expect(second.checkedAt).toBeGreaterThanOrEqual(first.checkedAt ?? 0);
		} finally {
			server.closeAllConnections();
			await new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
		}
	});
});
