import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

import {
  type BenchmarkReport,
  type BenchmarkSummaryRow,
  createSummaryRows,
  isWasmBuildProfile,
  toBenchmarkReportJson,
  type WasmBuildProfile,
} from "../src/benchmark-report.ts";
import { runBenchmarkSmoke } from "./benchmark-smoke.ts";
import { formatTable } from "./table-format.ts";

interface CompareCliOptions {
  baselinePath: string;
  currentPath?: string;
  outCurrentPath?: string;
  maxRegressionPct?: number;
  wasmProfile?: WasmBuildProfile;
}

interface DeltaRow {
  scenarioId: string;
  engineId: string;
  meanBase: number;
  meanCurrent: number;
  deltaMeanMs: number;
  deltaMeanPct: number | null;
  p95Base: number;
  p95Current: number;
  deltaP95Ms: number;
  deltaP95Pct: number | null;
  meanSpeedup: number | null;
}

interface WasmProfileCompatibilityResult {
  issue: string | null;
  warning: string | null;
}

function toMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function parseNumberArg(name: string, rawValue: string): number {
  const parsed = Number(rawValue);
  if (!Number.isFinite(parsed)) {
    throw new Error(`invalid numeric value for ${name}: ${rawValue}`);
  }
  return parsed;
}

function parseCliOptions(args: readonly string[]): CompareCliOptions {
  let baselinePath: string | undefined;
  let currentPath: string | undefined;
  let outCurrentPath: string | undefined;
  let maxRegressionPct: number | undefined;
  let wasmProfile: WasmBuildProfile | undefined;

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--baseline") {
      const value = args[index + 1];
      if (!value) {
        throw new Error("missing value for --baseline");
      }
      baselinePath = value;
      index += 1;
      continue;
    }
    if (arg === "--current") {
      const value = args[index + 1];
      if (!value) {
        throw new Error("missing value for --current");
      }
      currentPath = value;
      index += 1;
      continue;
    }
    if (arg === "--out-current") {
      const value = args[index + 1];
      if (!value) {
        throw new Error("missing value for --out-current");
      }
      outCurrentPath = value;
      index += 1;
      continue;
    }
    if (arg === "--max-regression-pct") {
      const value = args[index + 1];
      if (!value) {
        throw new Error("missing value for --max-regression-pct");
      }
      maxRegressionPct = parseNumberArg(arg, value);
      index += 1;
      continue;
    }
    if (arg === "--wasm-profile") {
      const value = args[index + 1];
      if (!value) {
        throw new Error("missing value for --wasm-profile");
      }
      if (!isWasmBuildProfile(value)) {
        throw new Error(
          `invalid --wasm-profile value: ${value} (expected dev|release)`,
        );
      }
      wasmProfile = value;
      index += 1;
      continue;
    }

    throw new Error(`unknown argument: ${arg}`);
  }

  if (!baselinePath) {
    throw new Error("missing required argument: --baseline <path>");
  }

  return {
    baselinePath,
    currentPath,
    outCurrentPath,
    maxRegressionPct,
    wasmProfile,
  };
}

function parseBenchmarkReport(
  rawJson: string,
  source: string,
): BenchmarkReport {
  let parsed: unknown;
  try {
    parsed = JSON.parse(rawJson);
  } catch (error) {
    throw new Error(
      `failed to parse benchmark report JSON (${source}): ${toMessage(error)}`,
    );
  }

  if (
    typeof parsed !== "object" ||
    parsed === null ||
    !("schemaVersion" in parsed) ||
    (parsed as { schemaVersion: unknown }).schemaVersion !== 1 ||
    !("scenarios" in parsed) ||
    !Array.isArray((parsed as { scenarios: unknown }).scenarios)
  ) {
    throw new Error(`invalid benchmark report schema (${source})`);
  }

  const metadata = (parsed as { metadata?: unknown }).metadata;
  if (metadata !== undefined) {
    if (typeof metadata !== "object" || metadata === null) {
      throw new Error(`invalid benchmark report metadata (${source})`);
    }
    const wasmProfile = (metadata as { wasmProfile?: unknown }).wasmProfile;
    if (wasmProfile !== undefined && !isWasmBuildProfile(wasmProfile)) {
      throw new Error(`invalid benchmark report wasmProfile (${source})`);
    }
  }

  return parsed as BenchmarkReport;
}

