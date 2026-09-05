# drawio-agent-llm-client

LLM provider abstraction with:
- `LlmProvider` trait — pluggable backend (text generation + visual review)
- `OpenAiCompatProvider` — concrete impl covering GLM / Qwen-VL / OpenAI / etc.
- `HttpTransport` trait — abstraction for HTTP calls (mockable in tests)
- `Usage` / `PriceBook` / `SessionUsage` — token counting and cost calculation
- Prompt templates for codegen, review, and patch

## Status

Phase 2 — TDD in progress.

## Test layout

- `tests/usage.rs` — `Usage`, `PriceBook`, `SessionUsage`
- `tests/provider.rs` — `LlmProvider` trait + `OpenAiCompatProvider` via mock transport
- `tests/prompt.rs` — prompt template rendering
