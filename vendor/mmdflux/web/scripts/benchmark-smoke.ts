import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

import { JSDOM } from "jsdom";

import {
  type BenchmarkEngineInput,
  type BenchmarkReport,
  type BenchmarkReportMetadata,
  createBenchmarkReport,
  createSummaryRows,
  isWasmBuildProfile,
  toBenchmarkReportJson,
  type WasmBuildProfile,
} from "../src/benchmark-report.ts";
import {
  type BenchmarkEngineRunner,
  createBenchmarkEngineRunners,
} from "../src/benchmarks/engine-runners.ts";
import {
  BENCHMARK_SCENARIOS,
  type BenchmarkScenario,
} from "../src/benchmarks/scenarios.ts";
import { formatTable } from "./table-format.ts";

type EngineId = BenchmarkEngineRunner["id"];

interface BenchmarkSmokeThresholds {
  maxMeanMsByEngine: Record<EngineId, number>;
  maxP95MsByEngine: Record<EngineId, number>;
}

export interface BenchmarkSmokePolicy {
  warmupIterations: number;
  measurementIterations: number;
  scenarios: readonly BenchmarkScenario[];
  thresholds: BenchmarkSmokeThresholds;
}

interface BenchmarkSmokeRunOptions {
  policy?: BenchmarkSmokePolicy;
  reportMetadata?: BenchmarkReportMetadata;
  createRunners?: () => Promise<BenchmarkEngineRunner[]>;
  now?: () => number;
}

interface BenchmarkSmokeRunResult {
  report: BenchmarkReport;
  failures: string[];
}

interface BenchmarkSmokeCliOptions {
  outPath?: string;
  full: boolean;
  enforceThresholds: boolean;
  wasmProfile?: WasmBuildProfile;
}

const SMOKE_SCENARIOS = BENCHMARK_SCENARIOS.filter(
  (scenario) => scenario.complexity !== "large",
);

export const BENCHMARK_SMOKE_POLICY: BenchmarkSmokePolicy = {
  warmupIterations: 1,
  measurementIterations: 3,
  scenarios: SMOKE_SCENARIOS,
  thresholds: {
    maxMeanMsByEngine: {
      mmdflux: 500,
      mermaid: 2_000,
    },
    maxP95MsByEngine: {
      mmdflux: 1_000,
      mermaid: 3_500,
    },
  },
};

function toMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function roundTo(value: number, precision = 2): number {
  const scale = 10 ** precision;
  return Math.round(value * scale) / scale;
}

function formatSummaryTable(report: BenchmarkReport): string {
  const rows = createSummaryRows(report).map((row) => ({
    scenario: `${row.scenarioName} (${row.complexity})`,
    engine: row.engineLabel,
    meanMs: row.meanMs.toFixed(2),
    medianMs: row.medianMs.toFixed(2),
    p95Ms: row.p95Ms.toFixed(2),
    minMs: row.minMs.toFixed(2),
    maxMs: row.maxMs.toFixed(2),
  }));

  return formatTable(rows, [
    {
      header: "Scenario",
      value: (row) => row.scenario,
    },
    {
      header: "Engine",
      value: (row) => row.engine,
    },
    {
      header: "Mean (ms)",
      align: "right",
      value: (row) => row.meanMs,
    },
    {
      header: "Median (ms)",
      align: "right",
      value: (row) => row.medianMs,
    },
    {
      header: "P95 (ms)",
      align: "right",
      value: (row) => row.p95Ms,
    },
    {
      header: "Min (ms)",
      align: "right",
      value: (row) => row.minMs,
    },
    {
      header: "Max (ms)",
      align: "right",
      value: (row) => row.maxMs,
    },
  ]);
}

function parseCliOptions(args: readonly string[]): BenchmarkSmokeCliOptions {
  const options: BenchmarkSmokeCliOptions = {
    full: false,
    enforceThresholds: true,
  };
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--out") {
      const outPath = args[index + 1];
      if (!outPath) {
        throw new Error("missing value for --out");
      }
      options.outPath = outPath;
      index += 1;
      continue;
    }
    if (arg === "--full") {
      options.full = true;
      continue;
    }
    if (arg === "--no-thresholds") {
      options.enforceThresholds = false;
      continue;
    }
    if (arg === "--wasm-profile") {
      const wasmProfile = args[index + 1];
      if (!wasmProfile) {
        throw new Error("missing value for --wasm-profile");
      }
      if (!isWasmBuildProfile(wasmProfile)) {
        throw new Error(
          `invalid --wasm-profile value: ${wasmProfile} (expected dev|release)`,
        );
      }
      options.wasmProfile = wasmProfile;
      index += 1;
      continue;
    }

    throw new Error(`unknown argument: ${arg}`);
  }

  return options;
}