export function evaluateWasmProfileCompatibility(
  baselineReport: BenchmarkReport,
  currentReport: BenchmarkReport,
): WasmProfileCompatibilityResult {
  const baselineProfile = baselineReport.metadata?.wasmProfile;
  const currentProfile = currentReport.metadata?.wasmProfile;

  if (baselineProfile && currentProfile) {
    if (baselineProfile !== currentProfile) {
      return {
        issue: `Wasm profile mismatch: baseline=${baselineProfile}, current=${currentProfile}. Compare reports generated from the same Wasm profile.`,
        warning: null,
      };
    }

    return {
      issue: null,
      warning: null,
    };
  }

  if (baselineProfile || currentProfile) {
    return {
      issue:
        "missing wasm profile metadata in one report. Re-run both benchmarks with --wasm-profile to enforce apples-to-apples comparisons.",
      warning: null,
    };
  }

  return {
    issue: null,
    warning:
      "cannot verify Wasm profile compatibility because both reports are missing wasm profile metadata.",
  };
}

function rowKey(
  row: Pick<BenchmarkSummaryRow, "scenarioId" | "engineId">,
): string {
  return `${row.scenarioId}::${row.engineId}`;
}

function toPercentDelta(
  currentValue: number,
  baselineValue: number,
): number | null {
  if (baselineValue === 0) {
    return null;
  }
  return ((currentValue - baselineValue) / baselineValue) * 100;
}

function formatSigned(value: number, digits = 2): string {
  const prefix = value > 0 ? "+" : "";
  return `${prefix}${value.toFixed(digits)}`;
}

function formatSignedPercent(value: number | null): string {
  if (value === null) {
    return "n/a";
  }
  const prefix = value > 0 ? "+" : "";
  return `${prefix}${value.toFixed(2)}%`;
}

function formatSpeedup(value: number | null): string {
  if (value === null) {
    return "n/a";
  }
  return `${value.toFixed(2)}x`;
}

function buildDeltaRows(
  baselineRows: readonly BenchmarkSummaryRow[],
  currentRows: readonly BenchmarkSummaryRow[],
): {
  deltas: DeltaRow[];
  missingFromBaseline: string[];
  missingFromCurrent: string[];
} {
  const baselineByKey = new Map(
    baselineRows.map((row) => [rowKey(row), row] as const),
  );
  const currentByKey = new Map(
    currentRows.map((row) => [rowKey(row), row] as const),
  );

  const deltas: DeltaRow[] = [];
  const missingFromBaseline: string[] = [];
  const missingFromCurrent: string[] = [];

  for (const currentRow of currentRows) {
    const key = rowKey(currentRow);
    const baselineRow = baselineByKey.get(key);
    if (!baselineRow) {
      missingFromBaseline.push(key);
      continue;
    }

    const deltaMeanMs = currentRow.meanMs - baselineRow.meanMs;
    const deltaP95Ms = currentRow.p95Ms - baselineRow.p95Ms;
    deltas.push({
      scenarioId: currentRow.scenarioId,
      engineId: currentRow.engineId,
      meanBase: baselineRow.meanMs,
      meanCurrent: currentRow.meanMs,
      deltaMeanMs,
      deltaMeanPct: toPercentDelta(currentRow.meanMs, baselineRow.meanMs),
      p95Base: baselineRow.p95Ms,
      p95Current: currentRow.p95Ms,
      deltaP95Ms,
      deltaP95Pct: toPercentDelta(currentRow.p95Ms, baselineRow.p95Ms),
      meanSpeedup:
        currentRow.meanMs === 0 ? null : baselineRow.meanMs / currentRow.meanMs,
    });
  }

  for (const baselineRow of baselineRows) {
    const key = rowKey(baselineRow);
    if (!currentByKey.has(key)) {
      missingFromCurrent.push(key);
    }
  }

  return {
    deltas,
    missingFromBaseline,
    missingFromCurrent,
  };
}

function formatCurrentSummaryTable(report: BenchmarkReport): string {
  const rows = createSummaryRows(report).map((row) => ({
    scenario: row.scenarioId,
    engine: row.engineId,
    meanMs: row.meanMs.toFixed(2),
    medianMs: row.medianMs.toFixed(2),
    p95Ms: row.p95Ms.toFixed(2),
    minMs: row.minMs.toFixed(2),
    maxMs: row.maxMs.toFixed(2),
  }));

  return formatTable(rows, [
    { header: "Scenario", value: (row) => row.scenario },
    { header: "Engine", value: (row) => row.engine },
    { header: "Mean", align: "right", value: (row) => row.meanMs },
    { header: "Median", align: "right", value: (row) => row.medianMs },
    { header: "P95", align: "right", value: (row) => row.p95Ms },
    { header: "Min", align: "right", value: (row) => row.minMs },
    { header: "Max", align: "right", value: (row) => row.maxMs },
  ]);
}

