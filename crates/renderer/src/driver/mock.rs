//! Mock driver for unit tests.
//!
//! Returns canned PNG bytes so renderer logic can be exercised without a
//! browser. Use [`MockDriver::with_bytes`] to set the exact PNG payload
//! the test expects.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{RenderDriver, RenderError, RenderOptions};

/// Mock that returns a fixed byte payload, recording every call.
#[derive(Clone, Default)]
pub struct MockDriver {
    state: Arc<Mutex<MockState>>,
}

impl std::fmt::Debug for MockDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockDriver").finish()
    }
}

#[derive(Default)]
struct MockState {
    /// Bytes returned by `render`. If None, returns a placeholder PNG.
    bytes: Option<Vec<u8>>,
    /// All (xml, opts) calls recorded.
    calls: Vec<(String, RenderOptions)>,
    /// Optional error to surface instead of bytes.
    error: Option<String>,
}

impl MockDriver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the bytes returned by `render`.
    pub fn with_bytes(self, bytes: Vec<u8>) -> Self {
        self.state.lock().unwrap().bytes = Some(bytes);
        self
    }

    /// Configure an error message returned by `render`.
    pub fn with_error(self, msg: impl Into<String>) -> Self {
        self.state.lock().unwrap().error = Some(msg.into());
        self
    }

    /// Inspect recorded calls.
    pub fn calls(&self) -> Vec<(String, RenderOptions)> {
        self.state.lock().unwrap().calls.clone()
    }
}

#[async_trait]
impl RenderDriver for MockDriver {
    async fn render(
        &self,
        xml: &str,
        opts: &RenderOptions,
    ) -> Result<Vec<u8>, RenderError> {
        let mut state = self.state.lock().unwrap();
        state.calls.push((xml.to_string(), opts.clone()));
        if let Some(err) = &state.error {
            return Err(RenderError::Export(err.clone()));
        }
        Ok(state.bytes.clone().unwrap_or_else(|| placeholder_png()))
    }
}

/// Minimal valid PNG (1x1 white pixel). Used when the mock isn't configured.
fn placeholder_png() -> Vec<u8> {
    vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, // signature
        0x00, 0x00, 0x00, 0x0d, // IHDR length
        b'I', b'H', b'D', b'R',
        0x00, 0x00, 0x00, 0x01, // width = 1
        0x00, 0x00, 0x00, 0x01, // height = 1
        0x08, 0x02, 0x00, 0x00, 0x00, // bit depth, color type, compression, filter, interlace
        0x90, 0x77, 0x53, 0xde, // CRC
        0x00, 0x00, 0x00, 0x0c, // IDAT length
        b'I', b'D', b'A', b'T',
        0x08, 0x99, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, // compressed pixel
        0x5b, 0x9b, 0xae, 0x6c, // CRC
        0x00, 0x00, 0x00, 0x00, // IEND length
        b'I', b'E', b'N', b'D',
        0xae, 0x42, 0x60, 0x82, // CRC
    ]
}
