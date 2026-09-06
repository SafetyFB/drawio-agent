//! TDD tests for subgraph application: modify scoped cells, preserve others.

use drawio_agent_xml_core::{Cell, Geometry, MxFile, MxGraphModel, Subgraph};

const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1"
                  tooltips="1" connect="1" arrows="1" fold="1" page="1"
                  pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="2" value="Hello" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="3" value="World" style="rounded=0;" vertex="1" parent="1">
          <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
        </mxCell>
        <mxCell id="4" style="edgeStyle=orthogonalEdgeStyle;" edge="1"
                parent="1" source="2" target="3">
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
fn apply_updates_existing_primary_cell_value() {
    let mut model = sample_model();
    let mut sub = model.extract_subgraph(&["2"]);
    sub.primary[0].value = Some("Bonjour".into());

    let result = model.apply_subgraph(&sub);

    // Both cell 2 (primary) and edge 4 (touches 2) are mutable scope.
    assert_eq!(result.updated, vec!["2".to_string(), "4".to_string()]);
    assert!(result.added.is_empty());
    assert_eq!(result.context_ignored, 1);

    let cell = model.get("2").unwrap();
    assert_eq!(cell.value.as_deref(), Some("Bonjour"));
}

#[test]
fn apply_updates_existing_primary_cell_geometry() {
    let mut model = sample_model();
    let mut sub = model.extract_subgraph(&["2"]);
    sub.primary[0].geometry = Some(Geometry {
        x: 999.0,
        y: 888.0,
        width: 77.0,
        height: 66.0,
    });

    model.apply_subgraph(&sub);

    let cell = model.get("2").unwrap();
    let geom = cell.geometry.as_ref().expect("geometry preserved");
    assert_eq!(geom.x, 999.0);
    assert_eq!(geom.y, 888.0);
    assert_eq!(geom.width, 77.0);
    assert_eq!(geom.height, 66.0);
}

#[test]
fn apply_inserts_new_primary_cell() {
    let mut model = sample_model();
    let mut new_cell = Cell::new("99");
    new_cell.value = Some("New".into());
    new_cell.vertex = true;
    new_cell.parent = Some("1".into());
    new_cell.geometry = Some(Geometry {
        x: 500.0,
        y: 100.0,
        width: 80.0,
        height: 40.0,
    });

    let sub = Subgraph {
        primary: vec![new_cell.clone()],
        edges: vec![],
        context: vec![],
        parents: vec![],
        missing: vec![],
    };

    let result = model.apply_subgraph(&sub);

    assert_eq!(result.added, vec!["99".to_string()]);
    assert!(result.updated.is_empty());

    let inserted = model.get("99").expect("new cell must be reachable");
    assert_eq!(inserted.value.as_deref(), Some("New"));
    assert!(inserted.vertex);
}

#[test]
fn apply_ignores_context_cell_changes() {
    let mut model = sample_model();
    let mut sub = model.extract_subgraph(&["2"]);
    // Tamper with the context cell (3) inside the subgraph.
    sub.context[0].value = Some("MUST_NOT_APPEAR".into());
    sub.context[0].geometry = Some(Geometry {
        x: -1.0,
        y: -1.0,
        width: 1.0,
        height: 1.0,
    });

    let result = model.apply_subgraph(&sub);
    assert_eq!(result.context_ignored, 1);

    let ctx = model.get("3").expect("context cell still present");
    assert_eq!(
        ctx.value.as_deref(),
        Some("World"),
        "context cell value must not change"
    );
    assert_eq!(
        ctx.geometry.as_ref().map(|g| g.x),
        Some(300.0),
        "context cell geometry must not change"
    );
}

