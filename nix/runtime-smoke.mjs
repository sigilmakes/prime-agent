// Installed public SDK smoke: no model call, daemon, or Python kernel.
// check.sh runs each phase in a fresh Node process to rule out in-memory resume.
import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { SessionManager } from "@earendil-works/pi-coding-agent";

// Reject stale reporting artifacts as well as removed endpoints in emitted bundles.
const sdkRoot = new URL("./", import.meta.resolve("@earendil-works/pi-coding-agent"));
for (const name of ["telemetry", "agent-traces", "platform-fidelity"]) {
    for (const extension of ["js", "js.map", "d.ts", "d.ts.map"]) {
        assert.equal(existsSync(new URL(`core/${name}.${extension}`, sdkRoot)), false,
            `obsolete reporting artifact: ${name}.${extension}`);
    }
}
function checkReportingArtifacts(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
        const child = new URL(entry.name + (entry.isDirectory() ? "/" : ""), directory);
        if (entry.isDirectory()) checkReportingArtifacts(child);
        else if (entry.name.endsWith(".js")) {
            const source = readFileSync(child, "utf8");
            for (const endpoint of ["agent-analytics/events", "agent-traces/sessions"]) {
                assert.equal(source.includes(endpoint), false, `removed reporting endpoint in ${child}`);
            }
        }
    }
}
checkReportingArtifacts(sdkRoot);

const [phase, state] = process.argv.slice(2);
assert.ok(state, "private state directory is required");
const manifest = join(state, "session-smoke.json");
const cwd = join(state, "work");
const sessions = process.env.PRIME_AGENT_SESSION_DIR;
assert.equal(sessions, join(state, "sessions"));
const user = (content) => ({ role: "user", content, timestamp: 1 });
const assistant = {
    role: "assistant",
    content: [{ type: "text", text: "offline answer" }],
    api: "openai-completions",
    provider: "smoke",
    model: "offline",
    usage: {
        input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2,
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: "stop",
    timestamp: 2,
};
const transcript = (session) => session.buildSessionContext().messages.map((message) =>
    typeof message.content === "string" ? message.content : message.content.map((part) => part.text).join(""));
const readEntries = (path) => readFileSync(path, "utf8").trim().split("\n").map((line) => JSON.parse(line));

switch (phase) {
    case "create": {
        const session = SessionManager.create(cwd, sessions);
        session.appendModelChange("smoke", "offline");
        session.appendMessage(user("first question"));
        const answerId = session.appendMessage(assistant);
        const tailId = session.appendMessage(user("original continuation"));
        const path = session.getSessionFile();
        const entries = readEntries(path);
        assert.equal(entries[0].type, "session");
        assert.equal(entries[0].id, session.getSessionId());
        assert.equal(entries.at(-1).id, tailId);
        assert.equal(entries.at(-1).parentId, answerId);
        writeFileSync(manifest, JSON.stringify({ path, id: session.getSessionId(), answerId, tailId }));
        break;
    }
    case "branch": {
        const saved = JSON.parse(readFileSync(manifest, "utf8"));
        const session = SessionManager.open(saved.path, sessions);
        assert.equal(session.getSessionId(), saved.id);
        assert.equal(session.getCwd(), cwd);
        assert.equal(session.getLeafId(), saved.tailId);
        assert.deepEqual(transcript(session), ["first question", "offline answer", "original continuation"]);
        assert.deepEqual(session.buildSessionContext().model, { provider: "smoke", modelId: "offline" });
        const original = readFileSync(saved.path, "utf8");
        const branchPath = session.createBranchedSession(saved.answerId);
        assert.notEqual(branchPath, saved.path);
        assert.notEqual(session.getSessionId(), saved.id);
        const branchTailId = session.appendMessage(user("branched continuation"));
        assert.equal(readFileSync(saved.path, "utf8"), original, "branch must not rewrite the source");
        writeFileSync(manifest, JSON.stringify({ ...saved, branchPath, branchId: session.getSessionId(), branchTailId }));
        break;
    }
    case "verify": {
        const saved = JSON.parse(readFileSync(manifest, "utf8"));
        const original = await SessionManager.openAsync(saved.path, sessions);
        const branch = await SessionManager.openAsync(saved.branchPath, sessions);
        assert.equal(original.getSessionId(), saved.id);
        assert.equal(original.getLeafId(), saved.tailId);
        assert.deepEqual(transcript(original), ["first question", "offline answer", "original continuation"]);
        assert.equal(branch.getSessionId(), saved.branchId);
        assert.equal(branch.getHeader().parentSession, saved.path);
        assert.equal(branch.getLeafId(), saved.branchTailId);
        assert.equal(branch.getEntry(saved.branchTailId).parentId, saved.answerId);
        assert.deepEqual(transcript(branch), ["first question", "offline answer", "branched continuation"]);
        const listed = await SessionManager.list(cwd, sessions);
        assert.deepEqual(listed.map((item) => item.id).sort(), [saved.id, saved.branchId].sort());
        assert.ok(listed.every((item) => item.messageCount === 3));
        assert.equal(readEntries(saved.branchPath).filter((entry) => entry.type === "session").length, 1);
        console.log("Installed SDK session create / persist / resume / branch smoke passed");
        break;
    }
    default:
        throw new Error(`Unknown smoke phase: ${phase}`);
}