function policyForCliOptions(
  cliOptions: BenchmarkSmokeCliOptions,
): BenchmarkSmokePolicy {
  if (!cliOptions.full) {
    return BENCHMARK_SMOKE_POLICY;
  }

  return {
    ...BENCHMARK_SMOKE_POLICY,
    scenarios: BENCHMARK_SCENARIOS,
  };
}

function evaluateThresholds(
  report: BenchmarkReport,
  policy: BenchmarkSmokePolicy,
): string[] {
  const failures: string[] = [];

  for (const row of createSummaryRows(report)) {
    const meanLimit =
      policy.thresholds.maxMeanMsByEngine[row.engineId as EngineId];
    const p95Limit =
      policy.thresholds.maxP95MsByEngine[row.engineId as EngineId];

    if (row.meanMs > meanLimit) {
      failures.push(
        `${row.engineId} mean ${row.meanMs.toFixed(
          2,
        )}ms exceeded ${meanLimit.toFixed(2)}ms on ${row.scenarioId}`,
      );
    }

    if (row.p95Ms > p95Limit) {
      failures.push(
        `${row.engineId} p95 ${row.p95Ms.toFixed(
          2,
        )}ms exceeded ${p95Limit.toFixed(2)}ms on ${row.scenarioId}`,
      );
    }
  }

  return failures;
}

function installDomGlobalsForMermaid(): () => void {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", {
    pretendToBeVisual: true,
    url: "https://benchmark.local/",
  });
  const { window } = dom;

  const restoreEntries: Array<() => void> = [];
  const defineGlobal = <T>(name: string, value: T): void => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
    restoreEntries.push(() => {
      if (descriptor) {
        Object.defineProperty(globalThis, name, descriptor);
      } else {
        Reflect.deleteProperty(globalThis, name);
      }
    });
    Object.defineProperty(globalThis, name, {
      configurable: true,
      writable: true,
      value,
    });
  };

  defineGlobal("window", window);
  defineGlobal("document", window.document);
  defineGlobal("navigator", window.navigator);
  defineGlobal("Element", window.Element);
  defineGlobal("Node", window.Node);
  defineGlobal("HTMLElement", window.HTMLElement);
  defineGlobal("SVGElement", window.SVGElement);
  defineGlobal("SVGGraphicsElement", window.SVGGraphicsElement);
  defineGlobal("DOMParser", window.DOMParser);
  defineGlobal("XMLSerializer", window.XMLSerializer);
  defineGlobal("getComputedStyle", window.getComputedStyle.bind(window));
  defineGlobal(
    "requestAnimationFrame",
    window.requestAnimationFrame.bind(window),
  );
  defineGlobal(
    "cancelAnimationFrame",
    window.cancelAnimationFrame.bind(window),
  );
  defineGlobal("location", window.location);

  const svgElementPrototype = window.SVGElement.prototype as SVGElement & {
    getBBox?: () => { x: number; y: number; width: number; height: number };
  };

  if (!svgElementPrototype.getBBox) {
    Object.defineProperty(svgElementPrototype, "getBBox", {
      configurable: true,
      value() {
        const text = (this.textContent ?? "").trim();
        return {
          x: 0,
          y: 0,
          width: Math.max(text.length * 8, 8),
          height: 16,
        };
      },
    });
  }

  return () => {
    for (const restore of restoreEntries.reverse()) {
      restore();
    }
    window.close();
  };
}

async function loadMmdfluxModuleForNode(): Promise<{
  default: () => Promise<void>;
  render: (input: string, format: string, configJson: string) => string;
}> {
  const wasmModule = await import("../src/wasm-pkg/mmdflux_wasm.js");
  let initialized = false;

  return {
    default: async () => {
      if (initialized) {
        return;
      }

      const wasmBytes = await readFile(
        new URL("../src/wasm-pkg/mmdflux_wasm_bg.wasm", import.meta.url),
      );
      await wasmModule.default({ module_or_path: wasmBytes });
      initialized = true;
    },
    render: wasmModule.render,
  };
}

