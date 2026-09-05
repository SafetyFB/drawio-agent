//! TDD tests for parse behavior.

use drawio_agent_xml_core::MxFile;

/// A minimal valid uncompressed mxfile with one diagram containing two nodes
/// and one edge between them.
const SAMPLE_UNCOMPRESSED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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

#[test]
fn parse_uncompressed_mxfile_single_diagram() {
    let file = MxFile::parse(SAMPLE_UNCOMPRESSED.as_bytes())
        .expect("should parse uncompressed mxfile");

    assert_eq!(file.diagrams.len(), 1, "exactly one diagram expected");

    let diagram = &file.diagrams[0];
    assert_eq!(diagram.id, "page-1");
    assert_eq!(diagram.name, "Page-1");
}

#[test]
fn parse_uncompressed_populates_root_cells() {
    let file = MxFile::parse(SAMPLE_UNCOMPRESSED.as_bytes()).unwrap();
    let model = file.diagrams[0]
        .model
        .as_ref()
        .expect("diagram should have parsed model");

    // Convention: cell id="0" is the synthetic root, id="1" is the default layer.
    assert_eq!(model.root.id, "0");
    assert!(!model.root.children.is_empty(), "root must have children");

    let layer = model
        .root
        .children
        .iter()
        .find(|c| c.id == "1")
        .expect("default layer cell id=1 must exist");
    assert!(!layer.children.is_empty(), "layer must have children");
}

#[test]
fn parse_uncompressed_extracts_node_geometry_and_value() {
    let file = MxFile::parse(SAMPLE_UNCOMPRESSED.as_bytes()).unwrap();
    let model = file.diagrams[0].model.as_ref().unwrap();

    let hello = find_cell(model, "2").expect("cell id=2 must exist");
    assert_eq!(hello.value.as_deref(), Some("Hello"));
    assert!(hello.vertex);
    assert!(!hello.edge);

    let geom = hello.geometry.as_ref().expect("hello must have geometry");
    assert_eq!(geom.x, 100.0);
    assert_eq!(geom.y, 100.0);
    assert_eq!(geom.width, 120.0);
    assert_eq!(geom.height, 60.0);
}

#[test]
fn parse_uncompressed_extracts_edge_source_target() {
    let file = MxFile::parse(SAMPLE_UNCOMPRESSED.as_bytes()).unwrap();
    let model = file.diagrams[0].model.as_ref().unwrap();

    let edge = find_cell(model, "4").expect("edge id=4 must exist");
    assert!(edge.edge);
    assert!(!edge.vertex);
    assert_eq!(edge.source.as_deref(), Some("2"));
    assert_eq!(edge.target.as_deref(), Some("3"));
}

// ---------------------------------------------------------------------------
// Compressed (base64 + raw deflate) diagrams — Draw.io's on-disk format.
// ---------------------------------------------------------------------------

/// Build a compressed diagram shell around the given mxGraphModel XML body.
/// Mirrors how the Draw.io desktop app serializes diagrams.
fn compressed_mxfile(inner: &str) -> String {
    use base64::Engine;
    let compressed = miniz_oxide::deflate::compress_to_vec(inner.as_bytes(), 6);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&compressed);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<mxfile host="app.diagrams.net">
  <diagram id="page-1" name="Page-1">{encoded}</diagram>
</mxfile>"#
    )
}

const SAMPLE_MODEL_XML: &str = r#"<mxGraphModel dx="800" dy="600" grid="1" gridSize="10"
                                  guides="1" tooltips="1" connect="1" arrows="1"
                                  fold="1" page="1" pageScale="1" pageWidth="850"
                                  pageHeight="1100" math="0" shadow="0">
  <root>
    <mxCell id="0"/>
    <mxCell id="1" parent="0"/>
    <mxCell id="2" value="Hello" style="rounded=0;" vertex="1" parent="1">
      <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
    </mxCell>
    <mxCell id="3" value="World" style="rounded=0;" vertex="1" parent="1">
      <mxGeometry x="300" y="100" width="120" height="60" as="geometry"/>
    </mxCell>
    <mxCell id="4" edge="1" parent="1" source="2" target="3">
      <mxGeometry relative="1" as="geometry"/>
    </mxCell>
  </root>
</mxGraphModel>"#;

#[test]
fn parse_compressed_mxfile_single_diagram() {
    let mxfile = compressed_mxfile(SAMPLE_MODEL_XML);
    let file = MxFile::parse(mxfile.as_bytes())
        .expect("should parse compressed mxfile");

    assert_eq!(file.diagrams.len(), 1);
    let diagram = &file.diagrams[0];
    assert_eq!(diagram.id, "page-1");
    assert_eq!(diagram.name, "Page-1");
}

#[test]
fn parse_compressed_mxfile_preserves_cell_structure() {
    let mxfile = compressed_mxfile(SAMPLE_MODEL_XML);
    let file = MxFile::parse(mxfile.as_bytes()).unwrap();
    let model = file.diagrams[0]
        .model
        .as_ref()
        .expect("compressed diagram should yield a parsed model");

    // Cell hierarchy must survive compression round-trip.
    assert_eq!(model.root.id, "0");
    let layer = model.root.children.iter().find(|c| c.id == "1").unwrap();
    assert_eq!(layer.children.len(), 3, "layer must contain 3 children");
}

#[test]
fn parse_compressed_mxfile_preserves_geometry_and_values() {
    let mxfile = compressed_mxfile(SAMPLE_MODEL_XML);
    let file = MxFile::parse(mxfile.as_bytes()).unwrap();
    let model = file.diagrams[0].model.as_ref().unwrap();

    let hello = find_cell(model, "2").expect("cell id=2 must exist after decompress");
    assert_eq!(hello.value.as_deref(), Some("Hello"));
    assert!(hello.vertex);
    let geom = hello.geometry.as_ref().expect("geometry preserved through compress");
    assert_eq!(geom.x, 100.0);
    assert_eq!(geom.width, 120.0);

    let edge = find_cell(model, "4").expect("edge must exist after decompress");
    assert!(edge.edge);
    assert_eq!(edge.source.as_deref(), Some("2"));
    assert_eq!(edge.target.as_deref(), Some("3"));
}

#[test]
fn parse_compressed_matches_uncompressed_structure() {
    // Same inner XML, two encodings: results must be deeply equal.
    let uncompressed_file = MxFile::parse(SAMPLE_UNCOMPRESSED.as_bytes()).unwrap();
    let compressed_file = MxFile::parse(compressed_mxfile(SAMPLE_MODEL_XML).as_bytes()).unwrap();

    let a = uncompressed_file.diagrams[0].model.as_ref().unwrap();
    let b = compressed_file.diagrams[0].model.as_ref().unwrap();

    assert_eq!(a.root.id, b.root.id);
    assert_eq!(a.root.children.len(), b.root.children.len());
}

/// Depth-first search by cell id.
fn find_cell<'a>(model: &'a drawio_agent_xml_core::MxGraphModel, id: &str) 
    -> Option<&'a drawio_agent_xml_core::Cell> 
{
    fn walk<'a>(cell: &'a drawio_agent_xml_core::Cell, id: &str) 
        -> Option<&'a drawio_agent_xml_core::Cell> 
    {
        if cell.id == id {
            return Some(cell);
        }
        for child in &cell.children {
            if let Some(found) = walk(child, id) {
                return Some(found);
            }
        }
        None
    }
    walk(&model.root, id)
}
