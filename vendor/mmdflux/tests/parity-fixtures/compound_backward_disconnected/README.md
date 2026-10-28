# compound_backward_disconnected parity fixture

This fixture captures the raw Mermaid/Dagre target for
`tests/fixtures/flowchart/compound_backward_disconnected.mmd`.

Generated with Dagre 0.8.5 from `/Users/kevin/src/dagre` and the literal
Mermaid handoff order recorded in research 0071:

```text
C, B, A, a1, a2, b1, b2, c1, c2
```

`scripts/mmds-to-dagre-input.jq` emits sorted nodes, so
`mmdflux-dagre-input.json` was generated from explicit `mermaid-layered` MMDS
and then reordered with `jq` to match Mermaid FlowDB output. Edge `2` remains
literal `c2 -> a2`; it is not reversed for this raw Dagre fixture.

Regeneration recipe:

```bash
mkdir -p tests/parity-fixtures/compound_backward_disconnected
cargo +stable run --quiet -- --format mmds --geometry-level layout \
  --layout-engine mermaid-layered \
  tests/fixtures/flowchart/compound_backward_disconnected.mmd \
  | jq -f scripts/mmds-to-dagre-input.jq \
  | jq '(["C","B","A","a1","a2","b1","b2","c1","c2"] as $order | .graph.marginx = 8 | .graph.marginy = 8 | .nodes |= sort_by(.id as $id | (($order | index($id)) // 999)))' \
  > tests/parity-fixtures/compound_backward_disconnected/mmdflux-dagre-input.json

DAGRE_ROOT=/Users/kevin/src/dagre \
  node scripts/dump-dagre-layout.js \
  tests/parity-fixtures/compound_backward_disconnected/mmdflux-dagre-input.json \
  > tests/parity-fixtures/compound_backward_disconnected/dagre-layout.json
```
