# drawio-agent-xml-core

Draw.io XML model: parse, query, mutate, validate, and serialize `mxfile` / `mxGraphModel`.

## Status

Phase 1 — TDD in progress.

## Test layout

- `tests/parse.rs` — parse behavior
- `tests/query.rs` — query by id, neighbors, subgraph extraction
- `tests/mutate.rs` — subgraph apply, structural update
- `tests/validate.rs` — schema validation
- `tests/roundtrip.rs` — parse → serialize → parse equivalence
