import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, unlinkSync } from "node:fs";
import { createConnection, createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import lockfile from "proper-lockfile";
import { describe, expect, it } from "vitest";
import { DaemonClient } from "../src/modes/daemon/daemon-client.js";
import { DAEMON_PROTOCOL_INFO, DAEMON_SCHEMA_REVISION } from "../src/modes/daemon/daemon-protocol.js";
import {
	cleanupDaemonSocketPath,
	DaemonSocketPathLease,
	defaultDaemonSocketPath,
	getDaemonSocketIdentity,
	normalizeSocketPath,
	prepareDaemonSocketPath,
} from "../src/modes/daemon/daemon-socket.js";

describe("normalizeSocketPath", () => {
	it("normalizes equivalent Unix spellings", () => {
		if (process.platform === "win32") return;
		expect(normalizeSocketPath("/a//b.sock/")).toBe("/a/b.sock");
	});
});

describe("defaultDaemonSocketPath", () => {
	it("uses a fixed Windows named pipe path", () => {
		if (process.platform !== "win32") {
			return;
		}

		expect(defaultDaemonSocketPath()).toBe("\\\\.\\pipe\\prime-agent-daemon");
	});

	it("uses a per-user Unix socket directory", () => {
		if (process.platform === "win32") {
			return;
		}

		const suffix = typeof process.getuid === "function" ? String(process.getuid()) : "user";
		const socketPath = defaultDaemonSocketPath();

		expect(dirname(socketPath)).toBe(join(tmpdir(), `prime-agent-${suffix}`));
		expect(basename(socketPath)).toBe("daemon.sock");
	});

	it("checks a live daemon before acquiring the socket path lock", async () => {
		if (process.platform === "win32") {
			return;
		}

		const dir = mkdtempSync(join(tmpdir(), "pa-socket-live-"));
		const socketPath = join(dir, "daemon.sock");
		let observedLock: boolean | undefined;
		const server = createServer((socket) => {
			observedLock = existsSync(`${socketPath}.lock`);
			socket.destroy();
		});
		try {
			await new Promise<void>((resolve, reject) => {
				server.once("error", reject);
				server.listen(socketPath, resolve);
			});

			await expect(prepareDaemonSocketPath(socketPath)).rejects.toThrow(/socket already in use/i);
			expect(observedLock).toBe(false);
		} finally {
			if (server.listening) {
				await new Promise<void>((resolve) => server.close(() => resolve()));
			}
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it("does not wait on a stale lock when no socket path exists", async () => {
		if (process.platform === "win32") {
			return;
		}

		const dir = mkdtempSync(join(tmpdir(), "pa-socket-stale-lock-"));
		const socketPath = join(dir, "daemon.sock");
		mkdirSync(`${socketPath}.lock`);
		let prepared = false;
		try {
			await Promise.race([
				prepareDaemonSocketPath(socketPath).then(() => {
					prepared = true;
				}),
				new Promise<void>((resolve) => setTimeout(resolve, 250)),
			]);
			expect(prepared).toBe(true);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it("does not unlink a replacement daemon's socket during delayed cleanup", async () => {
		if (process.platform === "win32") {
			return;
		}

		const dir = mkdtempSync(join(tmpdir(), "pa-socket-ownership-"));
		const socketPath = join(dir, "daemon.sock");
		const oldServer = createServer();
		const replacementServer = createServer();
		try {
			await new Promise<void>((resolve, reject) => {
				oldServer.once("error", reject);
				oldServer.listen(socketPath, resolve);
			});
			const oldIdentity = getDaemonSocketIdentity(socketPath);
			if (!oldIdentity) {
				throw new Error("Expected a Unix daemon socket identity");
			}

			unlinkSync(socketPath);
			await new Promise<void>((resolve, reject) => {
				replacementServer.once("error", reject);
				replacementServer.listen(socketPath, resolve);
			});

			cleanupDaemonSocketPath(socketPath, oldIdentity);

			await expect(
				new Promise<void>((resolve, reject) => {
					const client = createConnection(socketPath);
					client.once("connect", () => {
						client.destroy();
						resolve();
					});
					client.once("error", reject);
				}),
			).resolves.toBeUndefined();
		} finally {
			await Promise.all(
				[oldServer, replacementServer].map(
					(server) =>
						new Promise<void>((resolve) => {
							server.close(() => resolve());
						}),
				),
			);
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it("does not unlink a socket while another daemon owns the path lock", async () => {
		if (process.platform === "win32") {
			return;
		}

		const dir = mkdtempSync(join(tmpdir(), "pa-socket-lock-"));
		const socketPath = join(dir, "daemon.sock");
		const server = createServer();
		let releaseLock: (() => Promise<void>) | undefined;
		try {
			await new Promise<void>((resolve, reject) => {
				server.once("error", reject);
				server.listen(socketPath, resolve);
			});
			const identity = getDaemonSocketIdentity(socketPath);
			if (!identity) {
				throw new Error("Expected a Unix daemon socket identity");
			}
			releaseLock = await lockfile.lock(socketPath, { realpath: false });

			cleanupDaemonSocketPath(socketPath, identity);

			await expect(
				new Promise<void>((resolve, reject) => {
					const client = createConnection(socketPath);
					client.once("connect", () => {
						client.destroy();
						resolve();
					});
					client.once("error", reject);
				}),
			).resolves.toBeUndefined();
		} finally {
			await releaseLock?.();
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it("does not remove a replacement socket that appears while stale cleanup is pending", async () => {
		if (process.platform === "win32") {
			return;
		}

		const dir = mkdtempSync(join(tmpdir(), "pa-socket-startup-"));
		const socketPath = join(dir, "daemon.sock");
		const staleOwner = spawn(
			process.execPath,
			[
				"-e",
				"const { createServer } = require('node:net'); const server = createServer(); server.listen(process.argv[1], () => process.stdout.write('ready'));",
				socketPath,
			],
			{ stdio: ["ignore", "pipe", "ignore"] },
		);
		const replacementServer = createServer();
		let replacementTimer: ReturnType<typeof setTimeout> | undefined;
		try {
			await new Promise<void>((resolve, reject) => {
				staleOwner.stdout?.once("data", () => resolve());
				staleOwner.once("error", reject);
				staleOwner.once("exit", (code) => {
					if (code !== null && code !== 0) {
						reject(new Error(`Stale socket owner exited before listening: ${code}`));
					}
				});
			});
			staleOwner.kill("SIGKILL");
			await new Promise<void>((resolve) => staleOwner.once("close", () => resolve()));
			expect(existsSync(socketPath)).toBe(true);

			const replacementListening = new Promise<void>((resolve, reject) => {
				replacementTimer = setTimeout(() => {
					try {
						if (existsSync(socketPath)) {
							unlinkSync(socketPath);
						}
						replacementServer.once("error", reject);
						replacementServer.listen(socketPath, resolve);
					} catch (error) {
						reject(error);
					}
				}, 50);
			});

			await expect(prepareDaemonSocketPath(socketPath)).rejects.toThrow(
				/socket (already in use|changed ownership)/i,
			);
			await replacementListening;
			await expect(
				new Promise<void>((resolve, reject) => {
					const client = createConnection(socketPath);
					client.once("connect", () => {
						client.destroy();
						resolve();
					});
					client.once("error", reject);
				}),
			).resolves.toBeUndefined();
		} finally {
			if (replacementTimer) {
				clearTimeout(replacementTimer);
			}
			if (staleOwner.exitCode === null && staleOwner.signalCode === null) {
				staleOwner.kill("SIGKILL");
			}
			if (replacementServer.listening) {
				await new Promise<void>((resolve) => replacementServer.close(() => resolve()));
			}
			rmSync(dir, { recursive: true, force: true });
		}
	});
});

describe.skipIf(process.platform === "win32")("DaemonSocketPathLease compromise hardening", () => {
	it("records compromise without rethrowing listener failures", () => {
		const lease = new DaemonSocketPathLease("/tmp/test.sock", () => Promise.resolve());
		const observed: Error[] = [];
		lease.onCompromised(() => {
			throw new Error("listener failed");
		});
		lease.onCompromised((error) => observed.push(error));

		expect(() => lease.recordCompromise(new Error("lock update failed"))).not.toThrow();
		expect(lease.compromise?.message).toBe("lock update failed");
		expect(observed).toHaveLength(1);
	});

	it("does not unlink a successor socket after the old lease is compromised", async () => {
		const dir = mkdtempSync(join(tmpdir(), "pa-socket-compromise-"));
		const socketPath = join(dir, "daemon.sock");
		const server = createServer();
		try {
			await new Promise<void>((resolve) => server.listen(socketPath, resolve));
			const identity = getDaemonSocketIdentity(socketPath);
			const lease = new DaemonSocketPathLease(socketPath, () => Promise.resolve());
			lease.recordCompromise(new Error("lock stolen"));

			cleanupDaemonSocketPath(socketPath, identity, lease);
			expect(existsSync(socketPath)).toBe(true);
		} finally {
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(dir, { recursive: true, force: true });
		}
	});
});

/** A real JSONL peer exercises the pre-write gate, not the mocked transport tests. */
describe("daemon model refresh compatibility on a real socket", () => {
	it("keeps a legacy bare get_state frame working with a new daemon hello", async () => {
		const dir = mkdtempSync(join(tmpdir(), "pa-refresh-legacy-"));
		const socketPath =
			process.platform === "win32"
				? String.raw`\\.\pipe\pa-refresh-legacy-${basename(dir)}`
				: join(dir, "daemon.sock");
		const connections = new Set<Socket>();
		const server = createServer((socket) => {
			connections.add(socket);
			socket.on("close", () => connections.delete(socket));
			socket.write(
				`${JSON.stringify({
					type: "daemon_hello",
					socketPath,
					protocol: DAEMON_PROTOCOL_INFO,
					schemaRevision: DAEMON_SCHEMA_REVISION,
					serverCapabilities: ["model_refresh"],
				})}\n`,
			);
			socket.on("data", (data: Buffer) => {
				const command = JSON.parse(data.toString("utf8")) as { id: string; type: string };
				socket.write(
					`${JSON.stringify({
						type: "response",
						id: command.id,
						command: command.type,
						success: true,
						data: { state: "idle" },
					})}\n`,
				);
			});
		});
		let legacy: Socket | undefined;
		try {
			await new Promise<void>((resolve, reject) => {
				server.once("error", reject);
				server.listen(socketPath, resolve);
			});
			legacy = createConnection(socketPath);
			await new Promise<void>((resolve, reject) => {
				legacy!.once("connect", resolve);
				legacy!.once("error", reject);
			});
			const response = new Promise<{ command: string; success: boolean }>((resolve, reject) => {
				let buffer = "";
				legacy!.on("data", (data: Buffer) => {
					buffer += data.toString("utf8");
					for (let end = buffer.indexOf("\n"); end >= 0; end = buffer.indexOf("\n")) {
						const line = buffer.slice(0, end);
						buffer = buffer.slice(end + 1);
						const message = JSON.parse(line) as { type: string; command: string; success: boolean };
						if (message.type === "response") resolve(message);
					}
				});
				legacy!.once("error", reject);
			});
			legacy.write(`${JSON.stringify({ type: "get_state", id: "legacy-1", activeSessionId: "session-1" })}\n`);
			await expect(response).resolves.toMatchObject({ command: "get_state", success: true });
		} finally {
			legacy?.destroy();
			for (const socket of connections) socket.destroy();
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it.each([
		[DAEMON_SCHEMA_REVISION - 1, false],
		[DAEMON_SCHEMA_REVISION, true],
	] as const)("handles schema %s without writing unsupported commands", async (schemaRevision, supportsRefresh) => {
		const dir = mkdtempSync(join(tmpdir(), "pa-refresh-wire-"));
		const socketPath =
			process.platform === "win32"
				? String.raw`\\.\pipe\pa-refresh-wire-${basename(dir)}`
				: join(dir, "daemon.sock");
		const frames: Record<string, unknown>[] = [];
		const connections = new Set<Socket>();
		const report = { startedAt: 1, finishedAt: 2, sources: [] };
		const server = createServer((socket) => {
			connections.add(socket);
			socket.on("close", () => connections.delete(socket));
			socket.write(
				`${JSON.stringify({
					type: "daemon_hello",
					socketPath,
					protocol: DAEMON_PROTOCOL_INFO,
					schemaRevision,
					serverCapabilities: ["model_refresh"],
				})}\n`,
			);
			let buffer = "";
			socket.on("data", (chunk: Buffer) => {
				buffer += chunk.toString("utf8");
				for (let end = buffer.indexOf("\n"); end >= 0; end = buffer.indexOf("\n")) {
					const line = buffer.slice(0, end);
					buffer = buffer.slice(end + 1);
					const frame = JSON.parse(line) as Record<string, unknown>;
					frames.push(frame);
					const command = (frame.command ?? frame) as { type: string };
					if (command.type === "ack_result") continue;
					socket.write(
						`${JSON.stringify({
							type: "response",
							id: frame.id,
							command: command.type,
							success: true,
							data: command.type === "refresh_models" ? report : { state: "idle" },
						})}\n`,
					);
				}
			});
		});
		let client: DaemonClient | undefined;
		try {
			await new Promise<void>((resolve, reject) => {
				server.once("error", reject);
				server.listen(socketPath, resolve);
			});
			client = new DaemonClient(socketPath);
			await client.connect();
			await client.waitForHello();
			if (supportsRefresh) {
				await expect(
					client.request({ type: "refresh_models", activeSessionId: "session-1" }, 3000),
				).resolves.toMatchObject({ success: true, data: report });
			} else {
				await expect(
					client.request({ type: "refresh_models", activeSessionId: "session-1" }, 3000),
				).rejects.toThrow("does not support model_refresh");
				expect(frames).toEqual([]);
			}
			await expect(client.request({ type: "get_state", activeSessionId: "session-1" }, 3000)).resolves.toMatchObject(
				{ success: true, command: "get_state" },
			);
			expect(
				frames.filter((frame) => (frame.command as { type?: string } | undefined)?.type === "refresh_models"),
			).toHaveLength(supportsRefresh ? 1 : 0);
		} finally {
			client?.close();
			for (const socket of connections) socket.destroy();
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(dir, { recursive: true, force: true });
		}
	});
});
