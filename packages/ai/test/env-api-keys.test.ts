import { afterEach, expect, test } from "vitest";
import { findEnvKeys, getEnvApiKey } from "../src/env-api-keys.js";

const previous = process.env.OLLAMA_API_KEY;
afterEach(() => {
	if (previous === undefined) delete process.env.OLLAMA_API_KEY;
	else process.env.OLLAMA_API_KEY = previous;
});

test("#6 Ollama Cloud uses OLLAMA_API_KEY and reports configured auth", () => {
	delete process.env.OLLAMA_API_KEY;
	expect(findEnvKeys("ollama-cloud")).toBeUndefined();
	expect(getEnvApiKey("ollama-cloud")).toBeUndefined();
	process.env.OLLAMA_API_KEY = "local-key";
	expect(findEnvKeys("ollama-cloud")).toEqual(["OLLAMA_API_KEY"]);
	expect(getEnvApiKey("ollama-cloud")).toBe("local-key");
});
