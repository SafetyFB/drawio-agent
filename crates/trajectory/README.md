# drawio-agent-trajectory

Per-session execution log (DeepSeek Harness style): every LLM call,
every render, every state transition is recorded so the user (or a
debugger) can replay the agent's reasoning after the fact.

## Status

**Phase 5 — scaffold + TDD #1 in progress.**

## Architecture

```
session_id ──► TrajectoryStore (in-memory) ──► Vec<TrajectoryEvent>
                       │
                       ├── record(event)
                       ├── list(session_id) -> Vec<TrajectoryEvent>
                       ├── usage(session_id) -> UsageSummary
                       └── export_json(session_id) -> String
```

`TrajectoryEvent` is a serde-tagged enum so events round-trip cleanly
through JSON for export.

## Test layout

- `tests/events.rs` — TDD #1: record + list + filter by session
- `tests/usage.rs` — TDD #2: usage aggregation
- `tests/export.rs` — TDD #3: JSON export round-trip
