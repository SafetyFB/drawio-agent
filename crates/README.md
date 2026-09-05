# crates

This directory holds the workspace member crates.

| Crate | Purpose | Status |
|---|---|---|
| `xml-core` | Draw.io XML model: parse, query, mutate, validate, serialize | Phase 1 |
| `llm-client` | LLM provider abstraction + OpenAI-compat implementation | Phase 2 |
| `renderer` | Headless Chromium worker for Draw.io rendering | Phase 3 |
| `agent` | Agent Loop state machine | Phase 4 |
| `server` | Axum HTTP/WS service | Phase 5 |
