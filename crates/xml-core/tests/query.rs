//! TDD tests for cell query API: lookup by id across nested containers.

use drawio_agent_xml_core::{Cell, MxFile, MxGraphModel};

const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1"
                  tooltips="1" connect="1" arrows="1" fold="1" page="1"
                  pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Hello" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="World" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="10" value="Container" vertex="1" parent="1">
          <mxGeometry x="50" y="250" width="500" height="200" as="geometry"/>
        </mxCell>
        <mxCell id="11" value="ChildA" vertex="1" parent="10">
          <mxGeometry x="80" y="290" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="12" value="ChildB" vertex="1" parent="10">
          <mxGeometry x="200" y="290" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="4" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

fn sample_model() -> MxGraphModel {
    let file = MxFile::parse(SAMPLE.as_bytes()).expect("sample must parse");
    file.diagrams[0]
        .model
        .clone()
        .expect("sample diagram must have a model")
}

#[test]
fn get_root_returns_synthetic_root_cell() {
    let model = sample_model();
    let root = model.get("0").expect("root cell id=0 must exist");
    assert_eq!(root.id, "0");
    assert!(!root.children.is_empty(), "root must have children");
}

#[test]
fn get_top_level_layer_finds_default_layer() {
    let model = sample_model();
    let layer = model.get("1").expect("layer id=1 must exist");
    assert_eq!(layer.id, "1");
    assert_eq!(layer.parent.as_deref(), Some("0"));
    assert!(
        layer.children.len() >= 4,
        "layer must contain cells 2, 3, 10, 4"
    );
}

#[test]
fn get_finds_nested_cell_inside_container() {
    let model = sample_model();
    let child_a = model.get("11").expect("child id=11 must be findable");
    assert_eq!(child_a.value.as_deref(), Some("ChildA"));
    assert_eq!(child_a.parent.as_deref(), Some("10"));
}

#[test]
fn get_returns_none_for_missing_id() {
    let model = sample_model();
    assert!(model.get("999").is_none());
    assert!(model.get("not-an-id").is_none());
}

#[test]
fn get_is_consistent_for_same_id() {
    let model = sample_model();
    let a = model.get("2").unwrap();
    let b = model.get("2").unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.value, b.value);
    // Same memory address: pointers must match.
    assert!(std::ptr::eq(a as *const Cell, b as *const Cell));
}

#[test]
fn get_mut_allows_mutation() {
    let mut model = sample_model();
    {
        let cell = model.get_mut("2").expect("cell 2 must exist");
        cell.value = Some("Bonjour".into());
    }
    // Re-fetch and verify the mutation is visible.
    let cell = model.get("2").unwrap();
    assert_eq!(cell.value.as_deref(), Some("Bonjour"));
}

#[test]
fn get_mut_returns_none_for_missing_id() {
    let mut model = sample_model();
    assert!(model.get_mut("999").is_none());
}
