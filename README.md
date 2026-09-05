# drawio-agent

A multimodal AI agent for [Draw.io](https://www.drawio.com/) that combines natural language generation, visual feedback, and localized canvas selection.

See [`initial_draft.md`](./initial_draft.md) for the project description.

## Status

**Phase 1 (in progress)**: foundational `xml-core` crate with TDD.

## Architecture

See the architecture discussions in the commit history. Key components:

- **`crates/xml-core`** — Draw.io XML model (mxfile / mxGraphModel) with parse / query / mutate / validate / serialize
- **`crates/llm-client`** — LLM provider abstraction (planned)
- **`crates/renderer`** — Headless Chromium worker driving `viewer-static.min.js` (planned)
- **`crates/agent`** — Agent Loop state machine (planned)
- **`crates/server`** — Axum HTTP + WebSocket service (planned)

## Development

```bash
cargo test                 # run all tests
cargo test -p drawio-agent-xml-core   # focus on one crate
```
