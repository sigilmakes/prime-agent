export type BenchmarkScenarioComplexity = "small" | "medium" | "large";

export interface BenchmarkScenario {
  id: string;
  name: string;
  complexity: BenchmarkScenarioComplexity;
  description: string;
  input: string;
}

export const BENCHMARK_SCENARIOS: readonly BenchmarkScenario[] = [
  {
    id: "flowchart-small",
    name: "Flowchart Small",
    complexity: "small",
    description: "5 nodes with a single decision branch.",
    input: `graph TD
A[Start] --> B{Decision}
B -->|Yes| C[Process]
B -->|No| D[Error]
C --> E[End]`,
  },
  {
    id: "flowchart-medium",
    name: "Flowchart Medium",
    complexity: "medium",
    description:
      "Multi-step service pipeline with fan-out, fan-in, and retries.",
    input: `graph LR
Client[Client] --> Gateway[API Gateway]
Gateway --> Auth[Auth]
Gateway --> Cache[Cache]
Auth --> Router{Route}
Cache --> Router
Router --> ServiceA[Service A]
Router --> ServiceB[Service B]
ServiceA --> Queue[(Queue)]
ServiceB --> Queue
Queue --> WorkerA[Worker A]
Queue --> WorkerB[Worker B]
WorkerA --> DB[(Database)]
WorkerB --> DB
DB --> Notify[Notifier]
Notify --> Client
ServiceB -. timeout .-> Retry[Retry Policy]
Retry --> Router`,
  },
  {
    id: "flowchart-large",
    name: "Flowchart Large",
    complexity: "large",
    description:
      "Broad CI delivery pipeline with parallel stages and rollback loops.",
    input: `graph TD
Start[Start] --> Plan[Plan]
Plan --> BuildA[Build Linux]
Plan --> BuildB[Build macOS]
Plan --> BuildC[Build Windows]
BuildA --> TestA[Test Unit]
BuildB --> TestB[Test Integration]
BuildC --> TestC[Test E2E]
TestA --> Lint[Lint]
TestB --> Lint
TestC --> Lint
Lint --> Package[Package]
Package --> PublishA[Publish CLI]
Package --> PublishB[Publish Wasm]
PublishA --> SmokeA[Smoke CLI]
PublishB --> SmokeB[Smoke Web]
SmokeA --> Verify{Release Gate}
SmokeB --> Verify
Verify -->|pass| Announce[Announce Release]
Verify -->|fail| Rollback[Rollback]
Rollback --> Fix[Fix Forward]
Fix --> BuildA
Announce --> Docs[Update Docs]
Docs --> Metrics[Collect Metrics]
Metrics --> End[End]
subgraph "Canary Checks"
  CanaryA[Canary US] --> CanaryB[Canary EU]
  CanaryB --> CanaryC[Canary APAC]
end
PublishB --> CanaryA
CanaryC --> Verify`,
  },
];
