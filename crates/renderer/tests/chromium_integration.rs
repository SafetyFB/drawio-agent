//! Integration tests against a real Chromium binary — currently DEFERRED.
//!
//! HeadlessChromiumDriver is a stub (see src/driver/chromium.rs). These
//! tests exist as a record of the integration we want to run once the
//! renderer is unblocked (Docker Chromium with `--use-mock-keychain`,
//! browserless.io, frontend pre-render, or pure-Rust drawio-rs).
//!
//! All tests in this file are marked `#[ignore]` so they don't run by
//! default. Run with `cargo test -- --ignored` once a renderer is wired up.

use std::sync::Arc;

use drawio_agent_renderer::{
    find_chromium, HeadlessChromiumDriver, RenderOptions, Renderer,
};

const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="sample" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1"
                  tooltips="1" connect="1" arrows="1" fold="1" page="1"
                  pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Hello" style="rounded=0;whiteSpace=wrap;html=1;"
                vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="World" style="rounded=0;whiteSpace=wrap;html=1;"
                vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "HeadlessChromiumDriver is deferred (macOS keychain issue). \
            See src/driver/chromium.rs docstring."]
async fn chromium_renders_sample_xml_to_png() {
    let _ = find_chromium();
    let driver = HeadlessChromiumDriver::launch().await
        .expect("chromium driver should launch once unblocked");
    let renderer = Renderer::new(Arc::new(driver));
    let png = renderer
        .render(SAMPLE_XML, &RenderOptions::default())
        .await
        .expect("render should succeed");
    assert!(!png.is_empty());
    assert_eq!(
        &png[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "output must start with PNG signature"
    );
}
