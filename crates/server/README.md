# drawio-agent-server

HTTP + WebSocket server that exposes the Draw.io Agent to clients.

## Architecture

```
HTTP request  ──►  axum router  ──►  SessionStore
                                       │
                                       ├── LlmProvider (OpenAiCompatProvider)
                                       ├── Renderer    (RenderDriver trait)
                                       └── xml-core    (parse / extract / apply)
```

All state is in-memory for the Phase 4 skeleton. Persistence is out of
scope.

## API

| Method | Path                              | Purpose                            |
|--------|-----------------------------------|------------------------------------|
| POST   | `/api/sessions`                   | create session, return session_id   |
| GET    | `/api/sessions/:id`               | session info (current XML + meta)   |
| GET    | `/api/sessions/:id/versions`      | version history                     |
| POST   | `/api/sessions/:id/generate`      | generate XML from a prompt          |
| POST   | `/api/sessions/:id/patch`         | patch the selection                 |
| POST   | `/api/sessions/:id/render`        | render current XML to PNG bytes     |
| POST   | `/api/sessions/:id/review`        | visual review via VLM               |
| WS     | `/api/sessions/:id/events`        | live event stream                   |
| GET    | `/health`                         | liveness probe                      |

The renderer uses the Phase-3 `MockDriver` by default; the headless
Chromium impl is deferred (see `crates/renderer`).
