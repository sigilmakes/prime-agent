#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");

// Dagre root: use DAGRE_ROOT env var, or default to deps/dagre relative to repo root
const dagreRoot = process.env.DAGRE_ROOT || path.join(__dirname, "..", "deps", "dagre");
if (!fs.existsSync(dagreRoot)) {
  console.error(`Error: Dagre not found at ${dagreRoot}`);
  console.error("Run ./scripts/setup-debug-deps.sh to set up dependencies,");
  console.error("or set DAGRE_ROOT environment variable to your dagre checkout.");
  process.exit(1);
}

if (process.argv.length < 3) {
  console.error("Usage: dump-dagre-pipeline.js <input.json>");
  process.exit(1);
}

const inputPath = process.argv[2];
const data = JSON.parse(fs.readFileSync(inputPath, "utf8"));
let Graph;
try {
  // dagre >= 1.x (scoped graphlib)
  Graph = require(path.join(dagreRoot, "node_modules", "@dagrejs", "graphlib")).Graph;
} catch (err) {
  // dagre 0.8.x
  Graph = require(path.join(dagreRoot, "node_modules", "graphlib")).Graph;
}
const acyclic = require(path.join(dagreRoot, "lib", "acyclic"));
const normalize = require(path.join(dagreRoot, "lib", "normalize"));
const rank = require(path.join(dagreRoot, "lib", "rank"));
const util = require(path.join(dagreRoot, "lib", "util"));
const parentDummyChains = require(path.join(dagreRoot, "lib", "parent-dummy-chains"));
const nestingGraph = require(path.join(dagreRoot, "lib", "nesting-graph"));
const addBorderSegments = require(path.join(dagreRoot, "lib", "add-border-segments"));
const order = require(path.join(dagreRoot, "lib", "order"));

const g = new Graph({ multigraph: true, compound: true });

const graphAttrs = {
  rankdir: data.graph.rankdir || "TB",
  nodesep: data.graph.nodesep ?? 50,
  edgesep: data.graph.edgesep ?? 20,
  ranksep: data.graph.ranksep ?? 50,
  ranker: data.graph.ranker || "network-simplex",
  rankalign: "center",
};

g.setGraph(graphAttrs);

g.setDefaultEdgeLabel(() => ({}));

for (const node of data.nodes) {
  g.setNode(node.id, {
    label: node.label,
    width: node.width,
    height: node.height,
  });
}

for (const node of data.nodes) {
  if (node.parent) {
    g.setParent(node.id, node.parent);
  }
}

for (const edge of data.edges) {
  const label = {
    weight: 1,
    minlen: 1,
    width: 0,
    height: 0,
    labeloffset: 10,
    labelpos: "r",
  };
  if (edge.label) {
    label.label = edge.label;
    label.width = edge.label_width;
    label.height = edge.label_height;
  }
  g.setEdge(edge.from, edge.to, label, String(edge.index));
}

function makeSpaceForEdgeLabels(graph) {
  const graphLabel = graph.graph();
  graphLabel.ranksep /= 2;
  graph.edges().forEach((e) => {
    const edge = graph.edge(e);
    edge.minlen *= 2;
    if (edge.labelpos.toLowerCase() !== "c") {
      if (graphLabel.rankdir === "TB" || graphLabel.rankdir === "BT") {
        edge.width += edge.labeloffset;
      } else {
        edge.height += edge.labeloffset;
      }
    }
  });
}

function assignRankMinMax(graph) {
  let maxRank = 0;
  graph.nodes().forEach((v) => {
    const node = graph.node(v);
    if (node.borderTop) {
      node.minRank = graph.node(node.borderTop).rank;
      node.maxRank = graph.node(node.borderBottom).rank;
      maxRank = Math.max(maxRank, node.maxRank);
    }
  });
  graph.graph().maxRank = maxRank;
}

function dumpStage(graph, stage) {
  const records = [];
  for (const v of graph.nodes()) {
    const node = graph.node(v);
    if (node.rank === undefined) {
      continue;
    }
    const parent = graph.parent(v) || null;
    const dummy = node.dummy || null;
    const dummyEdge = node.edgeObj ? node.edgeObj.name : null;
    const border = node.borderType === "borderLeft"
      ? "left"
      : node.borderType === "borderRight"
        ? "right"
        : null;
    const isCompound = graph.children(v).length > 0;
    const isPosition = !isCompound;
    records.push({
      stage,
      id: v,
      rank: node.rank,
      order: node.order ?? 0,
      parent,
      dummy,
      dummy_edge: dummyEdge,
      border,
      is_position: isPosition,
      is_compound: isCompound,
      is_excluded: false,
    });
  }

  records.sort((a, b) => {
    if (a.rank !== b.rank) return a.rank - b.rank;
    if (a.order !== b.order) return a.order - b.order;
    return a.id.localeCompare(b.id);
  });

  for (const rec of records) {
    process.stdout.write(`${JSON.stringify(rec)}\n`);
  }
}

// --- Pipeline (mirrors dagre runLayout up to order) ---
makeSpaceForEdgeLabels(g);
acyclic.run(g);
nestingGraph.run(g);
rank(util.asNonCompoundGraph(g));
dumpStage(g, "after_rank");
util.removeEmptyRanks(g);
dumpStage(g, "after_remove_empty_ranks");
nestingGraph.cleanup(g);
dumpStage(g, "after_nesting_cleanup");
util.normalizeRanks(g);
dumpStage(g, "after_rank_normalize");
assignRankMinMax(g);
dumpStage(g, "after_rank_minmax");
// Edge label proxies skipped (no labeled edges in target fixtures)
normalize.run(g);
dumpStage(g, "after_normalize");
parentDummyChains(g);
dumpStage(g, "after_parent_dummy_chains");
addBorderSegments(g);
dumpStage(g, "after_border_segments");
order(g, {});
dumpStage(g, "after_order");
