//! Render driver implementations.

pub mod mock;

pub use mock::MockDriver;

// HeadlessChromiumDriver is implemented in `chromium.rs` but kept out of
// the default build until chromiumoxide is wired into the workspace and a
// Chromium binary is available. To enable:
//
//   1. Add `chromiumoxide = "0.7"` to `crates/renderer/Cargo.toml`
//   2. Add a `chromium` feature to that crate (default = off)
//   3. Gate this module and the `HeadlessChromiumDriver` re-export below
//      with `#[cfg(feature = "chromium")]`
//
// The Chromium integration code lives at `chromium.rs` in this directory
// and was written against the chromiumoxide 0.7 API.
#[allow(dead_code)]
const HEADLESS_CHROMIUM_DRIVER_NOTE: &str =
    "See src/driver/chromium.rs for the headless Chromium implementation sketch";
