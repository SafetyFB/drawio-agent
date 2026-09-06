//! Fidelity regression tests (M1 acceptance criteria).
//!
//! These encode REAL, user-visible roundtrip corruption found while
//! probing xml-core in 2025-09:
//!
//! 1. `value="line1&#10;line2"` (multi-line label) roundtrips into
//!    `value="line1&amp;#10;line2"` — the label renders as literal `&#10;`
//!    text on the canvas.
//! 2. `mxfile` / `<diagram>` / `mxGraphModel` level attributes (`host`,
//!    `dx`, `grid`, `pageWidth`, …) are silently dropped.
//! 3. `mxGeometry` loses `relative="1"` and its child elements
//!    (`<mxPoint>` waypoints / sourcePoint / targetPoint, `<Array>`) —
//!    edge paths are silently rerouted.
//!
//! Every parse → serialize roundtrip in patch / agent-loop / subgraph
//! paths runs into these. The tests are `#[ignore]`d until M1 lands (see
//! PLAN.md milestone 1); flipping them on is the acceptance gate.
//!
//! `normalize_whitespace` compares against the original after stripping
//! insignificant inter-tag whitespace, so the tests only fail on REAL
//! information loss, not on formatting drift.

use drawio_agent_xml_core::MxFile;

/// Full-featured sample exercising every known-lossy construct.
const FIDELITY_SAMPLE: &str = r#"<mxfile host="app.diagrams.net" type="device">
  <diagram id="d1" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1"
                  tooltips="1" connect="1" arrows="1" fold="1" page="1"
                  pageScale="1" pageWidth="850" pageHeight="1100" math="0"
                  shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="line1&#10;line2&#10;&amp; &lt;tag&gt;" style="rounded=1;" vertex="1" parent="1">
          <mxGeometry x="10" y="20" width="100" height="50" as="geometry"/>
        </mxCell>
        <mxCell id="3" style="edgeStyle=orthogonalEdgeStyle;html=1;" edge="1" parent="1" source="2" target="2">
          <mxGeometry relative="1" as="geometry">
            <mxPoint x="60" y="70" as="sourcePoint"/>
            <Array as="points">
              <mxPoint x="120" y="90"/>
              <mxPoint x="150" y="110"/>
            </Array>
            <mxPoint x="100" y="120" as="targetPoint"/>
          </mxGeometry>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

/// Strip inter-tag whitespace and the XML declaration for comparison.
fn normalize(xml: &str) -> String {
    let no_decl = xml.replace("<?xml version=\"1.0\" encoding=\"UTF-8\"?>", "");
    let mut out = String::new();
    let mut prev_was_tag = false;
    for ch in no_decl.trim().chars() {
        if ch.is_whitespace() && prev_was_tag {
            continue;
        }
        prev_was_tag = ch == '>';
        out.push(ch);
    }
    // Collapse runs of whitespace that are NOT inside attribute values is
    // hard without a real parse; instead collapse whitespace runs that sit
    // between '>' and '<' (pure indentation) only.
    out.replace("> ", ">").replace(" >", ">").replace("\n", "")
}

#[test]
fn roundtrip_preserves_multiline_label_entities() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let out = file.to_xml().expect("serialize must succeed");

    // The &#10; entity must survive exactly once (no double escaping into
    // &amp;#10;), and &amp; / &lt; must survive too.
    assert!(
        out.contains("line1&#10;line2&#10;&amp; &lt;tag&gt;"),
        "multi-line label corrupted by double escaping:\n{out}"
    );
    assert!(
        !out.contains("&amp;#10;"),
        "newline entity was double-escaped into &amp;#10;:\n{out}"
    );
}

#[test]
fn roundtrip_preserves_document_and_model_attributes() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let out = normalize(&file.to_xml().expect("serialize must succeed"));

    for attr in ["host=\"app.diagrams.net\"", "dx=\"800\"", "grid=\"1\"", "gridSize=\"10\"", "pageWidth=\"850\"", "pageHeight=\"1100\"", "math=\"0\""] {
        assert!(
            out.contains(attr),
            "lost attribute {attr} on roundtrip:\n{out}"
        );
    }
}

#[test]
fn roundtrip_preserves_edge_geometry_and_waypoints() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let out = file.to_xml().expect("serialize must succeed");

    assert!(
        out.contains("relative=\"1\""),
        "mxGeometry relative=1 dropped on roundtrip:\n{out}"
    );
    assert!(
        out.contains("<mxPoint x=\"60\" y=\"70\" as=\"sourcePoint\"/>"),
        "sourcePoint waypoint dropped on roundtrip:\n{out}"
    );
    assert!(
        out.contains("as=\"points\"") && out.contains("<mxPoint x=\"120\" y=\"90\"/>"),
        "Array waypoints dropped on roundtrip:\n{out}"
    );
    assert!(
        out.contains("<mxPoint x=\"100\" y=\"120\" as=\"targetPoint\"/>"),
        "targetPoint dropped on roundtrip:\n{out}"
    );
}

/// The subgraph serialization path must keep the same fidelity promises:
/// scope documents handed to the LLM must not mangle entities either.
#[test]
fn subgraph_serialize_preserves_multiline_labels() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let model = file.diagrams[0].model.as_ref().expect("model");
    let sub = model.extract_subgraph(&["2"]);
    let scope = drawio_agent_xml_core::serialize_subgraph(&sub).expect("scope serialize");

    assert!(
        scope.contains("line1&#10;line2"),
        "scope document mangled the label entity:\n{scope}"
    );
    assert!(
        !scope.contains("&amp;#10;"),
        "scope document double-escaped the newline entity:\n{scope}"
    );
}

/// Draw.io's on-disk format (base64 + raw deflate body) must round-trip to
/// the same canonical document as the uncompressed path.
#[test]
fn compressed_roundtrip_matches_uncompressed_output() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let compressed = file.to_compressed_xml().expect("compress");
    assert!(
        compressed.contains("host=\"app.diagrams.net\""),
        "mxfile attributes must survive on the compressed path"
    );
    let reparsed = MxFile::parse(compressed.as_bytes()).expect("decompress");
    assert_eq!(
        reparsed.to_xml().expect("serialize"),
        file.to_xml().expect("serialize"),
        "compressed roundtrip must produce the identical document"
    );
}

/// Entity semantics: decoded in-memory values carry the real newline, and
/// re-serialization restores the `&#10;` reference exactly once.
#[test]
fn entity_codec_is_symmetric() {
    let file = MxFile::parse(FIDELITY_SAMPLE.as_bytes()).expect("sample must parse");
    let cell = file.diagrams[0]
        .model
        .as_ref()
        .expect("model")
        .get("2")
        .expect("cell 2");
    assert_eq!(
        cell.value.as_deref(),
        Some("line1\nline2\n& <tag>"),
        "in-memory value must be decoded semantic text"
    );
    let out = file.to_xml().expect("serialize");
    assert!(
        out.contains("line1&#10;line2&#10;&amp; &lt;tag&gt;"),
        "serialized value must re-encode entities exactly once:\n{out}"
    );
}