#[test]
fn apply_preserves_unrelated_cells() {
    let mut model = sample_model();

    // Snapshot every cell's serializable state before apply.
    type Snapshot = (String, Option<String>, Option<String>, Option<Geometry>);
    let snapshot: Vec<Snapshot> = [
        "0", "1", "2", "3", "4",
    ]
    .iter()
    .map(|id| {
        let c = model.get(id).unwrap();
        (
            c.id.clone(),
            c.value.clone(),
            c.style.clone(),
            c.geometry.clone(),
        )
    })
    .collect();

    // Modify only cell 2.
    let mut sub = model.extract_subgraph(&["2"]);
    sub.primary[0].value = Some("Modified".into());

    model.apply_subgraph(&sub);

    // Every cell NOT touched by the subgraph must match the snapshot.
    for (id, value, style, geom) in &snapshot {
        if id == "2" {
            continue;
        }
        let c = model.get(id).expect("cell still present");
        assert_eq!(&c.value, value, "value changed for {id}");
        assert_eq!(&c.style, style, "style changed for {id}");
        assert_eq!(&c.geometry, geom, "geometry changed for {id}");
    }
}

#[test]
fn apply_updates_edge_style() {
    let mut model = sample_model();
    let mut sub = model.extract_subgraph(&["2", "3"]);
    sub.edges[0].style = Some("edgeStyle=elbowEdgeStyle;html=1;".into());

    let result = model.apply_subgraph(&sub);
    assert!(
        result.updated.contains(&"4".to_string()),
        "edge 4 must be reported as updated"
    );

    let edge = model.get("4").unwrap();
    assert_eq!(edge.style.as_deref(), Some("edgeStyle=elbowEdgeStyle;html=1;"));
}

#[test]
fn apply_preserves_children_of_updated_cell() {
    // Cell 10 has children 11, 12. Apply a subgraph that modifies cell 10
    // but does NOT include 11, 12. The children must survive.
    const WITH_CONTAINER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">
    <mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1"
                  tooltips="1" connect="1" arrows="1" fold="1" page="1"
                  pageScale="1" pageWidth="850" pageHeight="1100" math="0" shadow="0">
      <root>
        <mxCell id="0"/>
        <mxCell id="1" parent="0"/>
        <mxCell id="10" value="Container" vertex="1" parent="1">
          <mxGeometry x="50" y="250" width="500" height="200" as="geometry"/>
        </mxCell>
        <mxCell id="11" value="ChildA" vertex="1" parent="10">
          <mxGeometry x="80" y="290" width="80" height="40" as="geometry"/>
        </mxCell>
        <mxCell id="12" value="ChildB" vertex="1" parent="10">
          <mxGeometry x="200" y="290" width="80" height="40" as="geometry"/>
        </mxCell>
      </root>
    </mxGraphModel>
  </diagram>
</mxfile>"#;

    let file = MxFile::parse(WITH_CONTAINER.as_bytes()).unwrap();
    let mut model = file.diagrams[0].model.clone().unwrap();

    let mut sub = model.extract_subgraph(&["10"]);
    sub.primary[0].value = Some("Updated".into());

    model.apply_subgraph(&sub);

    let container = model.get("10").unwrap();
    assert_eq!(container.value.as_deref(), Some("Updated"));
    let child_ids: Vec<&str> = container.children.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(child_ids, vec!["11", "12"], "children must survive apply");
}

#[test]
fn apply_empty_subgraph_is_noop() {
    let mut model = sample_model();
    let sub = Subgraph::default();
    let result = model.apply_subgraph(&sub);

    assert!(result.updated.is_empty());
    assert!(result.added.is_empty());
    assert_eq!(result.context_ignored, 0);
}

#[test]
fn apply_reports_context_count_even_when_primary_empty() {
    let mut model = sample_model();
    let sub = Subgraph {
        primary: vec![],
        edges: vec![],
        context: vec![Cell::new("3")],
        parents: vec![],
        missing: vec![],
    };

    let result = model.apply_subgraph(&sub);
    assert_eq!(result.context_ignored, 1);
    // Cell 3 must still be untouched.
    assert_eq!(model.get("3").unwrap().value.as_deref(), Some("World"));
}
