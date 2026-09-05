//! TDD tests for XML serialization roundtrip.
//!
//! Roundtrip invariant: parse(S) -> serialize -> parse -> equals original.
//! Attribute equality is asserted per-cell so we catch attribute-level drift.

use drawio_agent_xml_core::{MxFile, MxGraphModel};

const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">
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
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;html=1;"
                edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

fn sample_file() -> MxFile {
    MxFile::parse(SAMPLE.as_bytes()).expect("sample must parse")
}

fn collect_ids(model: &MxGraphModel) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(cell: &drawio_agent_xml_core::Cell, out: &mut Vec<String>) {
        out.push(cell.id.clone());
        for child in &cell.children {
            walk(child, out);
        }
    }
    walk(&model.root, &mut out);
    out
}

#[test]
fn serialize_produces_well_formed_xml() {
    let file = sample_file();
    let xml = file.to_xml().expect("serialize must succeed");

    // Re-parse to confirm the output is well-formed XML.
    let parsed = MxFile::parse(xml.as_bytes())
        .expect("serialized output must reparse cleanly");
    assert!(!parsed.diagrams.is_empty());
}

#[test]
fn serialized_xml_contains_required_elements() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    // Cheap sanity checks on the literal text — these catch whole-structure
    // regressions (e.g. forgetting to emit mxGraphModel).
    assert!(xml.contains("<mxfile"), "missing <mxfile>");
    assert!(xml.contains("<diagram"), "missing <diagram>");
    assert!(xml.contains("<mxGraphModel"), "missing <mxGraphModel>");
    assert!(xml.contains("<mxCell"), "missing <mxCell>");
    assert!(xml.contains("<mxGeometry"), "missing <mxGeometry>");
}

#[test]
fn roundtrip_preserves_diagram_count() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();
    assert_eq!(parsed.diagrams.len(), file.diagrams.len());
    assert_eq!(parsed.diagrams[0].id, file.diagrams[0].id);
    assert_eq!(parsed.diagrams[0].name, file.diagrams[0].name);
}

#[test]
fn roundtrip_preserves_cell_ids() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();

    let orig = collect_ids(file.diagrams[0].model.as_ref().unwrap());
    let new = collect_ids(parsed.diagrams[0].model.as_ref().unwrap());

    let mut orig_sorted = orig.clone();
    let mut new_sorted = new.clone();
    orig_sorted.sort();
    new_sorted.sort();
    assert_eq!(orig_sorted, new_sorted, "id set must roundtrip exactly");
}

#[test]
fn roundtrip_preserves_cell_geometry() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();

    let orig = file.diagrams[0].model.as_ref().unwrap();
    let new = parsed.diagrams[0].model.as_ref().unwrap();

    for id in ["2", "3"] {
        let o = orig.get(id).unwrap();
        let n = new.get(id).unwrap();
        assert_eq!(o.geometry, n.geometry, "geometry must match for id {id}");
    }
}

#[test]
fn roundtrip_preserves_edge_endpoints_and_attributes() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();

    let orig = file.diagrams[0].model.as_ref().unwrap();
    let new = parsed.diagrams[0].model.as_ref().unwrap();

    let o = orig.get("4").unwrap();
    let n = new.get("4").unwrap();
    assert_eq!(o.source, n.source);
    assert_eq!(o.target, n.target);
    assert_eq!(o.style, n.style);
    assert_eq!(o.edge, n.edge);
    assert_eq!(o.parent, n.parent);
}

#[test]
fn roundtrip_preserves_value_and_style_on_nodes() {
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();

    let orig = file.diagrams[0].model.as_ref().unwrap();
    let new = parsed.diagrams[0].model.as_ref().unwrap();

    for id in ["2", "3"] {
        let o = orig.get(id).unwrap();
        let n = new.get(id).unwrap();
        assert_eq!(o.value, n.value, "value must match for id {id}");
        assert_eq!(o.style, n.style, "style must match for id {id}");
        assert_eq!(o.vertex, n.vertex, "vertex flag must match for id {id}");
    }
}

#[test]
fn roundtrip_yields_valid_model() {
    // After two parse hops (parse -> serialize -> parse) the result
    // must still pass validate().
    let file = sample_file();
    let xml = file.to_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();
    let report = parsed.diagrams[0]
        .model
        .as_ref()
        .unwrap()
        .validate();
    assert!(report.is_ok(), "roundtripped model must validate: {report}");
}

#[test]
fn compressed_serialize_produces_well_formed_xml() {
    let file = sample_file();
    let xml = file.to_compressed_xml().expect("compressed serialize must succeed");

    let parsed = MxFile::parse(xml.as_bytes())
        .expect("compressed serialized output must reparse cleanly");
    assert_eq!(parsed.diagrams.len(), file.diagrams.len());
}

#[test]
fn compressed_roundtrip_preserves_structure() {
    let file = sample_file();
    let xml = file.to_compressed_xml().unwrap();
    let parsed = MxFile::parse(xml.as_bytes()).unwrap();

    let orig = file.diagrams[0].model.as_ref().unwrap();
    let new = parsed.diagrams[0].model.as_ref().unwrap();

    for id in ["2", "3", "4"] {
        let o = orig.get(id).unwrap();
        let n = new.get(id).unwrap();
        assert_eq!(o.value, n.value, "value must match for id {id}");
        assert_eq!(o.geometry, n.geometry, "geometry must match for id {id}");
        assert_eq!(o.source, n.source, "source must match for id {id}");
        assert_eq!(o.target, n.target, "target must match for id {id}");
    }
}

#[test]
fn compressed_output_body_is_not_literal_xml() {
    // Sanity: the compressed body should not contain the literal
    // '<mxGraphModel' (which would mean we failed to compress).
    let file = sample_file();
    let xml = file.to_compressed_xml().unwrap();
    // The <diagram> opening tag exists, but its body must not be XML.
    let body_start = xml.find("<diagram").expect("diagram tag exists");
    let body_end = xml[body_start..]
        .find("</diagram>")
        .expect("closing diagram tag");
    let body = &xml[body_start..body_start + body_end];
    assert!(
        !body.contains("<mxGraphModel"),
        "diagram body should be compressed, not raw XML: {body}"
    );
}
