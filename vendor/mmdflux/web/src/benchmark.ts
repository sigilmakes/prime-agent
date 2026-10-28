import {
  type BenchmarkEngineInput,
  type BenchmarkReport,
  createBenchmarkReport,
  createSummaryRows,
  toBenchmarkReportJson,
} from "./benchmark-report";
import {
  type BenchmarkEngineRunner,
  createBenchmarkEngineRunners,
} from "./benchmarks/engine-runners";
import { BENCHMARK_SCENARIOS } from "./benchmarks/scenarios";

export interface BenchmarkAppOptions {
  createRunners?: () => Promise<BenchmarkEngineRunner[]>;
  now?: () => number;
  warmupIterations?: number;
  measurementIterations?: number;
}

const DEFAULT_WARMUP_ITERATIONS = 2;
const DEFAULT_MEASUREMENT_ITERATIONS = 10;

function formatNumber(value: number): string {
  return value.toFixed(2);
}

async function measureRunner(
  runner: BenchmarkEngineRunner,
  input: string,
  now: () => number,
  warmupIterations: number,
  measurementIterations: number,
): Promise<BenchmarkEngineInput> {
  for (let warmup = 0; warmup < warmupIterations; warmup += 1) {
    await runner.warm(input);
  }

  const samplesMs: number[] = [];
  for (
    let measurement = 0;
    measurement < measurementIterations;
    measurement += 1
  ) {
    const startedAt = now();
    await runner.render(input);
    const completedAt = now();
    samplesMs.push(completedAt - startedAt);
  }

  return {
    engineId: runner.id,
    engineLabel: runner.label,
    samplesMs,
  };
}

function toErrorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }

  return String(error);
}

function toSummaryTable(report: BenchmarkReport): string {
  const rows = createSummaryRows(report);
  const header =
    "Scenario | Engine | Mean (ms) | Median (ms) | P95 (ms) | Min (ms) | Max (ms)";
  const separator = "--- | --- | ---: | ---: | ---: | ---: | ---:";

  return [header, separator]
    .concat(
      rows.map(
        (row) =>
          `${row.scenarioName} (${row.complexity}) | ${row.engineLabel} | ${formatNumber(
            row.meanMs,
          )} | ${formatNumber(row.medianMs)} | ${formatNumber(
            row.p95Ms,
          )} | ${formatNumber(row.minMs)} | ${formatNumber(row.maxMs)}`,
      ),
    )
    .join("\n");
}

function downloadBenchmarkReport(report: BenchmarkReport): void {
  const reportJson = toBenchmarkReportJson(report);
  const blob = new Blob([reportJson], { type: "application/json" });
  const objectUrl = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = objectUrl;
  anchor.download = `mmdflux-benchmark-report-${report.generatedAt.replace(
    /[:.]/g,
    "-",
  )}.json`;
  anchor.click();
  URL.revokeObjectURL(objectUrl);
}

export async function renderBenchmarkApp(
  root: HTMLElement,
  options: BenchmarkAppOptions = {},
): Promise<void> {
  root.innerHTML = `
    <main class="playground benchmark-mode">
      <header class="toolbar">
        <h1>mmdflux Benchmark</h1>
        <div class="toolbar-actions">
          <button type="button" class="toolbar-button" data-benchmark-run>Run Benchmark</button>
          <button type="button" class="toolbar-button" data-benchmark-export disabled>Export JSON</button>
        </div>
      </header>
      <section class="workspace">
        <div class="panel">
          <h2>Status</h2>
          <p class="share-status" data-benchmark-status>Loading benchmark engines...</p>
          <ul data-benchmark-runners></ul>
        </div>
        <div class="panel">
          <h2>Latest Run</h2>
          <pre class="preview-output" data-benchmark-output>No benchmark run yet.</pre>
        </div>
      </section>
    </main>
  `;

  const status = root.querySelector<HTMLElement>("[data-benchmark-status]");
  const runnerList = root.querySelector<HTMLUListElement>(
    "[data-benchmark-runners]",
  );
  const output = root.querySelector<HTMLElement>("[data-benchmark-output]");
  const runButton = root.querySelector<HTMLButtonElement>(
    "[data-benchmark-run]",
  );
  const exportButton = root.querySelector<HTMLButtonElement>(
    "[data-benchmark-export]",
  );

  if (!status || !runnerList || !output || !runButton || !exportButton) {
    return;
  }

  const createRunners = options.createRunners ?? createBenchmarkEngineRunners;
  const warmupIterations =
    options.warmupIterations ?? DEFAULT_WARMUP_ITERATIONS;
  const measurementIterations =
    options.measurementIterations ?? DEFAULT_MEASUREMENT_ITERATIONS;
  const now =
    options.now ??
    (() => {
      return performance.now();
    });
  let latestReport: BenchmarkReport | null = null;

  let runners: BenchmarkEngineRunner[];
  try {
    runners = await createRunners();
  } catch (error) {
    status.textContent = `Failed to load benchmark engines: ${toErrorMessage(error)}`;
    runButton.disabled = true;
    return;
  }

  for (const runner of runners) {
    const item = document.createElement("li");
    item.textContent = `${runner.label} (${runner.id})`;
    runnerList.append(item);
  }
  status.textContent = `Benchmark engines ready. ${BENCHMARK_SCENARIOS.length} scenarios, ${measurementIterations} measured runs each.`;

  runButton.addEventListener("click", () => {
    void (async () => {
      runButton.disabled = true;
      exportButton.disabled = true;
      status.textContent = "Running benchmark...";

      try {
        const scenarioResults = [];
        for (const scenario of BENCHMARK_SCENARIOS) {
          status.textContent = `Running ${scenario.name} (${scenario.complexity})...`;

          const engines: BenchmarkEngineInput[] = [];
          for (const runner of runners) {
            engines.push(
              await measureRunner(
                runner,
                scenario.input,
                now,
                warmupIterations,
                measurementIterations,
              ),
            );
          }

          scenarioResults.push({
            scenario,
            engines,
          });
        }

        latestReport = createBenchmarkReport({
          warmupIterations,
          measurementIterations,
          scenarios: scenarioResults,
        });
        output.textContent = toSummaryTable(latestReport);
        status.textContent = "Benchmark run complete.";
        exportButton.disabled = false;
      } catch (error) {
        status.textContent = `Benchmark failed: ${toErrorMessage(error)}`;
      } finally {
        runButton.disabled = false;
      }
    })();
  });

  exportButton.addEventListener("click", () => {
    if (!latestReport) {
      return;
    }

    downloadBenchmarkReport(latestReport);
    status.textContent = "Benchmark report exported as JSON.";
  });
}
