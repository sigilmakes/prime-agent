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
  console.error("Usage: dump-dagre-order.js <input.json>");
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

// --- Pipeline (mirrors dagre runLayout up to order) ---
makeSpaceForEdgeLabels(g);
acyclic.run(g);
nestingGraph.run(g);
rank(util.asNonCompoundGraph(g));
// Edge label proxies skipped (no labeled edges in target fixtures)
util.removeEmptyRanks(g);
nestingGraph.cleanup(g);
util.normalizeRanks(g);
assignRankMinMax(g);
// removeEdgeLabelProxies skipped
normalize.run(g);
parentDummyChains(g);
addBorderSegments(g);
order(g, {});

const byRank = new Map();
for (const v of g.nodes()) {
  const node = g.node(v);
  if (node.rank === undefined) {
    continue;
  }
  const rankVal = node.rank;
  if (!byRank.has(rankVal)) {
    byRank.set(rankVal, []);
  }
  byRank.get(rankVal).push({
    id: v,
    order: node.order,
    dummy: node.dummy,
    borderTop: node.borderTop,
    borderBottom: node.borderBottom,
  });
}

const ranks = Array.from(byRank.keys()).sort((a, b) => a - b);
for (const rankVal of ranks) {
  const layer = byRank.get(rankVal);
  layer.sort((a, b) => a.order - b.order);
  const items = layer.map((item) => {
    const tags = [];
    if (item.dummy) tags.push(`dummy:${item.dummy}`);
    if (item.borderTop) tags.push(`bt:${item.borderTop}`);
    if (item.borderBottom) tags.push(`bb:${item.borderBottom}`);
    const tagStr = tags.length ? `(${tags.join(";")})` : "";
    return `${item.id}${tagStr}=${item.order}`;
  });
  console.log(`rank ${rankVal}: [${items.join(", ")}]`);
}