async function defaultCreateRunners(): Promise<BenchmarkEngineRunner[]> {
  return createBenchmarkEngineRunners({
    loadMmdfluxModule: loadMmdfluxModuleForNode,
  });
}

async function sampleRunner(
  runner: BenchmarkEngineRunner,
  scenario: BenchmarkScenario,
  policy: BenchmarkSmokePolicy,
  now: () => number,
): Promise<BenchmarkEngineInput> {
  for (let index = 0; index < policy.warmupIterations; index += 1) {
    await runner.warm(scenario.input);
  }

  const samplesMs: number[] = [];
  for (let index = 0; index < policy.measurementIterations; index += 1) {
    const startedAt = now();
    const output = await runner.render(scenario.input);
    const completedAt = now();

    if (output.trim().length === 0) {
      throw new Error(
        `${runner.id} produced empty output for scenario ${scenario.id}`,
      );
    }

    samplesMs.push(roundTo(completedAt - startedAt));
  }

  return {
    engineId: runner.id,
    engineLabel: runner.label,
    samplesMs,
  };
}

export async function runBenchmarkSmoke(
  options: BenchmarkSmokeRunOptions = {},
): Promise<BenchmarkSmokeRunResult> {
  const policy = options.policy ?? BENCHMARK_SMOKE_POLICY;
  const reportMetadata = options.reportMetadata;
  const now = options.now ?? (() => performance.now());
  const createRunners = options.createRunners ?? defaultCreateRunners;

  const restoreGlobals = installDomGlobalsForMermaid();
  try {
    const runners = await createRunners();
    const scenarioResults = [];

    for (const scenario of policy.scenarios) {
      const engines: BenchmarkEngineInput[] = [];
      for (const runner of runners) {
        engines.push(await sampleRunner(runner, scenario, policy, now));
      }

      scenarioResults.push({
        scenario,
        engines,
      });
    }

    const report = createBenchmarkReport({
      metadata: reportMetadata,
      warmupIterations: policy.warmupIterations,
      measurementIterations: policy.measurementIterations,
      scenarios: scenarioResults,
    });

    return {
      report,
      failures: evaluateThresholds(report, policy),
    };
  } finally {
    restoreGlobals();
  }
}

async function main(
  args: readonly string[] = process.argv.slice(2),
): Promise<void> {
  try {
    const cliOptions = parseCliOptions(args);
    const policy = policyForCliOptions(cliOptions);
    const reportMetadata: BenchmarkReportMetadata | undefined =
      cliOptions.wasmProfile
        ? { wasmProfile: cliOptions.wasmProfile }
        : undefined;

    console.log(
      `Running benchmark ${cliOptions.full ? "full" : "smoke"} checks...`,
    );
    console.log(
      `Scenarios: ${policy.scenarios
        .map((scenario) => scenario.id)
        .join(", ")}`,
    );
    console.log(
      `Iterations: warmup=${policy.warmupIterations}, measured=${policy.measurementIterations}`,
    );
    console.log(
      `Threshold checks: ${cliOptions.enforceThresholds ? "enabled" : "disabled"}`,
    );
    if (reportMetadata?.wasmProfile) {
      console.log(`Wasm profile: ${reportMetadata.wasmProfile}`);
    }

    const result = await runBenchmarkSmoke({ policy, reportMetadata });

    console.log("");
    console.log(formatSummaryTable(result.report));
    console.log("");

    if (cliOptions.outPath) {
      await writeFile(
        cliOptions.outPath,
        toBenchmarkReportJson(result.report),
        "utf8",
      );
      console.log(`Wrote benchmark report JSON: ${cliOptions.outPath}`);
      console.log("");
    }

    if (result.failures.length > 0 && cliOptions.enforceThresholds) {
      console.error("Benchmark smoke checks failed:");
      for (const failure of result.failures) {
        console.error(`- ${failure}`);
      }
      process.exitCode = 1;
      return;
    }

    if (result.failures.length > 0) {
      console.log("Threshold checks disabled; potential issues observed:");
      for (const failure of result.failures) {
        console.log(`- ${failure}`);
      }
      console.log("");
    }

    console.log(
      `Benchmark ${cliOptions.full ? "full" : "smoke"} checks completed.`,
    );
  } catch (error) {
    console.error(`Benchmark smoke checks failed to run: ${toMessage(error)}`);
    process.exitCode = 1;
  }
}

const entryUrl = process.argv[1] ? pathToFileURL(process.argv[1]).href : "";
if (import.meta.url === entryUrl) {
  await main();
}
