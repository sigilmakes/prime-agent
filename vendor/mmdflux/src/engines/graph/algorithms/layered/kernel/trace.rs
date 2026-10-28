use std::cell::RefCell;

use super::graph::LayoutGraph;
use super::pipeline::layout_with_trace;
use super::types::DummyType;
use super::{DiGraph, LayoutConfig, NodeId};

thread_local! {
    static ACTIVE_TRACE: RefCell<Option<LayeredPhaseTrace>> = const { RefCell::new(None) };
}

#[derive(Debug, Default, Clone)]
pub(crate) struct LayeredPhaseTrace {
    pub(crate) stages: Vec<TraceStageSnapshot>,
}

impl LayeredPhaseTrace {
    pub(crate) fn push_stage(&mut self, stage: TraceStageSnapshot) {
        self.stages.push(stage);
    }

    pub(crate) fn has_stage(&self, stage: TraceStage) -> bool {
        self.stages.iter().any(|snapshot| snapshot.stage == stage)
    }

    pub(crate) fn stage_names(&self) -> Vec<TraceStage> {
        self.stages.iter().map(|stage| stage.stage).collect()
    }

    pub(crate) fn generated_dummy_ids(&self) -> Vec<String> {
        self.stages
            .iter()
            .flat_map(|stage| stage.dummies.iter().map(|dummy| dummy.generated_id.clone()))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TraceStageSnapshot {
    pub(crate) stage: TraceStage,
    pub(crate) nodes: Vec<TraceNodeSnapshot>,
    pub(crate) dummies: Vec<TraceDummySnapshot>,
    pub(crate) reversed_edges: Vec<usize>,
}

impl TraceStageSnapshot {
    pub(crate) fn empty(stage: TraceStage) -> Self {
        Self {
            stage,
            nodes: Vec::new(),
            dummies: Vec::new(),
            reversed_edges: Vec::new(),
        }
    }

    pub(crate) fn from_layout_graph(stage: TraceStage, graph: &LayoutGraph) -> Self {
        let nodes = graph
            .node_ids
            .iter()
            .enumerate()
            .filter(|(idx, _)| !graph.is_dummy_index(*idx))
            .map(|(idx, id)| TraceNodeSnapshot {
                id: id.0.clone(),
                rank: graph.ranks[idx],
                order: graph.order[idx],
                x: Some(graph.positions[idx].x),
                y: Some(graph.positions[idx].y),
            })
            .collect();

        let mut dummies = Vec::new();
        for chain in &graph.dummy_chains {
            for (chain_position, dummy_id) in chain.dummy_ids.iter().enumerate() {
                let Some(dummy) = graph.dummy_nodes.get(dummy_id) else {
                    continue;
                };
                let Some(&idx) = graph.node_index.get(dummy_id) else {
                    continue;
                };
                let is_label_dummy = chain.label_dummy_index == Some(chain_position);
                dummies.push(TraceDummySnapshot {
                    generated_id: dummy_id.0.clone(),
                    key: DummyTraceKey {
                        edge_index: chain.edge_index,
                        role: DummyTraceRole::from(dummy.dummy_type),
                        chain_position,
                        is_label_dummy,
                    },
                    rank: graph.ranks[idx],
                    order: graph.order[idx],
                });
            }
        }

        Self {
            stage,
            nodes,
            dummies,
            reversed_edges: graph.reversed_edges.iter().copied().collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TraceStage {
    Acyclic,
    Rank,
    Normalize,
    Order,
    Position,
    Route,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TraceNodeSnapshot {
    pub(crate) id: String,
    pub(crate) rank: i32,
    pub(crate) order: usize,
    pub(crate) x: Option<f64>,
    pub(crate) y: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceDummySnapshot {
    pub(crate) generated_id: String,
    pub(crate) key: DummyTraceKey,
    pub(crate) rank: i32,
    pub(crate) order: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DummyTraceKey {
    pub(crate) edge_index: usize,
    pub(crate) role: DummyTraceRole,
    pub(crate) chain_position: usize,
    pub(crate) is_label_dummy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DummyTraceRole {
    Edge,
    EdgeLabel,
}

impl From<DummyType> for DummyTraceRole {
    fn from(value: DummyType) -> Self {
        match value {
            DummyType::Edge => Self::Edge,
            DummyType::EdgeLabel => Self::EdgeLabel,
        }
    }
}

pub(crate) fn begin_capture() {
    ACTIVE_TRACE.with(|trace| {
        *trace.borrow_mut() = Some(LayeredPhaseTrace::default());
    });
}

pub(crate) fn finish_capture() -> LayeredPhaseTrace {
    ACTIVE_TRACE.with(|trace| trace.borrow_mut().take().unwrap_or_default())
}

pub(crate) fn capture_stage(stage: TraceStage, graph: &LayoutGraph) {
    ACTIVE_TRACE.with(|trace| {
        if let Some(active) = trace.borrow_mut().as_mut() {
            active.push_stage(TraceStageSnapshot::from_layout_graph(stage, graph));
        }
    });
}

#[test]
fn dummy_chain_key_ignores_generated_dummy_node_id() {
    let before = TraceDummySnapshot {
        generated_id: "_d4".to_string(),
        key: DummyTraceKey {
            edge_index: 7,
            role: DummyTraceRole::Edge,
            chain_position: 2,
            is_label_dummy: false,
        },
        rank: 3,
        order: 1,
    };
    let after = TraceDummySnapshot {
        generated_id: "_d9".to_string(),
        key: before.key.clone(),
        rank: 3,
        order: 1,
    };

    assert_eq!(before.key, after.key);
    assert_ne!(before.generated_id, after.generated_id);
}

#[test]
fn phase_trace_records_named_stages_in_order() {
    let mut trace = LayeredPhaseTrace::default();
    trace.push_stage(TraceStageSnapshot::empty(TraceStage::Acyclic));
    trace.push_stage(TraceStageSnapshot::empty(TraceStage::Rank));
    trace.push_stage(TraceStageSnapshot::empty(TraceStage::Normalize));

    assert_eq!(
        trace.stage_names(),
        vec![TraceStage::Acyclic, TraceStage::Rank, TraceStage::Normalize]
    );
}

#[test]
fn stage_snapshot_carries_future_diff_payloads() {
    let snapshot = TraceStageSnapshot {
        stage: TraceStage::Order,
        nodes: vec![TraceNodeSnapshot {
            id: "A".to_string(),
            rank: 1,
            order: 0,
            x: Some(10.0),
            y: Some(20.0),
        }],
        dummies: vec![TraceDummySnapshot {
            generated_id: "_ld2".to_string(),
            key: DummyTraceKey {
                edge_index: 4,
                role: DummyTraceRole::EdgeLabel,
                chain_position: 1,
                is_label_dummy: true,
            },
            rank: 2,
            order: 3,
        }],
        reversed_edges: vec![4],
    };

    assert_eq!(snapshot.stage, TraceStage::Order);
    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.dummies.len(), 1);
    assert_eq!(snapshot.reversed_edges, vec![4]);
    assert_eq!([TraceStage::Position, TraceStage::Route].len(), 2);
}

#[test]
fn trace_capture_includes_core_layered_stages() {
    let mut graph = DiGraph::<()>::new();
    graph.add_node(NodeId::from("A"), ());
    graph.add_node(NodeId::from("B"), ());
    graph.add_edge(NodeId::from("A"), NodeId::from("B"));

    let (_layout, trace) =
        layout_with_trace(&graph, &LayoutConfig::default(), |_id, _node| (40.0, 20.0));

    assert!(trace.has_stage(TraceStage::Acyclic));
    assert!(trace.has_stage(TraceStage::Rank));
    assert!(trace.has_stage(TraceStage::Normalize));
    assert!(trace.has_stage(TraceStage::Order));
    assert!(trace.has_stage(TraceStage::Position));
    assert!(trace.has_stage(TraceStage::Route));
}
