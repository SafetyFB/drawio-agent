//! TDD tests for MxGraphModel::apply_cell_diff (fix-diff protocol merge).

use drawio_agent_xml_core::{Cell, MxFile, MxGraphModel};

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
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="2" target="3">
          <mxGeometry relative="1" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

fn model() -> MxGraphModel {
    let file = MxFile::parse(SAMPLE.as_bytes()).expect("sample must parse");
    file.diagrams[0].model.clone().expect("model")
}

/// Cell list collector matching what the server extracts from the model's
/// fix-diff response: every cell except synthetic root/layer.
fn collect_visible(file: &MxFile) -> Vec<Cell> {
    fn walk(cell: &Cell, out: &mut Vec<Cell>) {
        if cell.id != "0" && cell.id != "1" {
            out.push(cell.clone());
        }
        for c in &cell.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    let model = file.diagrams[0].model.as_ref().expect("model");
    walk(&model.root, &mut out);
    out
}

#[test]
fn diff_updates_only_listed_cells() {
    let mut m = model();
    let mut changed = m.get("2").unwrap().clone();
    changed.value = Some("A2".into());
    changed.geometry.as_mut().unwrap().x = 150.0;

    let result = m.apply_cell_diff(&[changed], &[]);
    assert_eq!(result.updated, vec!["2"]);
    assert!(result.added.is_empty());

    let cell = m.get("2").unwrap();
    assert_eq!(cell.value.as_deref(), Some("A2"));
    assert_eq!(cell.geometry.as_ref().unwrap().x, 150.0);
    // Untouched cell: byte-identical attributes.
    let b = m.get("3").unwrap();
    assert_eq!(b.value.as_deref(), Some("B"));
    assert_eq!(b.geometry.as_ref().unwrap().x, 300.0);
}

#[test]
fn diff_inserts_new_cells_and_deletes_removed() {
    let mut m = model();

    let mut fresh = Cell::new("9");
    fresh.value = Some("New".into());
    fresh.vertex = true;
    fresh.parent = Some("1".into());
    fresh.geometry = Some(drawio_agent_xml_core::Geometry {
        x: 500.0,
        y: 200.0,
        width: 80.0,
        height: 40.0,
        ..Default::default()
    });

    let result = m.apply_cell_diff(&[fresh], &["3"]);
    assert_eq!(result.added, vec!["9"]);
    assert!(m.get("3").is_none(), "removed cell must be gone");
    assert!(m.get("4").is_none(), "edge to removed cell must be cleaned");
    assert!(m.get("2").is_some());
    assert!(m.get("9").is_some());
}

#[test]
fn diff_with_full_document_echo_is_a_no_op_after_canonicalization() {
    // A model that ignores the diff rule and echoes everything back must
    // still produce a valid (unchanged) result — and the server's no-op
    // detection sees through it.
    let mut m = model();
    let before = m.clone();
    let file = MxFile::parse(SAMPLE.as_bytes()).unwrap();
    let all = collect_visible(&file);
    let result = m.apply_cell_diff(&all, &[]);
    assert!(result.added.is_empty());
    assert_eq!(result.updated.len(), 3);
    // Structural equality of every cell's attributes.
    for id in ["2", "3", "4"] {
        let a = before.get(id).unwrap();
        let b = m.get(id).unwrap();
        assert_eq!(a.value, b.value);
        assert_eq!(a.style, b.style);
        assert_eq!(a.geometry, b.geometry);
    }
}
