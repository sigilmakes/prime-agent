import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Model } from "@earendil-works/pi-ai";
import { afterEach, expect, test } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { createAgentSession } from "../src/core/sdk.js";
import { SessionManager } from "../src/core/session-manager.js";

const roots: string[] = [];
const servers: Server[] = [];
afterEach(async () => {
	await Promise.all(
		servers
			.splice(0)
			.map(
				(server) =>
					new Promise<void>((resolve, reject) => server.close((error) => (error ? reject(error) : resolve()))),
			),
	);
	for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

async function startServer(): Promise<{
	url: string;
	requests: Array<{ body: Record<string, unknown>; authorization: string | undefined }>;
}> {
	const requests: Array<{ body: Record<string, unknown>; authorization: string | undefined }> = [];
	const server = createServer(async (req, res) => {
		const chunks: Buffer[] = [];
		for await (const chunk of req) chunks.push(Buffer.from(chunk));
		requests.push({
			body: JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>,
			authorization: req.headers.authorization,
		});
		res.writeHead(200, { "content-type": "text/event-stream" });
		res.end(
			'data: {"id":"local","object":"chat.completion.chunk","created":0,"model":"sample","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n',
		);
	});
	await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
	servers.push(server);
	return { url: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`, requests };
}

async function runRequest(provider: string, url: string, cwd: string, agentDir: string): Promise<void> {
	const model: Model<"openai-completions"> = {
		id: "sample",
		name: "sample",
		api: "openai-completions",
		provider,
		baseUrl: url,
		reasoning: false,
		input: ["text"],
		contextWindow: 32768,
		maxTokens: 1024,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	};
	const authStorage = AuthStorage.create(join(agentDir, "auth.json"));
	if (provider !== "ollama-cloud") authStorage.setRuntimeApiKey(provider, "local-secret");
	const { session } = await createAgentSession({
		cwd,
		agentDir,
		model,
		authStorage,
		tools: [],
		sessionManager: SessionManager.inMemory(cwd),
	});
	try {
		await session.prompt("hello");
	} finally {
		session.dispose();
	}
}

test("#6 sends configured Ollama Cloud sampling in actual JSON but does not change another provider", async () => {
	const root = mkdtempSync(join(tmpdir(), "ollama-cloud-request-"));
	roots.push(root);
	const cwd = join(root, "project");
	const agentDir = join(root, "agent");
	mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
	mkdirSync(agentDir);
	writeFileSync(
		join(agentDir, "ollama-cloud.json"),
		JSON.stringify({
			inferenceParams: { temperature: 0.2, top_p: 0.6, seed: 13 },
			models: { sample: { temperature: 0.3 } },
		}),
	);
	writeFileSync(
		join(cwd, ".prime", "agent", "ollama-cloud.json"),
		JSON.stringify({ models: { sample: { top_p: 0.8 } } }),
	);
	writeFileSync(
		join(agentDir, "auth.json"),
		JSON.stringify({ "ollama-cloud": { type: "api_key", key: "stored-credential" } }),
	);
	const previousKey = process.env.OLLAMA_API_KEY;
	process.env.OLLAMA_API_KEY = "different-env-credential";
	const server = await startServer();
	try {
		await runRequest("ollama-cloud", server.url, cwd, agentDir);
		await runRequest("other-provider", server.url, cwd, agentDir);
	} finally {
		if (previousKey === undefined) delete process.env.OLLAMA_API_KEY;
		else process.env.OLLAMA_API_KEY = previousKey;
	}
	expect(server.requests).toHaveLength(2);
	expect(server.requests[0].authorization).toBe("Bearer stored-credential");
	expect(server.requests[0].body).toMatchObject({
		model: "sample",
		temperature: 0.3,
		top_p: 0.8,
		seed: 13,
		stream: true,
	});
	expect(server.requests[0].body.messages).toBeDefined();
	expect(server.requests[1].body).not.toHaveProperty("top_p");
	expect(server.requests[1].body).not.toHaveProperty("seed");
});

test("#6 user before_provider_request extension overrides native Ollama Cloud defaults", async () => {
	const root = mkdtempSync(join(tmpdir(), "ollama-cloud-extension-"));
	roots.push(root);
	const cwd = join(root, "project");
	const agentDir = join(root, "agent");
	mkdirSync(cwd);
	mkdirSync(join(agentDir, "extensions"), { recursive: true });
	writeFileSync(
		join(agentDir, "ollama-cloud.json"),
		JSON.stringify({ inferenceParams: { temperature: 0.2, seed: 7 } }),
	);
	writeFileSync(
		join(agentDir, "auth.json"),
		JSON.stringify({ "ollama-cloud": { type: "api_key", key: "stored-credential" } }),
	);
	writeFileSync(
		join(agentDir, "extensions", "sampling.ts"),
		'export default (pi) => pi.on("before_provider_request", (event) => ({ ...event.payload, temperature: 0.9 }));\n',
	);
	const server = await startServer();
	await runRequest("ollama-cloud", server.url, cwd, agentDir);
	expect(server.requests).toHaveLength(1);
	expect(server.requests[0].body).toMatchObject({ temperature: 0.9, seed: 7 });
});

test("#6 sessions keep project-specific compaction caps; reload updates or removes the current cap", async () => {
	const root = mkdtempSync(join(tmpdir(), "ollama-cloud-caps-"));
	roots.push(root);
	const agentDir = join(root, "agent");
	const projectA = join(root, "a");
	const projectB = join(root, "b");
	for (const cwd of [projectA, projectB]) mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
	mkdirSync(agentDir);
	writeFileSync(
		join(agentDir, "auth.json"),
		JSON.stringify({ "ollama-cloud": { type: "api_key", key: "stored-credential" } }),
	);
	const configPath = (cwd: string) => join(cwd, ".prime", "agent", "ollama-cloud.json");
	writeFileSync(configPath(projectA), JSON.stringify({ modelOverrides: { sample: { contextWindow: 4000 } } }));
	writeFileSync(configPath(projectB), JSON.stringify({ modelOverrides: { sample: { contextWindow: 8000 } } }));
	const server = await startServer();
	const model: Model<"openai-completions"> = {
		id: "sample",
		name: "sample",
		api: "openai-completions",
		provider: "ollama-cloud",
		baseUrl: server.url,
		reasoning: false,
		input: ["text"],
		contextWindow: 32768,
		maxTokens: 1024,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	};
	const authStorage = AuthStorage.create(join(agentDir, "auth.json"));
	const modelRegistry = ModelRegistry.create(authStorage, join(agentDir, "models.json"));
	const create = (cwd: string) =>
		createAgentSession({
			cwd,
			agentDir,
			model,
			authStorage,
			modelRegistry,
			tools: [],
			sessionManager: SessionManager.inMemory(cwd),
		});
	const { session: a } = await create(projectA);
	const { session: b } = await create(projectB);
	try {
		expect(a.model?.contextWindow).toBe(4000);
		expect(a.getContextUsage()?.contextWindow).toBe(4000);
		expect(b.model?.contextWindow).toBe(8000);
		expect(b.getContextUsage()?.contextWindow).toBe(8000);
		await b.setModel(model);
		expect(b.getContextUsage()?.contextWindow).toBe(8000);
		await a.prompt("hello");
		await b.prompt("hello");
		expect(server.requests).toHaveLength(2);
		writeFileSync(configPath(projectA), JSON.stringify({ modelOverrides: { sample: { contextWindow: 6000 } } }));
		await a.reload();
		expect(a.getContextUsage()?.contextWindow).toBe(6000);
		expect(b.getContextUsage()?.contextWindow).toBe(8000);
		writeFileSync(configPath(projectA), JSON.stringify({}));
		await a.reload();
		expect(a.getContextUsage()?.contextWindow).toBe(32768);
		expect(b.getContextUsage()?.contextWindow).toBe(8000);
	} finally {
		a.dispose();
		b.dispose();
	}
});

test("#6 saved-session restore and scoped model cycle keep project caps", async () => {
	const previousOffline = process.env.PI_OFFLINE;
	process.env.PI_OFFLINE = "1";
	try {
		const root = mkdtempSync(join(tmpdir(), "ollama-cloud-restore-"));
		roots.push(root);
		const cwd = join(root, "project");
		const agentDir = join(root, "agent");
		mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
		mkdirSync(agentDir);
		writeFileSync(
			join(cwd, ".prime", "agent", "ollama-cloud.json"),
			JSON.stringify({
				modelOverrides: {
					"glm-5.3": { contextWindow: 4000 },
					"glm-5.3-flash": { contextWindow: 6000 },
				},
			}),
		);
		const server = await startServer();
		const authStorage = AuthStorage.create(join(agentDir, "auth.json"));
		authStorage.setRuntimeApiKey("ollama-cloud", "local-secret");
		const modelRegistry = ModelRegistry.create(authStorage, join(agentDir, "models.json"));
		const base = modelRegistry.find("ollama-cloud", "glm-5.3");
		const next = modelRegistry.find("ollama-cloud", "glm-5.3-flash");
		expect(base).toBeDefined();
		expect(next).toBeDefined();
		const model = { ...base!, baseUrl: server.url };
		const first = await createAgentSession({
			cwd,
			agentDir,
			model,
			modelRegistry,
			authStorage,
			tools: [],
			sessionManager: SessionManager.create(cwd, join(agentDir, "sessions")),
		});
		let sessionFile: string | undefined;
		try {
			await first.session.prompt("hello");
			sessionFile = first.session.sessionManager.getSessionFile();
			expect(first.session.getContextUsage()?.contextWindow).toBe(4000);
		} finally {
			first.session.dispose();
		}
		expect(sessionFile).toBeDefined();
		const restored = await createAgentSession({
			cwd,
			agentDir,
			modelRegistry,
			authStorage,
			tools: [],
			sessionManager: SessionManager.open(sessionFile!, join(agentDir, "sessions"), cwd),
		});
		try {
			expect(restored.session.model?.id).toBe("glm-5.3");
			expect(restored.session.getContextUsage()?.contextWindow).toBe(4000);
			restored.session.setScopedModels([{ model: base! }, { model: next! }]);
			const cycled = await restored.session.cycleModel("forward");
			expect(cycled?.model.id).toBe("glm-5.3-flash");
			expect(cycled?.model.contextWindow).toBe(6000);
			expect(restored.session.getContextUsage()?.contextWindow).toBe(6000);
		} finally {
			restored.session.dispose();
		}
	} finally {
		if (previousOffline === undefined) delete process.env.PI_OFFLINE;
		else process.env.PI_OFFLINE = previousOffline;
	}
});

test("#6 reload preserves an explicit Ollama model gateway and metadata while updating project cap", async () => {
	const root = mkdtempSync(join(tmpdir(), "ollama-cloud-gateway-"));
	roots.push(root);
	const cwd = join(root, "project");
	const agentDir = join(root, "agent");
	mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
	mkdirSync(agentDir);
	const configPath = join(cwd, ".prime", "agent", "ollama-cloud.json");
	writeFileSync(configPath, JSON.stringify({ modelOverrides: { "glm-5.3": { contextWindow: 4000 } } }));
	const server = await startServer();
	const authStorage = AuthStorage.create(join(agentDir, "auth.json"));
	authStorage.setRuntimeApiKey("ollama-cloud", "local-secret");
	const modelRegistry = ModelRegistry.create(authStorage, join(agentDir, "models.json"));
	const catalogModel = modelRegistry.find("ollama-cloud", "glm-5.3");
	expect(catalogModel).toBeDefined();
	const explicitModel = { ...catalogModel!, baseUrl: server.url, name: "Local gateway", maxTokens: 777 };
	const { session } = await createAgentSession({
		cwd,
		agentDir,
		model: explicitModel,
		modelRegistry,
		authStorage,
		tools: [],
		sessionManager: SessionManager.inMemory(cwd),
	});
	try {
		await session.prompt("before reload");
		writeFileSync(configPath, JSON.stringify({ modelOverrides: { "glm-5.3": { contextWindow: 6000 } } }));
		await session.reload();
		expect(session.model).toMatchObject({
			baseUrl: server.url,
			name: "Local gateway",
			maxTokens: 777,
			contextWindow: 6000,
		});
		await session.prompt("after reload");
		expect(server.requests).toHaveLength(2);
		expect(server.requests.map((request) => request.body.model)).toEqual(["glm-5.3", "glm-5.3"]);
	} finally {
		session.dispose();
	}
});

test("#6 setModel gateway survives reload; cycling back to catalog removes the gateway baseline", async () => {
	const previousOffline = process.env.PI_OFFLINE;
	process.env.PI_OFFLINE = "1";
	const root = mkdtempSync(join(tmpdir(), "ollama-cloud-switch-"));
	roots.push(root);
	try {
		const cwd = join(root, "project");
		const agentDir = join(root, "agent");
		mkdirSync(join(cwd, ".prime", "agent"), { recursive: true });
		mkdirSync(agentDir);
		const configPath = join(cwd, ".prime", "agent", "ollama-cloud.json");
		writeFileSync(configPath, JSON.stringify({ modelOverrides: { "glm-5.3": { contextWindow: 4000 } } }));
		const server = await startServer();
		const authStorage = AuthStorage.create(join(agentDir, "auth.json"));
		authStorage.setRuntimeApiKey("ollama-cloud", "local-secret");
		const modelRegistry = ModelRegistry.create(authStorage, join(agentDir, "models.json"));
		const catalog = modelRegistry.find("ollama-cloud", "glm-5.3");
		const alternate = modelRegistry.find("ollama-cloud", "glm-5.3-flash");
		expect(catalog).toBeDefined();
		expect(alternate).toBeDefined();
		const { session } = await createAgentSession({
			cwd,
			agentDir,
			model: catalog!,
			modelRegistry,
			authStorage,
			tools: [],
			sessionManager: SessionManager.inMemory(cwd),
		});
		try {
			const gateway = { ...catalog!, baseUrl: server.url, name: "Switched gateway", maxTokens: 713 };
			await session.setModel(gateway);
			writeFileSync(configPath, JSON.stringify({ modelOverrides: { "glm-5.3": { contextWindow: 6000 } } }));
			await session.reload();
			expect(session.model).toMatchObject({
				baseUrl: server.url,
				name: "Switched gateway",
				maxTokens: 713,
				contextWindow: 6000,
			});
			await session.prompt("gateway after reload");
			expect(server.requests).toHaveLength(1);
			session.setScopedModels([{ model: catalog! }, { model: alternate! }]);
			await session.cycleModel("forward");
			await session.cycleModel("forward");
			expect(session.model?.id).toBe("glm-5.3");
			expect(session.model?.baseUrl).toBe(catalog!.baseUrl);
			await session.reload();
			expect(session.model?.baseUrl).toBe(catalog!.baseUrl);
			expect(session.model?.name).toBe(catalog!.name);
			expect(session.model?.contextWindow).toBe(6000);
		} finally {
			session.dispose();
		}
	} finally {
		if (previousOffline === undefined) delete process.env.PI_OFFLINE;
		else process.env.PI_OFFLINE = previousOffline;
	}
});
