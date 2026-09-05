# drawio-agent-renderer

Draw.io diagram renderer. Drives some backend (browser, cloud service,
or pure-Rust) to turn Draw.io XML into PNG bytes for VLM-based visual
review.

## Status

**Phase 3 spike — deferred on the Chromium path.**

- ✅ `Renderer` + `RenderDriver` trait — stable interface
- ✅ `MockDriver` — deterministic, 10 unit tests pass
- ⏸ `HeadlessChromiumDriver` — stub; see "Why deferred" below
- ✅ `find_chromium()` path resolver (works, returns `Some` on this system)
- ✅ `assets/render.html` + bundled `viewer-static.min.js` (3.4 MB) ready
- ✅ `assets/cdp.rs` minimal CDP client — gets past launch but blocked by macOS keychain
- ✅ Integration test scaffold (`tests/chromium_integration.rs`, `#[ignore]`d)

## What shipped

- **`Renderer::render(xml, opts) -> Vec<u8>`** — the only public API
  callers need to know.
- **`RenderDriver` trait** — pluggable backend. Mock + (stub) Chromium
  impls; add more by implementing the trait.
- **`MockDriver`** — configurable bytes, call recording, error
  injection, valid 1x1 PNG placeholder.
- **`RenderOptions { scale, background, border }`** with `Default`.
- **`RenderError` enum** with `Xml | Browser | Page | Export` variants.
- 10 unit tests covering Renderer + MockDriver + RenderOptions.
- Deferred integration test (`#[ignore]`d) for live Chromium.
- `find_chromium()` looks at `CHROMIUM_PATH` env + macOS app path +
  common Linux binary names.

## Why deferred

Live Chromium on this machine triggers macOS **keychain access prompts**
when launched in headless mode — unacceptable for a server / agent
context. We tried two paths; both got past the CDP connect but
either hung (chromiumoxide 0.7 hangs on Chrome 150+ because of pinned
Chrome ~126) or stalled on a keychain dialog before the first
screenshot (hand-rolled CDP client via tokio-tungstenite).

The `RenderDriver` trait means we can swap in any of these without
touching callers:

| Option | Pros | Cons |
|---|---|---|
| **Docker Chromium with `--use-mock-keychain --password-store=basic`** | Bypasses keychain; standard pipeline | Need Docker in CI; bigger images |
| **browserless.io / hosted rendering service** | No local browser; clean | Per-render cost; needs API key |
| **Frontend pre-render via WebSocket** | Zero infra; uses user's actual browser | Requires user to have tab open; async coordination |
| **Pure-Rust `drawio-rs`** | No browser at all | Limited fidelity (per Phase 0 research) |

The frontend-pre-render approach is interesting for our agent:
the user's browser already has the Draw.io embed loaded and rendering,
so `canvas.toDataURL()` is essentially free when the user has the
diagram open. The Agent Loop would then ask "send me your current
canvas" instead of "render server-side". Both flows share the same
`RenderDriver` trait, so we can A/B them later.

## Test layout

- `tests/render.rs` — 10 unit tests with `MockDriver`. Run with `cargo test`.
- `tests/chromium_integration.rs` — `#[ignore]`d; documents the live
  integration we want once a renderer is wired up. Run with
  `cargo test -- --ignored`.

## Run

```bash
cargo test                     # 10 mock tests, no browser needed
cargo test -- --ignored        # chromium integration (currently unreachable)
```

## Why this matters

The whole visual-review loop depends on this: after an LLM produces
or patches Draw.io XML, we need to render it to PNG and feed the PNG
to a VLM for the "is there overlap / text overflow / crossed edges?"
check. Without a working renderer, the Agent Loop has no eyes.

## Bundled chrome-headless-shell

The renderer crate's build.rs downloads a pinned `chrome-headless-shell`
binary from `storage.googleapis.com/chrome-for-testing-public` on first
build and caches it under `XDG_CACHE_HOME/drawio-agent/chrome-headless-shell/`
(or the platform equivalent). Subsequent builds use the cached copy.

### Environment variables

| Variable | Effect |
|---|---|
| `DRAWIO_AGENT_CHROMIUM_PATH` | Override with an explicit binary path. Wins over bundled. |
| `DRAWIO_AGENT_OFFLINE=1` | Skip the download entirely; `find_chromium()` falls through to system chrome or env override. |
| `DRAWIO_AGENT_CACHE_DIR` | Override the cache root (default: `XDG_CACHE_HOME/drawio-agent/`). |

### Checksum verification

The four SHA-256 hashes of the chrome-headless-shell binary are baked into
`crates/renderer/src/checksum.rs::CHECKSUMS`. On every build, the downloaded
binary is verified against the pinned hash; mismatch fails the build with an
actionable error message.

### Version bump procedure

1. Update `PINNED_VERSION` in `crates/renderer/build.rs` to the new Chrome
   for Testing version (e.g. `132.0.6834.83`).
2. Run `cargo build -p drawio-agent-renderer` on **each supported platform**
   (mac-x64, mac-arm64, linux64, win64 — use `cargo build --target` or CI).
   The build will fail with a "no pinned SHA-256" error and print the new
   observed hash for that platform.
3. Paste each observed hash into the `CHECKSUMS` table in
   `crates/renderer/src/checksum.rs`.
4. Re-run `cargo build` to confirm verification passes.
5. Commit both files together (`build.rs` and `checksum.rs`) in one PR.

### Escape hatch (USE WITH CAUTION)

`DRAWIO_AGENT_ACCEPT_NEW_CHECKSUM=1` allows the build to proceed when a
platform has no pinned hash yet. Use this only during a version bump when
capturing hashes for a new platform. Never set this in production CI.

### Cache reset

```sh
rm -rf ~/.cache/drawio-agent/chrome-headless-shell  # Linux
rm -rf ~/Library/Caches/drawio-agent/chrome-headless-shell  # macOS
```
