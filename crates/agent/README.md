# drawio-agent-agent

Agent Loop state machine: Generate → Render → Review → Patch, iterating
until the visual reviewer reports `verdict: "pass"` or `max_iterations`
is reached.

## Status

Phase 8 — scaffold + TDD in progress.

## Architecture

```
┌─ AgentLoop::run(loop, deps)
│
│  iteration = 0
│  loop:
│    deps.generate(prompt)        → new XML
│    deps.render(xml)              → PNG
│    deps.review(xml, png)         → ReviewResponse
│    if verdict == "pass":         → Done
│    elif issues found:            deps.patch(xml, issues) → new XML
│    elif iteration >= max:        → Failed
│    else:                         continue
│
└─ AgentOutcome { final_xml, iterations, trajectory }
```

The state machine is decoupled from concrete implementations via the
[`AgentDeps`] trait, so tests can drive it with stub LLM/render/review
outputs.

## Test layout

- `tests/loop.rs` — state machine flow tests (pass / patch-and-retry /
  give-up / initial-xml path)
