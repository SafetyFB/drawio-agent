//! Draw.io diagram renderer.
//!
//! Phase 3 spike: validates the headless-Chromium + viewer-static.min.js
//! approach. The public [`Renderer`] is built on a [`RenderDriver`] trait
//! so tests can substitute a deterministic mock.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

pub mod checksum;
pub mod driver;

pub use driver::{
    bundled_chromium_path, find_chromium, HeadlessChromiumDriver, MockDriver,
    PINNED_CHROMIUM_VERSION,
};

/// Render options for a single diagram export.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Output pixel scale multiplier (1.0 = native, 2.0 = retina).
    pub scale: f64,
    /// Background CSS color (e.g. `"#ffffff"`). Empty string = transparent.
    pub background: String,
    /// Optional border padding in pixels around the diagram.
    pub border: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            scale: 1.0,
            background: "#ffffff".into(),
            border: 10,
        }
    }
}

/// Errors surfaced by the renderer.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("browser: {0}")]
    Browser(String),
    #[error("page: {0}")]
    Page(String),
    #[error("xml: {0}")]
    Xml(String),
    #[error("export: {0}")]
    Export(String),
}

/// Pluggable rendering backend. Implementations may drive a real browser
/// (production) or return canned bytes (tests).
#[async_trait]
pub trait RenderDriver: Send + Sync {
    async fn render(
        &self,
        xml: &str,
        opts: &RenderOptions,
    ) -> Result<Vec<u8>, RenderError>;
}

/// The main entry point. Holds a shared driver and forwards calls.
#[derive(Clone)]
pub struct Renderer {
    driver: Arc<dyn RenderDriver>,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").field("driver", &"<dyn RenderDriver>").finish()
    }
}

impl Renderer {
    pub fn new(driver: Arc<dyn RenderDriver>) -> Self {
        Self { driver }
    }

    /// Render a Draw.io XML diagram to PNG bytes.
    pub async fn render(
        &self,
        xml: &str,
        opts: &RenderOptions,
    ) -> Result<Vec<u8>, RenderError> {
        if xml.trim().is_empty() {
            return Err(RenderError::Xml("empty XML input".into()));
        }
        self.driver.render(xml, opts).await
    }
}
