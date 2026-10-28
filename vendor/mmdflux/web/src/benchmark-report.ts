import type {
  BenchmarkScenario,
  BenchmarkScenarioComplexity,
} from "./benchmarks/scenarios";

export const WASM_BUILD_PROFILES = ["dev", "release"] as const;

export type WasmBuildProfile = (typeof WASM_BUILD_PROFILES)[number];

export interface BenchmarkReportMetadata {
  wasmProfile?: WasmBuildProfile;
}

export interface BenchmarkMetrics {
  sampleCount: number;
  minMs: number;
  maxMs: number;
  meanMs: number;
  medianMs: number;
  p95Ms: number;
}

export interface BenchmarkEngineInput {
  engineId: string;
  engineLabel: string;
  samplesMs: number[];
}

export interface BenchmarkScenarioInput {
  scenario: BenchmarkScenario;
  engines: BenchmarkEngineInput[];
}

export interface CreateBenchmarkReportInput {
  generatedAt?: string;
  metadata?: BenchmarkReportMetadata;
  warmupIterations: number;
  measurementIterations: number;
  scenarios: BenchmarkScenarioInput[];
}

export interface BenchmarkEngineReport {
  engineId: string;
  engineLabel: string;
  samplesMs: number[];
  metrics: BenchmarkMetrics;
}

export interface BenchmarkScenarioReport {
  scenarioId: string;
  scenarioName: string;
  complexity: BenchmarkScenarioComplexity;
  description: string;
  engines: BenchmarkEngineReport[];
}

export interface BenchmarkReport {
  schemaVersion: 1;
  generatedAt: string;
  metadata?: BenchmarkReportMetadata;
  warmupIterations: number;
  measurementIterations: number;
  scenarios: BenchmarkScenarioReport[];
}

export interface BenchmarkSummaryRow {
  scenarioId: string;
  scenarioName: string;
  complexity: BenchmarkScenarioComplexity;
  engineId: string;
  engineLabel: string;
  meanMs: number;
  medianMs: number;
  p95Ms: number;
  minMs: number;
  maxMs: number;
}

function roundTo(value: number, precision = 4): number {
  const multiplier = 10 ** precision;
  return Math.round(value * multiplier) / multiplier;
}

function sortedSamples(samplesMs: number[]): number[] {
  return [...samplesMs].sort((left, right) => left - right);
}

function percentileFromSorted(samplesMs: number[], percentile: number): number {
  if (samplesMs.length === 0) {
    return 0;
  }

  const rank = Math.ceil((percentile / 100) * samplesMs.length) - 1;
  const index = Math.min(Math.max(rank, 0), samplesMs.length - 1);
  return samplesMs[index];
}

function medianFromSorted(samplesMs: number[]): number {
  if (samplesMs.length === 0) {
    return 0;
  }

  const middle = Math.floor(samplesMs.length / 2);
  if (samplesMs.length % 2 === 1) {
    return samplesMs[middle];
  }

  return (samplesMs[middle - 1] + samplesMs[middle]) / 2;
}

export function summarizeSamples(samplesMs: number[]): BenchmarkMetrics {
  const sorted = sortedSamples(samplesMs);
  const total = sorted.reduce((sum, sample) => sum + sample, 0);
  const mean = sorted.length === 0 ? 0 : total / sorted.length;

  return {
    sampleCount: sorted.length,
    minMs: sorted[0] ?? 0,
    maxMs: sorted.at(-1) ?? 0,
    meanMs: roundTo(mean),
    medianMs: roundTo(medianFromSorted(sorted)),
    p95Ms: roundTo(percentileFromSorted(sorted, 95)),
  };
}

export function isWasmBuildProfile(value: unknown): value is WasmBuildProfile {
  return value === "dev" || value === "release";
}

export function createBenchmarkReport(
  input: CreateBenchmarkReportInput,
): BenchmarkReport {
  return {
    schemaVersion: 1,
    generatedAt: input.generatedAt ?? new Date().toISOString(),
    ...(input.metadata ? { metadata: { ...input.metadata } } : {}),
    warmupIterations: input.warmupIterations,
    measurementIterations: input.measurementIterations,
    scenarios: input.scenarios.map((scenarioInput) => ({
      scenarioId: scenarioInput.scenario.id,
      scenarioName: scenarioInput.scenario.name,
      complexity: scenarioInput.scenario.complexity,
      description: scenarioInput.scenario.description,
      engines: scenarioInput.engines.map((engineInput) => ({
        engineId: engineInput.engineId,
        engineLabel: engineInput.engineLabel,
        samplesMs: [...engineInput.samplesMs],
        metrics: summarizeSamples(engineInput.samplesMs),
      })),
    })),
  };
}

export function createSummaryRows(
  report: BenchmarkReport,
): BenchmarkSummaryRow[] {
  return report.scenarios.flatMap((scenario) =>
    scenario.engines.map((engine) => ({
      scenarioId: scenario.scenarioId,
      scenarioName: scenario.scenarioName,
      complexity: scenario.complexity,
      engineId: engine.engineId,
      engineLabel: engine.engineLabel,
      meanMs: engine.metrics.meanMs,
      medianMs: engine.metrics.medianMs,
      p95Ms: engine.metrics.p95Ms,
      minMs: engine.metrics.minMs,
      maxMs: engine.metrics.maxMs,
    })),
  );
}

export function toBenchmarkReportJson(report: BenchmarkReport): string {
  return JSON.stringify(report, null, 2);
}
