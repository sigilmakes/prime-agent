// Explicitly configured public TS APIs only. No package/HOME discovery or network.
import { existsSync, writeFileSync, openSync, closeSync } from "node:fs";
import { join, isAbsolute } from "node:path";
import { pathToFileURL } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = process.env.PA_AUTH_COMPAT_ROOT;
const pkg = process.env.PA_TS_PACKAGE_DIR;
const actor = process.env.PA_AUTH_COMPAT_ACTOR;
const provider = "auth-refresh-compat-dummy";
if (!root || !pkg || !isAbsolute(root) || !isAbsolute(pkg)) {
    throw new Error("Explicit absolute fixture paths required");
}
const watchdog = setTimeout(() => process.exit(124), 35000);
const mark = (name) => writeFileSync(join(root, name), "ready");
async function waitFor(name) {
    const deadline = performance.now() + 25000;
    while (!existsSync(join(root, name))) {
        if (performance.now() >= deadline) throw new Error("Coordination deadline expired");
        await delay(5);
    }
}
const { AuthStorage, FileAuthStorageBackend } = await import(pathToFileURL(
    join(pkg, "packages/coding-agent/dist/core/auth-storage.js")).href);

if (process.env.PA_AUTH_COMPAT_MODE === "hold-lock") {
    const backend = new FileAuthStorageBackend(join(root, "auth.json"));
    await backend.withLockAsync(async () => {
        mark("ts-lock-held");
        const started = performance.now();
        // This timer establishes the stale-age test condition, not readiness by sleep.
        const signal = (async () => {
            while (performance.now() - started < 11500) {
                await delay(Math.max(1, 11500 - (performance.now() - started)));
            }
            mark("ts-old-window");
        })();
        await waitFor("release");
        await signal;
        return { result: undefined };
    });
} else if (process.env.PA_AUTH_COMPAT_MODE === "probe-rust-heartbeat") {
    const backend = new FileAuthStorageBackend(join(root, "auth.json"));
    const started = performance.now();
    // Explicit elapsed test condition: Rust must have held its real refresh
    // lock beyond TS sync's 10s stale threshold. This is not a readiness sleep.
    while (performance.now() - started < 11500) {
        await delay(Math.max(1, 11500 - (performance.now() - started)));
    }
    let entered = false;
    let refused = false;
    try {
        backend.withLock(() => {
            entered = true;
            return { result: undefined };
        });
    } catch {
        refused = true;
    }
    writeFileSync(join(root, "result-ts"), JSON.stringify({ refused, entered }));
    mark("done-ts");
} else if (process.env.PA_AUTH_COMPAT_MODE === "refresh") {
    const { registerOAuthProvider } = await import(pathToFileURL(
        join(pkg, "packages/ai/dist/utils/oauth/index.js")).href);
    registerOAuthProvider({
        id: provider,
        name: "Isolated auth fixture",
        async login() { throw new Error("Login is forbidden in this fixture"); },
        getApiKey(credentials) { return credentials.access; },
        async refreshToken(credentials) {
            if (credentials.refresh !== "fixture-single-use-refresh") {
                throw new Error("Unexpected fake refresh input");
            }
            mark(`entered-${actor}`);
            try {
                closeSync(openSync(join(root, "spent"), "wx", 0o600));
            } catch (error) {
                if (error.code !== "EEXIST") throw error;
                mark(`duplicate-${actor}`);
                throw new Error("Fake single-use refresh already spent");
            }
            await waitFor("release");
            return { ...credentials, access: "fixture-fresh-access",
                refresh: "fixture-rotated-refresh", expires: 4000000000000 };
        },
    });
    const auth = AuthStorage.fromStorage(new FileAuthStorageBackend(join(root, "auth.json")),
        { usePrimeCliConfig: false });
    mark(`ready-${actor}`);
    await waitFor(`go-${actor}`);
    mark(`attempting-${actor}`);
    const value = await auth.getApiKey(provider);
    writeFileSync(join(root, `result-${actor}`), value === "fixture-fresh-access" ? "fresh"
        : value === undefined ? "none" : "other");
    mark(`done-${actor}`);
} else {
    throw new Error("Unknown TS fixture mode");
}
clearTimeout(watchdog);
