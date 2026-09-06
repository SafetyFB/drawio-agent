//! TDD tests for MxGraphModel::remove_cells (fix-diff protocol deletions).

use drawio_agent_xml_core::MxFile;

const SAMPLE: &str = r#"<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="A" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="B" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" value="Group" vertex="1" parent="1">
          <mxGeometry x="50" y="50" width="400" height="200" as="geometry"/>
        </mxCell>
        <mxCell id="5" value="child" vertex="1" parent="4">
          <mxGeometry x="60" y="60" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="6" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
        <mxCell id="7" value="Unrelated" vertex="1" parent="1">
          <mxGeometry x="600" y="300" width="120" height="60" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

fn model() -> drawio_agent_xml_core::MxGraphModel {
    let file = MxFile::parse(SAMPLE.as_bytes()).expect("sample must parse");
    file.diagrams[0].model.clone().expect("model")
}

#[test]
fn removes_leaf_cell_and_leaves_others_untouched() {
    let mut m = model();
    let removed = m.remove_cells(&["3"]);
    assert!(removed.contains(&"3".to_string()));
    assert!(m.get("3").is_none());
    assert!(m.get("2").is_some());
    assert!(m.get("7").is_some(), "unrelated cells must survive");
}

#[test]
fn removes_container_with_descendants() {
    let mut m = model();
    let removed = m.remove_cells(&["4"]);
    assert!(removed.contains(&"4".to_string()));
    assert!(m.get("4").is_none());
    assert!(m.get("5").is_none(), "descendant must go with its container");
    assert!(m.get("2").is_some());
}

#[test]
fn removes_dangling_edges_of_deleted_endpoints() {
    let mut m = model();
    m.remove_cells(&["3"]);
    // Edge 6 connects 2 -> 3; with 3 gone it must be cleaned up.
    assert!(m.get("6").is_none(), "edge to a removed cell must be removed");
    assert!(m.get("2").is_some());
}

#[test]
fn never_removes_root_or_default_layer() {
    let mut m = model();
    let removed = m.remove_cells(&["0", "1"]);
    assert!(removed.is_empty(), "root/layer are protected");
    assert!(m.get("0").is_some());
    assert!(m.get("1").is_some());
}

#[test]
fn missing_ids_are_reported_as_not_removed() {
    let mut m = model();
    let removed = m.remove_cells(&["does-not-exist", "2"]);
    assert!(!removed.contains(&"does-not-exist".to_string()));
    assert!(removed.contains(&"2".to_string()));
}
