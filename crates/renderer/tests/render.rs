//! TDD tests for the renderer scaffold.
//!
//! Unit tests use `MockDriver` so they don't require a browser. Live
//! Chromium integration tests (when chromiumoxide is wired in) would
//! live in a separate gated test file.

use drawio_agent_renderer::{MockDriver, RenderDriver, RenderError, RenderOptions, Renderer};

// ---------------------------------------------------------------------------
// Renderer-level tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renderer_returns_bytes_from_driver() {
    let driver = MockDriver::new().with_bytes(vec![0x89, b'P', b'N', b'G']);
    let renderer = Renderer::new(std::sync::Arc::new(driver));

    let bytes = renderer
        .render(
            r#"<mxfile><diagram id="x"/></mxfile>"#,
            &RenderOptions::default(),
        )
        .await
        .expect("render should succeed");
    assert_eq!(bytes, vec![0x89, b'P', b'N', b'G']);
}

#[tokio::test]
async fn renderer_rejects_empty_xml() {
    let driver = MockDriver::new();
    let renderer = Renderer::new(std::sync::Arc::new(driver));

    let err = renderer
        .render("   ", &RenderOptions::default())
        .await
        .expect_err("empty xml must error");
    assert!(matches!(err, RenderError::Xml(_)));
}

#[tokio::test]
async fn renderer_passes_xml_and_opts_to_driver() {
    let driver = MockDriver::new();
    let renderer = Renderer::new(std::sync::Arc::new(driver.clone()));

    let opts = RenderOptions {
        scale: 2.5,
        background: "#000000".into(),
        border: 42,
        ..Default::default()
    };
    let xml = "<mxfile><diagram id='a'/></mxfile>";
    renderer.render(xml, &opts).await.unwrap();

    let calls = driver.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, xml);
    assert!((calls[0].1.scale - 2.5).abs() < 1e-9);
    assert_eq!(calls[0].1.background, "#000000");
    assert_eq!(calls[0].1.border, 42);
}

#[tokio::test]
async fn renderer_propagates_driver_errors() {
    let driver = MockDriver::new().with_error("boom");
    let renderer = Renderer::new(std::sync::Arc::new(driver));

    let err = renderer
        .render("<mxfile/>", &RenderOptions::default())
        .await
        .expect_err("driver error must propagate");
    match err {
        RenderError::Export(msg) => assert_eq!(msg, "boom"),
        other => panic!("expected Export, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// MockDriver tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mock_driver_records_every_call() {
    let driver = MockDriver::new();
    let _ = driver
        .render("<a/>", &RenderOptions::default())
        .await
        .unwrap();
    let _ = driver.render("<b/>", &RenderOptions::default()).await.unwrap();
    let _ = driver.render("<c/>", &RenderOptions::default()).await.unwrap();
    let calls = driver.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0, "<a/>");
    assert_eq!(calls[1].0, "<b/>");
    assert_eq!(calls[2].0, "<c/>");
}

#[tokio::test]
async fn mock_driver_placeholder_is_valid_png_signature() {
    // When no bytes are configured, MockDriver returns a 1x1 placeholder PNG.
    // Verify the 8-byte PNG signature is correct so downstream code that
    // sniffs PNG won't choke.
    let driver = MockDriver::new();
    let bytes = driver
        .render("<mxfile/>", &RenderOptions::default())
        .await
        .unwrap();
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        "mock placeholder must start with the PNG signature"
    );
}

#[tokio::test]
async fn mock_driver_returns_configured_bytes_over_placeholder() {
    let driver = MockDriver::new().with_bytes(b"custom payload".to_vec());
    let bytes = driver
        .render("<mxfile/>", &RenderOptions::default())
        .await
        .unwrap();
    assert_eq!(bytes, b"custom payload");
}

#[tokio::test]
async fn mock_driver_with_error_short_circuits_to_err() {
    let driver = MockDriver::new().with_error("forced");
    let err = driver
        .render("<mxfile/>", &RenderOptions::default())
        .await
        .expect_err("with_error must surface as Err");
    assert!(matches!(err, RenderError::Export(ref m) if m == "forced"));
    // Calls are still recorded even when erroring — useful for debugging.
    assert_eq!(driver.calls().len(), 1);
}

// ---------------------------------------------------------------------------
// RenderOptions tests
// ---------------------------------------------------------------------------

#[test]
fn render_options_default_is_sensible() {
    let opts = RenderOptions::default();
    assert!((opts.scale - 1.0).abs() < 1e-9);
    assert_eq!(opts.background, "#ffffff");
    assert!(opts.border >= 1, "default border must be >= 1");
}

#[test]
fn render_options_clone_preserves_fields() {
    let opts = RenderOptions {
        scale: 3.0,
        background: "transparent".into(),
        border: 7,
        ..Default::default()
    };
    let copy = opts.clone();
    assert!((copy.scale - 3.0).abs() < 1e-9);
    assert_eq!(copy.background, "transparent");
    assert_eq!(copy.border, 7);
}