function formatDeltaTable(rows: readonly DeltaRow[]): string {
  const displayRows = rows.map((row) => ({
    scenario: row.scenarioId,
    engine: row.engineId,
    mean: `${row.meanBase.toFixed(2)} -> ${row.meanCurrent.toFixed(2)}`,
    deltaMeanMs: formatSigned(row.deltaMeanMs),
    deltaMeanPct: formatSignedPercent(row.deltaMeanPct),
    p95: `${row.p95Base.toFixed(2)} -> ${row.p95Current.toFixed(2)}`,
    deltaP95Ms: formatSigned(row.deltaP95Ms),
    deltaP95Pct: formatSignedPercent(row.deltaP95Pct),
    speedup: formatSpeedup(row.meanSpeedup),
  }));

  return formatTable(displayRows, [
    { header: "Scenario", value: (row) => row.scenario },
    { header: "Engine", value: (row) => row.engine },
    { header: "Mean (base -> curr)", value: (row) => row.mean },
    { header: "ΔMean ms", align: "right", value: (row) => row.deltaMeanMs },
    { header: "ΔMean %", align: "right", value: (row) => row.deltaMeanPct },
    { header: "P95 (base -> curr)", value: (row) => row.p95 },
    { header: "ΔP95 ms", align: "right", value: (row) => row.deltaP95Ms },
    { header: "ΔP95 %", align: "right", value: (row) => row.deltaP95Pct },
    { header: "Mean speedup", align: "right", value: (row) => row.speedup },
  ]);
}

async function readReportFromPath(path: string): Promise<BenchmarkReport> {
  const raw = await readFile(path, "utf8");
  return parseBenchmarkReport(raw, path);
}

async function resolveCurrentReport(
  options: CompareCliOptions,
): Promise<{ report: BenchmarkReport; smokeFailures: string[] }> {
  if (options.currentPath) {
    return {
      report: await readReportFromPath(options.currentPath),
      smokeFailures: [],
    };
  }

  const smokeResult = await runBenchmarkSmoke({
    reportMetadata: options.wasmProfile
      ? { wasmProfile: options.wasmProfile }
      : undefined,
  });
  if (options.outCurrentPath) {
    await writeFile(
      options.outCurrentPath,
      toBenchmarkReportJson(smokeResult.report),
      "utf8",
    );
  }
  return {
    report: smokeResult.report,
    smokeFailures: smokeResult.failures,
  };
}

async function main(
  args: readonly string[] = process.argv.slice(2),
): Promise<void> {
  try {
    const options = parseCliOptions(args);
    const baselineReport = await readReportFromPath(options.baselinePath);
    const currentResult = await resolveCurrentReport(options);
    const currentReport = currentResult.report;
    const profileCompatibility = evaluateWasmProfileCompatibility(
      baselineReport,
      currentReport,
    );

    const baselineRows = createSummaryRows(baselineReport);
    const currentRows = createSummaryRows(currentReport);
    const deltaResult = buildDeltaRows(baselineRows, currentRows);

    console.log("Current benchmark metrics:");
    console.log(formatCurrentSummaryTable(currentReport));
    console.log("");
    console.log("Baseline delta metrics:");
    console.log(formatDeltaTable(deltaResult.deltas));
    console.log("");

    if (options.outCurrentPath && !options.currentPath) {
      console.log(
        `Wrote current benchmark report JSON: ${options.outCurrentPath}`,
      );
      console.log("");
    }

    if (profileCompatibility.warning) {
      console.warn(
        `Profile compatibility warning: ${profileCompatibility.warning}`,
      );
      console.log("");
    }

    const failures: string[] = [];
    if (profileCompatibility.issue) {
      failures.push(profileCompatibility.issue);
    }
    failures.push(
      ...currentResult.smokeFailures.map((failure) => `smoke: ${failure}`),
    );
    failures.push(
      ...deltaResult.missingFromBaseline.map(
        (key) => `missing baseline row: ${key}`,
      ),
    );
    failures.push(
      ...deltaResult.missingFromCurrent.map(
        (key) => `missing current row: ${key}`,
      ),
    );

    if (options.maxRegressionPct !== undefined) {
      for (const row of deltaResult.deltas) {
        const exceeded =
          (row.deltaMeanPct !== null &&
            row.deltaMeanPct > options.maxRegressionPct) ||
          (row.deltaP95Pct !== null &&
            row.deltaP95Pct > options.maxRegressionPct);
        if (exceeded) {
          failures.push(
            `regression>${options.maxRegressionPct.toFixed(2)}%: ${row.scenarioId}/${row.engineId} (Δmean=${formatSignedPercent(
              row.deltaMeanPct,
            )}, Δp95=${formatSignedPercent(row.deltaP95Pct)})`,
          );
        }
      }
    }

    if (failures.length > 0) {
      console.error("Benchmark comparison found issues:");
      for (const failure of failures) {
        console.error(`- ${failure}`);
      }
      process.exitCode = 1;
      return;
    }

    console.log("Benchmark comparison completed without issues.");
  } catch (error) {
    console.error(`Benchmark comparison failed: ${toMessage(error)}`);
    process.exitCode = 1;
  }
}

const entryUrl = process.argv[1] ? pathToFileURL(process.argv[1]).href : "";
if (import.meta.url === entryUrl) {
  await main();
}
