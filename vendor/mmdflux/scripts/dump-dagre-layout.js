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
  console.error("Usage: dump-dagre-layout.js <input.json>");
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
const layout = require(path.join(dagreRoot, "lib", "layout"));

const g = new Graph({ multigraph: true, compound: true });

const graphAttrs = {
  rankdir: data.graph.rankdir || "TB",
  nodesep: data.graph.nodesep ?? 50,
  edgesep: data.graph.edgesep ?? 20,
  ranksep: data.graph.ranksep ?? 50,
  ranker: data.graph.ranker || "network-simplex",
  rankalign: "center",
  marginx: data.graph.marginx ?? 0,
  marginy: data.graph.marginy ?? 0,
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

layout(g);

const nodes = [];
for (const v of g.nodes()) {
  const node = g.node(v);
  const isDummy = Boolean(node.dummy) || Boolean(node.borderType);
  if (isDummy) {
    continue;
  }
  const width = node.width ?? 0;
  const height = node.height ?? 0;
  const centerX = node.x ?? 0;
  const centerY = node.y ?? 0;
  const x = centerX - width / 2;
  const y = centerY - height / 2;
  const parent = g.parent(v) || null;
  const isCompound = g.children(v).length > 0;
  nodes.push({
    id: v,
    x,
    y,
    width,
    height,
    center_x: centerX,
    center_y: centerY,
    parent,
    is_compound: isCompound,
  });
}

nodes.sort((a, b) => a.id.localeCompare(b.id));

const edges = [];
for (const e of g.edges()) {
  const edge = g.edge(e);
  const points = (edge.points || []).map((p) => [p.x, p.y]);
  const idx = Number.parseInt(e.name, 10);
  edges.push({
    index: Number.isNaN(idx) ? null : idx,
    name: e.name,
    from: e.v,
    to: e.w,
    points,
  });
}

edges.sort((a, b) => {
  const aIdx = a.index ?? Number.MAX_SAFE_INTEGER;
  const bIdx = b.index ?? Number.MAX_SAFE_INTEGER;
  if (aIdx !== bIdx) return aIdx - bIdx;
  return String(a.name).localeCompare(String(b.name));
});

const subgraphBounds = nodes
  .filter((n) => n.is_compound)
  .map((n) => ({
    id: n.id,
    x: n.x,
    y: n.y,
    width: n.width,
    height: n.height,
  }));

subgraphBounds.sort((a, b) => a.id.localeCompare(b.id));

const graphLabel = g.graph();
const out = {
  graph: {
    rankdir: graphLabel.rankdir,
    nodesep: graphLabel.nodesep,
    edgesep: graphLabel.edgesep,
    ranksep: graphLabel.ranksep,
    ranker: graphLabel.ranker,
    marginx: graphLabel.marginx,
    marginy: graphLabel.marginy,
    width: graphLabel.width,
    height: graphLabel.height,
  },
  nodes,
  edges,
  subgraph_bounds: subgraphBounds,
};

process.stdout.write(`${JSON.stringify(out, null, 2)}\n`);
