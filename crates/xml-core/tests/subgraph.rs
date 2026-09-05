//! TDD tests for subgraph extraction (selection-based editing scope).

use drawio_agent_xml_core::{MxFile, MxGraphModel, Subgraph};

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

fn primary_ids(sub: &Subgraph) -> Vec<String> {
    sub.primary.iter().map(|c| c.id.clone()).collect()
}

#[test]
fn extract_two_connected_cells_includes_their_edge() {
    let model = sample_model();
    let sub = model.extract_subgraph(&["2", "3"]);

    assert_eq!(primary_ids(&sub), vec!["2", "3"]);
    assert_eq!(sub.edges.len(), 1);
    assert_eq!(sub.edges[0].id, "4");
    assert!(sub.context.is_empty(), "no external neighbors for [2,3]");
    assert!(sub.missing.is_empty());
}

#[test]
fn extract_single_cell_includes_edge_neighbors_as_context() {
    let model = sample_model();
    let sub = model.extract_subgraph(&["2"]);

    assert_eq!(primary_ids(&sub), vec!["2"]);
    assert_eq!(sub.edges.len(), 1);
    assert_eq!(sub.edges[0].id, "4");

    let ctx_ids: Vec<&str> = sub.context.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ctx_ids, vec!["3"], "cell 3 is the other endpoint of edge 4");
    assert!(sub.missing.is_empty());
}

#[test]
fn extract_isolated_cell_has_no_edges_or_context() {
    let model = sample_model();
    let sub = model.extract_subgraph(&["11"]);

    assert_eq!(primary_ids(&sub), vec!["11"]);
    assert!(sub.edges.is_empty(), "no edges touching cell 11");
    assert!(sub.context.is_empty());
}

#[test]
fn extract_reports_missing_ids() {
    let model = sample_model();
    let sub = model.extract_subgraph(&["2", "999"]);

    assert_eq!(primary_ids(&sub), vec!["2"]);
    assert_eq!(sub.edges.len(), 1);
    assert_eq!(sub.context.len(), 1);
    assert_eq!(sub.missing, vec!["999".to_string()]);
}

#[test]
fn extract_empty_input_returns_empty_subgraph() {
    let model = sample_model();
    let sub = model.extract_subgraph(&[]);

    assert!(sub.primary.is_empty());
    assert!(sub.edges.is_empty());
    assert!(sub.context.is_empty());
    assert!(sub.missing.is_empty());
}

#[test]
fn extract_all_missing_returns_empty_primary() {
    let model = sample_model();
    let sub = model.extract_subgraph(&["999", "888"]);

    assert!(sub.primary.is_empty());
    assert!(sub.edges.is_empty());
    assert!(sub.context.is_empty());
    assert_eq!(sub.missing, vec!["999".to_string(), "888".to_string()]);
}

#[test]
fn extract_deduplicates_input_ids() {
    // Requesting the same cell twice should not produce duplicates.
    let model = sample_model();
    let sub = model.extract_subgraph(&["2", "2", "3"]);

    assert_eq!(primary_ids(&sub), vec!["2", "3"]);
}

#[test]
fn extract_does_not_include_unrelated_cells_in_context() {
    // Selecting cell 11 (inside container 10) should NOT pull in
    // unrelated cells like 2, 3, or 12.
    let model = sample_model();
    let sub = model.extract_subgraph(&["11"]);

    let ctx_ids: Vec<&str> = sub.context.iter().map(|c| c.id.as_str()).collect();
    for forbidden in ["2", "3", "12"] {
        assert!(
            !ctx_ids.contains(&forbidden),
            "context must not contain {forbidden}"
        );
    }
}
