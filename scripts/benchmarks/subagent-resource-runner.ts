/** Opt-in issue #9 inline AgentSession benchmark. Started only by subagent_resources.py. */
import { createInterface } from "node:readline";
import { monitorEventLoopDelay, performance } from "node:perf_hooks";
import { fauxAssistantMessage } from "@earendil-works/pi-ai";
import { createHarness } from "../../packages/coding-agent/test/suite/harness.js";

function deferred() {
    let resolve!: () => void;
    const promise = new Promise<void>((done) => {
        resolve = done;
    });
    return { promise, resolve };
}

const count = Number(process.argv[2]);
if (!process.env.PRIME_RESOURCE_BENCH_HOME || process.env.HOME !== process.env.PRIME_RESOURCE_BENCH_HOME) {
    throw new Error("Use subagent_resources.py; an isolated benchmark HOME is required");
}
if (!Number.isSafeInteger(count) || count < 1) throw new Error("Invalid child count");
const input = createInterface({ input: process.stdin });
const acknowledgements = input[Symbol.asyncIterator]();
const histogram = monitorEventLoopDelay({ resolution: 10 });
const harness = await createHarness({ provider: "faux-resource-bench", persistSession: true });
const releases: Array<ReturnType<typeof deferred>> = [];

async function phase(name: string) {
    const start = performance.now();
    const snapshot = harness.session.getRlmChildSnapshots();
    const json = JSON.stringify(snapshot);
    const snapshotMs = performance.now() - start;
    histogram.reset();
    histogram.enable();
    process.stdout.write(
        `${JSON.stringify({
            event: "phase",
            name,
            count,
            children: snapshot,
            roster_snapshot_ms: snapshotMs,
            roster_snapshot_bytes: Buffer.byteLength(json),
        })}\n`,
    );
    const acknowledgement = await acknowledgements.next();
    histogram.disable();
    if (acknowledgement.done || acknowledgement.value !== name) throw new Error("Lost phase acknowledgement");
    process.stdout.write(
        `${JSON.stringify({
            event: "window_end",
            name,
            event_loop_samples: histogram.count,
            event_loop_delay_mean_ms: histogram.count ? histogram.mean / 1e6 : null,
            event_loop_delay_max_ms: histogram.count ? histogram.max / 1e6 : null,
            event_loop_delay_p99_ms: histogram.count ? histogram.percentile(99) / 1e6 : null,
        })}\n`,
    );
}

try {
    await phase("parent_baseline");
    const entered = Array.from({ length: count }, () => deferred());
    releases.push(...Array.from({ length: count }, () => deferred()));
    harness.setResponses(
        entered.map((gate, index) => async () => {
            gate.resolve();
            await releases[index].promise;
            return fauxAssistantMessage("benchmark child complete");
        }),
    );
    const handles = await Promise.all(
        entered.map((_, index) =>
            harness.session.runRlmChild("resource benchmark", { name: `resource-${index}` }),
        ),
    );
    await Promise.all(entered.map((gate) => gate.promise));
    const running = harness.session.getRlmChildSnapshots();
    if (running.length !== count || running.some((child) => child.status !== "running")) {
        throw new Error("Not all real child sessions reached the provider gate");
    }
    await phase("active_provider_wait_never_kernel");
    for (const release of releases) release.resolve();
    const collected = await harness.session.collectRlmChildren(
        handles.map((handle) => handle.rlm_child_id),
        30_000,
    );
    if (
        collected.results.length !== count ||
        collected.results.some((child) => !child.settled || child.status !== "done")
    ) {
        throw new Error(`Child completion failed: ${JSON.stringify(collected)}`);
    }
    for (const handle of handles) {
        const child = harness.session.getRlmChildSession(handle.rlm_child_id);
        if (!child || child.isSessionActive) throw new Error("Completed child was not retained and idle");
    }
    await phase("completed_retained_idle_never_kernel");
    for (const handle of handles) await harness.session.deleteRlmSubagent(handle.rlm_child_id);
    await harness.session.waitForRlmQuiescence();
    if (harness.session.getRlmChildSnapshots().length !== 0)
        throw new Error("Deleted children remain resident");
    await phase("children_deleted_not_passivated");
} finally {
    for (const release of releases) release.resolve();
    histogram.disable();
    input.close();
    await harness.session.disposeAsync();
    harness.cleanup();
}
process.stdout.write(`${JSON.stringify({ event: "complete" })}\n`);
