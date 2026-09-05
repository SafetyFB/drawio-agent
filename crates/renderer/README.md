# drawio-agent-renderer

Draw.io diagram renderer. Drives a headless Chromium browser against a
self-hosted `viewer-static.min.js` page to export PNG bytes from raw
Draw.io XML.

## Status

**Phase 3 spike** — architecture + scaffolding landed. MockDriver fully
tested. Live Chromium integration (`HeadlessChromiumDriver`) is sketched
out but not in the default build because the chromiumoxide API surface
couldn't be verified without a Chromium binary on PATH.

## What shipped in this spike

- `Renderer` — async API that takes `&str` XML + `RenderOptions`, returns
  `Vec<u8>` PNG via a `RenderDriver`.
- `RenderDriver` trait — pluggable backend; production = browser, tests =
  canned bytes.
- `MockDriver` — deterministic mock returning configurable bytes (or a
  valid 1x1 PNG placeholder) and recording every call for assertions.
- `RenderOptions { scale, background, border }` with `Default`.
- `RenderError` enum with `Xml | Browser | Page | Export` variants.
- 10 unit tests covering Renderer + MockDriver + RenderOptions.
- Sketch of `HeadlessChromiumDriver` using `chromiumoxide` — see
  `src/driver/chromium.rs.bak` if present, or `git log` for the historical
  version. Not compiled in default build.

## Test layout

- `tests/render.rs` — unit tests with `MockDriver` (10 tests, all green).

## What's missing / recommended next steps

1. **Install Chromium** (`brew install --cask chromium` or
   `npx playwright install chromium`) so live tests can run.
2. **Wire chromiumoxide back into the workspace** as an optional
   dependency behind a `chromium` cargo feature; gate
   `HeadlessChromiumDriver` and its tests with `#[cfg(feature = "chromium")]`.
3. **Verify the chromium.rs sketch against real chromiumoxide 0.7** —
   the API drifted between minor versions; `BrowserConfig::builder()`,
   `Browser::launch()`, and `Page::evaluate()` return shapes need a sanity
   pass.
4. **Self-host `viewer-static.min.js`** — either commit a pinned copy
   (1–2 MB) into `assets/` and load via `file://`, or pin a CDN URL with a
   hash check.
5. **Tune the export script** — the existing sketch calls `new Graph(...)`
   + `mxXmlCodec.decode(...)`. Verify this works against viewer-static
   (which exposes `mxGraph` globally, not as a class).

## Run

```bash
cargo test                     # mock tests (no browser needed)
```

## Why this matters

The whole visual-review loop depends on this: after an LLM produces or
patches Draw.io XML, we render it back to PNG and feed the PNG to a VLM
for the "is there overlap / text overflow / crossed edges?" check.
Without a working renderer, the Agent Loop has no eyes.
