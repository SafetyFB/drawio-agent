//! TDD tests for model validation: structural integrity checks.

use drawio_agent_xml_core::{Cell, MxFile, MxGraphModel, ValidationError};

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
fn validate_valid_model_returns_empty_report() {
    let model = sample_model();
    let report = model.validate();
    assert!(
        report.is_ok(),
        "valid model should produce no errors, got: {report}"
    );
    assert!(report.errors.is_empty());
}

#[test]
fn validate_detects_missing_root() {
    let mut model = sample_model();
    // Rename the synthetic root from "0" so no cell has id "0" anymore.
    model.root.id = "5".to_string();

    let report = model.validate();
    assert!(!report.is_ok());
    assert!(
        report
            .errors
            .iter()
            .any(|e| matches!(e, ValidationError::MissingRoot)),
        "expected MissingRoot in {report}"
    );
}

#[test]
fn validate_detects_duplicate_ids() {
    let mut model = sample_model();
    // Push a copy of cell 2 onto the layer (id collision).
    let dup = model.get("2").unwrap().clone();
    let layer = model.get_mut("1").expect("layer exists");
    layer.children.push(dup);

    let report = model.validate();
    let duplicates: Vec<&str> = report
        .errors
        .iter()
        .filter_map(|e| match e {
            ValidationError::DuplicateId(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        duplicates.contains(&"2"),
        "expected DuplicateId(\"2\") in {report}"
    );
}

#[test]
fn validate_detects_missing_edge_source() {
    let mut model = sample_model();
    let edge = model.get_mut("4").unwrap();
    edge.source = Some("999".to_string());

    let report = model.validate();
    let missing_sources: Vec<(&str, &str)> = report
        .errors
        .iter()
        .filter_map(|e| match e {
            ValidationError::EdgeMissingSource { edge_id, source } => {
                Some((edge_id.as_str(), source.as_str()))
            }
            _ => None,
        })
        .collect();
    assert!(
        missing_sources.contains(&("4", "999")),
        "expected EdgeMissingSource in {report}"
    );
}

#[test]
fn validate_detects_missing_edge_target() {
    let mut model = sample_model();
    let edge = model.get_mut("4").unwrap();
    edge.target = Some("888".to_string());

    let report = model.validate();
    let missing_targets: Vec<(&str, &str)> = report
        .errors
        .iter()
        .filter_map(|e| match e {
            ValidationError::EdgeMissingTarget { edge_id, target } => {
                Some((edge_id.as_str(), target.as_str()))
            }
            _ => None,
        })
        .collect();
    assert!(
        missing_targets.contains(&("4", "888")),
        "expected EdgeMissingTarget in {report}"
    );
}

#[test]
fn validate_collects_multiple_errors() {
    let mut model = sample_model();
    // Three independent problems at once.
    model.root.id = "99".to_string(); // MissingRoot
    let edge = model.get_mut("4").unwrap();
    edge.source = Some("777".to_string()); // EdgeMissingSource
    edge.target = Some("888".to_string()); // EdgeMissingTarget

    let report = model.validate();

    let has_missing_root = report
        .errors
        .iter()
        .any(|e| matches!(e, ValidationError::MissingRoot));
    let has_missing_source = report
        .errors
        .iter()
        .any(|e| matches!(e, ValidationError::EdgeMissingSource { source, .. } if source == "777"));
    let has_missing_target = report
        .errors
        .iter()
        .any(|e| matches!(e, ValidationError::EdgeMissingTarget { target, .. } if target == "888"));

    assert!(has_missing_root, "expected MissingRoot in {report}");
    assert!(has_missing_source, "expected EdgeMissingSource in {report}");
    assert!(has_missing_target, "expected EdgeMissingTarget in {report}");
}

#[test]
fn validate_edge_with_self_loop_is_allowed() {
    // An edge can legitimately point back at itself in some Draw.io flows.
    let mut model = sample_model();
    let edge = model.get_mut("4").unwrap();
    edge.source = Some("2".to_string());
    edge.target = Some("2".to_string());

    let report = model.validate();
    assert!(report.is_ok(), "self-loop should validate cleanly: {report}");
}

#[test]
fn validate_does_not_mutate_model() {
    let model = sample_model();
    let _ = model.validate();
    // Re-validate to ensure idempotency and no internal state mutation.
    let report = model.validate();
    assert!(report.is_ok());
}

// Helper trait import to silence the unused warning for `Cell` on some
// toolchains.
#[allow(dead_code)]
fn _phantom(_: Cell) {}
