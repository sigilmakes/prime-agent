#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CorpusTier {
    TierA,
    TierB,
    Synthetic,
    Excluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiagramFamily {
    Flowchart,
    Class,
    TimelineExcluded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityPolicy {
    /// Current MVP policy. Future semantic-diff work may add an inferred
    /// rename diagnostic mode, but node/subgraph IDs remain canonical here.
    IdsAreCanonical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticChangeKind {
    NodeAdded,
    NodeRemoved,
    EdgeAdded,
    EdgeRemoved,
    /// Hand-authored high-level event for M01. Metric collectors still treat
    /// canonical edge identity changes as removed/added unless a later diff
    /// layer proves a stable edge correspondence.
    EdgeSplit,
    NodeLabelChanged,
    EdgeLabelChanged,
    EdgeStyleChanged,
    SubgraphAdded,
    SubgraphMembershipChanged,
    SubgraphDirectionChanged,
    CycleResolved,
    ClassNodeAdded,
    ClassEdgeAdded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationInput {
    Fixture {
        family: &'static str,
        name: &'static str,
    },
    Inline(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MutationPair {
    pub(crate) id: &'static str,
    pub(crate) family: DiagramFamily,
    pub(crate) tier: CorpusTier,
    pub(crate) base: MutationInput,
    pub(crate) mutated: MutationInput,
    pub(crate) expected_changes: &'static [SemanticChangeKind],
    pub(crate) direct_nodes: &'static [&'static str],
    pub(crate) direct_edges: &'static [&'static str],
    pub(crate) include_in_graph_stability_metrics: bool,
    pub(crate) identity_policy: IdentityPolicy,
}

const TIER_A_PAIRS: &[MutationPair] = &[
    MutationPair {
        id: "M01",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("chain.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Step 1] --> B[Step 2] --> X[Inserted] --> C[Step 3] --> D[Step 4]\n",
        ),
        expected_changes: &[
            SemanticChangeKind::NodeAdded,
            SemanticChangeKind::EdgeAdded,
            SemanticChangeKind::EdgeRemoved,
            SemanticChangeKind::EdgeSplit,
        ],
        direct_nodes: &["X"],
        direct_edges: &["e1", "e2"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M02",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("fan_out.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Source] --> B[Target A]\n    A --> C[Target B]\n    A --> D[Target C]\n    A --> E[Target D]\n",
        ),
        expected_changes: &[SemanticChangeKind::NodeAdded, SemanticChangeKind::EdgeAdded],
        direct_nodes: &["E"],
        direct_edges: &["e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M03",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("fan_in.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Source A] --> D[Target]\n    B[Source B] --> D\n    C[Source C] --> D\n    E[Source D] --> D\n",
        ),
        expected_changes: &[SemanticChangeKind::NodeAdded, SemanticChangeKind::EdgeAdded],
        direct_nodes: &["E"],
        direct_edges: &["e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M04",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("chain.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Step 1] --> B[Step 2] --> C[Step 3] --> D[Step 4]\n    A --> D\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeAdded],
        direct_nodes: &[],
        direct_edges: &["e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M05",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("ci_pipeline.mmd"),
        mutated: MutationInput::Inline(
            "graph LR\n    Push[Git Push] --> Build[Build]\n    Build --> Test[Run Tests]\n    Test --> Lint[Static Analysis]\n    Lint --> Deploy{Deploy?}\n    Deploy -->|staging| Staging[Staging Env]\n    Deploy -->|production| Prod[Production]\n",
        ),
        expected_changes: &[SemanticChangeKind::NodeLabelChanged],
        direct_nodes: &["Lint"],
        direct_edges: &["e2", "e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M06",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: MutationInput::Inline("graph TD\n    B[Step 2] --> C[Step 3]\n"),
        mutated: MutationInput::Inline("graph TD\n    B[Step 2] -->|validate| C[Step 3]\n"),
        expected_changes: &[SemanticChangeKind::EdgeLabelChanged],
        direct_nodes: &[],
        direct_edges: &["e0"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M07",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("edge_styles.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Solid] -.-> B[Normal]\n    C[Dotted] -.-> D[Arrow]\n    E[Thick] ==> F[Arrow]\n    G[Open] --- H[Line]\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeStyleChanged],
        direct_nodes: &[],
        direct_edges: &["e0"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M08",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("ci_pipeline.mmd"),
        mutated: MutationInput::Inline(
            "graph LR\n    Push[Git Push] --> Build[Build]\n    subgraph checks [Checks]\n        Build --> Test[Run Tests]\n        Test --> Lint[Lint Check]\n    end\n    Lint --> Deploy{Deploy?}\n    Deploy -->|staging| Staging[Staging Env]\n    Deploy -->|production| Prod[Production]\n",
        ),
        expected_changes: &[
            SemanticChangeKind::SubgraphAdded,
            SemanticChangeKind::SubgraphMembershipChanged,
        ],
        direct_nodes: &["Build", "Test", "Lint"],
        direct_edges: &["e0", "e1", "e2", "e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M09",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("system-architecture.mmd"),
        mutated: MutationInput::Inline(
            "graph LR\n  subgraph clients [Client Layer]\n    A([Web App])\n    C([Mobile App])\n  end\n  subgraph services [Service Layer]\n    B[API Gateway]\n    A --> B\n    C --> B\n    B --> D[Auth Service]\n    B --> E[User Service]\n    B --> F[Order Service]\n  end\n  subgraph data [Data Layer]\n    D --> G[(Auth DB)]\n    E --> H[(User DB)]\n    F --> I[(Order DB)]\n    F --> J([Message Queue])\n  end\n",
        ),
        expected_changes: &[SemanticChangeKind::SubgraphMembershipChanged],
        direct_nodes: &["B"],
        direct_edges: &["e0", "e1", "e2", "e3", "e4"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M10",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("subgraph_direction_lr.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    Start --> A\n    subgraph sg1[Horizontal Flow]\n        direction TB\n        A[Step 1] --> B[Step 2] --> C[Step 3]\n    end\n    C --> End\n",
        ),
        expected_changes: &[SemanticChangeKind::SubgraphDirectionChanged],
        direct_nodes: &["A", "B", "C"],
        direct_edges: &["e1", "e2", "e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M11",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("subgraph_direction_cross_boundary.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    subgraph sg1[Horizontal Section]\n        direction LR\n        A --> B\n    end\n    C --> E\n    E --> A\n    C --> A\n    B --> F\n    F --> D\n    B --> D\n    A --> D\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeAdded],
        direct_nodes: &[],
        direct_edges: &["e7"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M12",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("simple_cycle.mmd"),
        mutated: MutationInput::Inline("graph TD\n    A[Start] --> B[Process]\n    B --> C[End]\n"),
        expected_changes: &[
            SemanticChangeKind::EdgeRemoved,
            SemanticChangeKind::CycleResolved,
        ],
        direct_nodes: &[],
        direct_edges: &["e2"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M14",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("inline_label_flowchart.mmd"),
        mutated: MutationInput::Inline(
            "flowchart TD\n  start((Start)) --> ingest[Ingest Request]\n  ingest --> parse[Parse Payload]\n  parse --> validate{Valid?}\n\n  validate -- no --> reject[Reject]\n  reject -.-> notify[Notify User]\n  reject --> metrics[Emit Metrics]\n\n  validate -- yes --> route{Route Type}\n  route -- sync --> sync[Sync Pipeline]\n  route -- async --> queue[Enqueue Job]\n\n  queue --> worker[Worker Pool]\n  worker --> process[Process Job]\n  process --> success{Success?}\n\n  success -- retry later --> retry[Retry]\n  retry ==> queue\n\n  success -- yes --> persist[Persist Result]\n  sync --> persist\n  persist --> metrics\n\n  parse --> cache[Lookup Cache]\n  cache -- hit --> fastpath[Serve Cached]\n  fastpath --> metrics\n  cache -- miss --> validate\n\n  ingest --> audit[Audit Log]\n  audit --> metrics\n\n  process -- warn --> alert[Page On-call]\n  alert -.-> metrics\n\n  metrics --> End((Done))\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeLabelChanged],
        direct_nodes: &[],
        direct_edges: &["e11"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M15",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("nested_subgraph_parallel_labels.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    subgraph outer [Outer region]\n        subgraph inner_a [A region]\n            A1 --> A2\n        end\n        subgraph inner_b [B region]\n            B1 --> B2\n        end\n    end\n    A1 -->|cross edge one| B1\n    A2 -->|cross edge two| B2\n    A1 -->|cross edge three| B2\n",
        ),
        expected_changes: &[
            SemanticChangeKind::EdgeLabelChanged,
            SemanticChangeKind::EdgeAdded,
        ],
        direct_nodes: &[],
        direct_edges: &["e4"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M19",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("fan_out.mmd"),
        mutated: MutationInput::Inline(
            "graph TD\n    A[Source] --> B[Target A]\n    A --> C[Target B]\n    A --> D[Target C]\n    B --> D\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeAdded],
        direct_nodes: &[],
        direct_edges: &["e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M20",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: MutationInput::Inline(
            "graph TD\n    A --> B\n    A --> C\n    B --> D\n    C --> D\n    B --> E\n    C --> E\n    D --> F\n    E --> F\n",
        ),
        mutated: MutationInput::Inline(
            "graph TD\n    A --> B\n    A --> C\n    B --> D\n    C --> D\n    B --> E\n    C --> E\n    D --> F\n    E --> F\n    A --> F\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeAdded],
        direct_nodes: &[],
        direct_edges: &["e8"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M21",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: MutationInput::Inline(
            "graph TD\n    subgraph left [Left]\n        A --> B\n        A --> C\n    end\n    subgraph right [Right]\n        D --> E\n        D --> F\n    end\n    B --> D\n    C --> E\n    E --> G\n    F --> G\n",
        ),
        mutated: MutationInput::Inline(
            "graph TD\n    subgraph left [Left]\n        A --> B\n        A --> C\n    end\n    subgraph right [Right]\n        D --> E\n        D --> F\n    end\n    B --> D\n    C --> E\n    E --> G\n    F --> G\n    C --> F\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeAdded],
        direct_nodes: &[],
        direct_edges: &["e8"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M05-ID",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::TierA,
        base: flowchart_fixture("ci_pipeline.mmd"),
        mutated: MutationInput::Inline(
            "graph LR\n    Push[Git Push] --> Build[Build]\n    Build --> Test[Run Tests]\n    Test --> Audit[Lint Check]\n    Audit --> Deploy{Deploy?}\n    Deploy -->|staging| Staging[Staging Env]\n    Deploy -->|production| Prod[Production]\n",
        ),
        expected_changes: &[
            SemanticChangeKind::NodeRemoved,
            SemanticChangeKind::NodeAdded,
        ],
        direct_nodes: &["Lint", "Audit"],
        direct_edges: &["e2", "e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "S01",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::Synthetic,
        base: MutationInput::Inline("graph TD\n    A --> C\n    B --> C\n"),
        mutated: MutationInput::Inline("graph TD\n    A --> C\n    X --> C\n    B --> C\n"),
        expected_changes: &[SemanticChangeKind::NodeAdded, SemanticChangeKind::EdgeAdded],
        direct_nodes: &["X"],
        direct_edges: &["e1"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "S02",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::Synthetic,
        base: MutationInput::Inline("graph TD\n    A -->|x| B\n"),
        mutated: MutationInput::Inline("graph TD\n    A -->|a deliberately long label| B\n"),
        expected_changes: &[SemanticChangeKind::EdgeLabelChanged],
        direct_nodes: &[],
        direct_edges: &["e0"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "S03",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::Synthetic,
        base: MutationInput::Inline("graph LR\n    A --> B\n    B --> C\n"),
        mutated: MutationInput::Inline("graph LR\n    A --> B\n    B -.-> C\n"),
        expected_changes: &[SemanticChangeKind::EdgeStyleChanged],
        direct_nodes: &[],
        direct_edges: &["e1"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "S04",
        family: DiagramFamily::Flowchart,
        tier: CorpusTier::Synthetic,
        base: MutationInput::Inline(
            "graph TD\n    subgraph sg[Group]\n        A --> B\n    end\n    C --> A\n",
        ),
        mutated: MutationInput::Inline(
            "graph TD\n    subgraph sg[Group]\n        A --> B\n        C --> A\n    end\n",
        ),
        expected_changes: &[SemanticChangeKind::SubgraphMembershipChanged],
        direct_nodes: &["C"],
        direct_edges: &["e1"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
];

const CLASS_CANARIES: &[MutationPair] = &[
    MutationPair {
        id: "M16",
        family: DiagramFamily::Class,
        tier: CorpusTier::TierB,
        base: class_fixture("inheritance_chain.mmd"),
        mutated: MutationInput::Inline(
            "classDiagram\n    class Vehicle\n    class Car\n    class Truck\n    class ElectricCar\n    class ElectricTruck\n    Vehicle <|-- Car\n    Vehicle <|-- Truck\n    Car <|-- ElectricCar\n    Truck <|-- ElectricTruck\n",
        ),
        expected_changes: &[
            SemanticChangeKind::ClassNodeAdded,
            SemanticChangeKind::ClassEdgeAdded,
        ],
        direct_nodes: &["ElectricTruck"],
        direct_edges: &["e3"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
    MutationPair {
        id: "M17",
        family: DiagramFamily::Class,
        tier: CorpusTier::TierB,
        base: class_fixture("cardinality_labels.mmd"),
        mutated: MutationInput::Inline(
            "classDiagram\nUser \"1\" --> \"many\" Session:owns actively\nOrder \"0..1\" --> \"*\" Item:contains\n",
        ),
        expected_changes: &[SemanticChangeKind::EdgeLabelChanged],
        direct_nodes: &[],
        direct_edges: &["e0"],
        include_in_graph_stability_metrics: true,
        identity_policy: IdentityPolicy::IdsAreCanonical,
    },
];

const SEQUENCE_EXCLUSION_CANARY: MutationPair = MutationPair {
    id: "S-SEQ",
    family: DiagramFamily::TimelineExcluded,
    tier: CorpusTier::Excluded,
    base: MutationInput::Fixture {
        family: "sequence",
        name: "simple.mmd",
    },
    mutated: MutationInput::Inline(
        "sequenceDiagram\n    participant A\n    participant B\n    A->>B: Hello\n    B->>A: Hi\n    A->>B: Follow up\n",
    ),
    expected_changes: &[],
    direct_nodes: &[],
    direct_edges: &[],
    include_in_graph_stability_metrics: false,
    identity_policy: IdentityPolicy::IdsAreCanonical,
};

pub(crate) fn tier_a_pairs() -> &'static [MutationPair] {
    TIER_A_PAIRS
}

pub(crate) fn class_canaries() -> &'static [MutationPair] {
    CLASS_CANARIES
}

pub(crate) fn sequence_exclusion_canary() -> &'static MutationPair {
    &SEQUENCE_EXCLUSION_CANARY
}

pub(crate) fn pair_by_id(id: &str) -> Option<&'static MutationPair> {
    TIER_A_PAIRS
        .iter()
        .chain(CLASS_CANARIES)
        .chain([&SEQUENCE_EXCLUSION_CANARY])
        .find(|pair| pair.id == id)
}

const fn flowchart_fixture(name: &'static str) -> MutationInput {
    MutationInput::Fixture {
        family: "flowchart",
        name,
    }
}

const fn class_fixture(name: &'static str) -> MutationInput {
    MutationInput::Fixture {
        family: "class",
        name,
    }
}
