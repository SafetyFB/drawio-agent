//! Integration test against a real Chromium binary.
//!
//! Requires an actual browser: a cached chrome-headless-shell bundle, a
//! system Chrome/Chromium/Edge, or `DRAWIO_AGENT_CHROMIUM_PATH`. Skipped by
//! default (`#[ignore]`) so `cargo test` works on machines without any
//! browser available; run with `cargo test -- --ignored` where one exists.

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
#[ignore = "needs a real browser (cached bundle / system chrome / explicit path)"]
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

/// E2E for the hot path: cold launch, then two warm renders (plain +
/// annotate) with timings. The warm renders exercise the event-driven
/// wrapper flow (`load` event → annotate → export) and the same-content
/// cache in the harness layer is NOT involved here (that lives above).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real browser (cached bundle / system chrome / explicit path)"]
async fn chromium_hot_path_render_timings() {
    let driver = HeadlessChromiumDriver::launch().await
        .expect("chromium driver should launch");
    let renderer = Renderer::new(Arc::new(driver));

    let t0 = std::time::Instant::now();
    let png = renderer
        .render(SAMPLE_XML, &RenderOptions::default())
        .await
        .expect("cold render should succeed");
    let cold = t0.elapsed();
    assert_eq!(&png[..4], b"\x89PNG");

    let t1 = std::time::Instant::now();
    let png2 = renderer
        .render(SAMPLE_XML, &RenderOptions::default())
        .await
        .expect("warm render should succeed");
    let warm = t1.elapsed();
    assert_eq!(&png2[..4], b"\x89PNG");

    let t2 = std::time::Instant::now();
    let opts = RenderOptions { annotate: true, ..Default::default() };
    let png3 = renderer
        .render(SAMPLE_XML, &opts)
        .await
        .expect("annotate render should succeed");
    let warm_annotate = t2.elapsed();
    assert_eq!(&png3[..4], b"\x89PNG");

    println!("cold(启动+首次渲染): {cold:.1?}");
    println!("warm(事件驱动热路径): {warm:.1?}");
    println!("warm+annotate(徽章): {warm_annotate:.1?}");
    // 热路径应明显快于冷启动（不做硬上限断言，慢机 CI 也应通过）。
    assert!(warm < cold, "warm render should beat cold launch: {warm:.1?} vs {cold:.1?}");
}

/// libavoid 避障布线（drawio 内置 LibavoidRouting，headless 经插件执行）。
/// 直连路径上有障碍节点 → 改写后的 XML 应带正交样式与避障拐点。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real browser (cached bundle / system chrome / explicit path)"]
async fn chromium_libavoid_reroute_writes_waypoints() {
    let _port = drawio_agent_renderer::init_static_server()
        .await
        .expect("static server should start (webapp cached)");
    let driver = HeadlessChromiumDriver::launch().await
        .expect("chromium driver should launch");
    let renderer = Renderer::new(std::sync::Arc::new(driver));
    let xml = r#"<mxfile host="app.diagrams.net"><diagram id="d" name="Page-1"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/><mxCell id="a" value="A" style="rounded=1;" vertex="1" parent="1"><mxGeometry x="40" y="60" width="120" height="60" as="geometry"/></mxCell><mxCell id="b" value="B" style="rounded=1;" vertex="1" parent="1"><mxGeometry x="500" y="60" width="120" height="60" as="geometry"/></mxCell><mxCell id="obs" value="挡路" style="rounded=1;" vertex="1" parent="1"><mxGeometry x="280" y="70" width="100" height="40" as="geometry"/></mxCell><mxCell id="e" edge="1" parent="1" source="a" target="b"><mxGeometry relative="1" as="geometry"/></mxCell></root></mxGraphModel></diagram></mxfile>"#;
    let out = renderer.reroute(xml).await.expect("libavoid reroute should succeed");
    println!("rerouted xml bytes: {}", out.len());
    let path = std::env::temp_dir().join("e2e_libavoid_out.xml");
    std::fs::write(&path, &out).unwrap();
    println!("written to {}", path.display());
    // 正交样式与拐点应出现；若 libavoid 未启用则明确失败便于诊断
    assert!(
        out.contains("orthogonalEdgeStyle") || out.contains("<Array"),
        "期望正交样式或拐点，输出前 400 字节: {}",
        &out[..out.len().min(400)]
    );
}
