//! Headless Chromium driver — currently a DEFERRED STUB.
//!
//! ## Why deferred
//!
//! Live Chromium on macOS triggers keychain access prompts which are
//! unacceptable in a server / agent context. Even with `--no-sandbox`
//! and headless flags, Chromium 155 occasionally pokes the keychain for
//! cert validation. We attempted two paths:
//!
//! 1. **`chromiumoxide` 0.7** — pinned to Chrome ~126, hangs forever
//!    connecting to CDP on Chrome 150+.
//! 2. **Hand-rolled minimal CDP client** via tokio-tungstenite — gets
//!    past launch but blocks on a keychain dialog before the first
//!    screenshot.
//!
//! ## Plan
//!
//! The [`RenderDriver`] trait and [`MockDriver`] (tests/render.rs) are
//! stable and fully tested. Swap in any of these later without touching
//! callers:
//!
//! - **Server-side rendering service** (e.g. browserless.io) over HTTP
//! - **Pre-render in the user's browser** (frontend has a canvas, can
//!   `toDataURL()` and post back the PNG via WebSocket)
//! - **A pure-Rust Draw.io XML renderer** (drawio-rs has limited fidelity)
//! - **Pre-bundled Chromium in Docker** with `--password-store=basic`
//!   and `--use-mock-keychain` flags (may bypass the prompt)
//!
//! Until then, [`HeadlessChromiumDriver`] is a stub: [`launch`] and
//! [`launch_with`] both return [`RenderError::Browser`] explaining the
//! situation. [`find_chromium`] still works for binary discovery.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{RenderDriver, RenderError, RenderOptions};

/// Resolve the Chromium binary path. Looks at (in order):
/// 1. `CHROMIUM_PATH` env var
/// 2. `/Applications/Chromium.app/Contents/MacOS/Chromium` (macOS app)
/// 3. Various Linux binary names
pub fn find_chromium() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CHROMIUM_PATH") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let candidates = [
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/local/bin/chromium",
    ];
    for c in candidates {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Some(pb);
        }
    }
    for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
        if let Some(p) = which(name) {
            return Some(p);
        }
    }
    None
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Stub driver. [`HeadlessChromiumDriver::launch`] always fails until the
/// real implementation is unblocked. See module-level docs.
#[derive(Debug, Default)]
pub struct HeadlessChromiumDriver {
    _private: (), // prevent construction outside this module
}

const DEFERRED_MSG: &str = "HeadlessChromiumDriver is deferred: see \
    src/driver/chromium.rs docstring. Use MockDriver for tests; pick a \
    different production renderer (Docker Chromium with \
    --use-mock-keychain, browserless.io, frontend pre-render, or \
    pure-Rust drawio-rs).";

impl HeadlessChromiumDriver {
    /// Always returns [`RenderError::Browser`] explaining the deferral.
    pub async fn launch() -> Result<Self, RenderError> {
        Err(RenderError::Browser(DEFERRED_MSG.into()))
    }

    /// Always returns [`RenderError::Browser`] explaining the deferral.
    pub async fn launch_with(_chrome_path: impl AsRef<Path>) -> Result<Self, RenderError> {
        Err(RenderError::Browser(DEFERRED_MSG.into()))
    }
}

#[async_trait]
impl RenderDriver for HeadlessChromiumDriver {
    async fn render(
        &self,
        _xml: &str,
        _opts: &RenderOptions,
    ) -> Result<Vec<u8>, RenderError> {
        // Unreachable in practice — launch() always fails — but must compile.
        Err(RenderError::Browser(DEFERRED_MSG.into()))
    }
}
