# Mmdflux 2.6.1 titled-empty-subgraph layout fix

Pinned source of crates.io `mmdflux` 2.6.1 (upstream license, from the
`mmdflux-v2.6.1` GitHub tag; the published tarball excludes `tests/`, which
`src/internal_tests` include, so the test fixtures come from the same tag).

The only upstream source change is
`src/engines/graph/algorithms/layered/kernel/graph.rs` (plus its regression
test). See `PRIME_PATCH.patch`.

## The bug

`LayoutGraph::from_digraph` derives `compound_nodes` only from the parent map:
a node is a compound exactly when somebody lists it as a parent. A titled
subgraph with no member nodes (`subgraph ONWK["on the workers"] end`) is added
as a node and registered in `nodes_with_title` -> `compound_titles`, but it is
nobody's parent, so it never joins `compound_nodes` and the nesting phase
(`run_with_policy`) never creates its `border_top`/`border_bottom`.

`insert_title_nodes` then walks `compound_titles` and reads
`lg.border_top[&compound_idx]` - a HashMap index, which panics with
`no entry found for key` (`nesting.rs:269`). Any flowchart that reaches the
layered engine (it must contain edges) and carries one titled empty subgraph
kills the render - in the daemon TUI this is a process-wide panic when the
assistant draws such a diagram, taking the whole UI down with it.

Verified against cargo version 2.6.1 with an isolated test harness and an
RUST_BACKTRACE run; no upstream change addresses it (last `nesting.rs` commit
2026-04-24, latest release 2026-08-09).

## The fix

`from_digraph` keeps titled subgraphs without children inside
`compound_nodes`, restoring the invariant `compound_titles` must satisfy.
Such a subgraph then goes through the ordinary compound pipeline: border
top/bottom nodes are created, the title node is inserted, and layout proceeds.
An empty compound has no member ranks, so it contributes no subgraph bounds -
the text render draws the rest of the diagram and simply omits the empty box,
and the TUI's existing `aligned()`/empty-art fallback shows the source for
that block. Unlabeled empty subgraphs are unaffected (never in
`compound_titles`, no title pass, no border pass).

## Test evidence

- `cargo test` on the patched crate: 2434 + 26 + 9 tests pass (was 2425
  passing before; +1 is the added regression, which panics without the fix).
- The regression `test_layout_titled_empty_subgraph_end_to_end` reproduces
  the panic shape (titled empty subgraph + sibling compound with edges) and
  asserts the invariant plus completed layout.
- Upstream `dagre_parity_tests` and `regression_tests` batteries unchanged
  and green.

Prefer upstreaming this fix to kevinswiber/mmdflux with the same regression
test; this vendor patch stands until a release carries it.

## Vendored wiring notes

- `Cargo.toml` gets the bare `[workspace]` escape marker appended: the vendored
  crate sits inside this repository's workspace tree, and the marker lets its
  own suite (`cargo test` from the vendor directory) run as its separate unit
  instead of erroring against the parent workspace it is not a member of. No
  product wiring change; the workspace builds it through
  `[patch.crates-io] mmdflux = { path = "vendor/mmdflux" }` in the root
  `Cargo.toml`, and `Cargo.lock` records the path source.
- `tests/` comes from the upstream `mmdflux-v2.6.1` tag because the crates.io
  tarball excludes it while `src/internal_tests` compile it with
  `include_str!`; the published crate's test battery cannot compile without it.
