//! TDD tests for prompt refinement — richer domain knowledge, explicit
//! conventions, and full issue/severity taxonomy.

use drawio_agent_llm_client::{
    codegen_system_prompt, codegen_user_prompt, review_system_prompt, review_user_prompt,
};

#[test]
fn codegen_system_prompt_mentions_synthetic_root_convention() {
    // The model needs to know that cell id=\"0\" is the synthetic root
    // (not visible, every other cell must have a parent).
    let p = codegen_system_prompt();
    assert!(
        p.contains("id=\"0\"") || p.contains("id=0") || p.contains("synthetic root"),
        "codegen prompt must explain the synthetic-root convention: {p}"
    );
}

#[test]
fn codegen_system_prompt_explains_parent_attribute() {
    let p = codegen_system_prompt();
    assert!(
        p.contains("parent"),
        "codegen prompt must explain the parent attribute: {p}"
    );
}

#[test]
fn codegen_system_prompt_recommends_specific_shape_choices() {
    let p = codegen_system_prompt().to_lowercase();
    // Should mention containers, swimlane, or similar Draw.io layout primitives.
    assert!(
        p.contains("container") || p.contains("swimlane") || p.contains("group"),
        "codegen prompt should recommend concrete shape/layout choices: {p}"
    );
}

#[test]
fn codegen_system_prompt_includes_a_concrete_example() {
    let p = codegen_system_prompt();
    // An inline example is the most reliable way to anchor the model's output
    // format. Look for any <mxCell ...> or <mxfile fragment.
    assert!(
        p.contains("<mxCell") || p.contains("<mxfile") || p.contains("<diagram"),
        "codegen prompt should include at least one concrete XML fragment: {p}"
    );
}

#[test]
fn codegen_system_prompt_emphasizes_xml_only_output() {
    // When json_mode is off, the model must output ONLY the XML (no prose).
    let p = codegen_system_prompt().to_lowercase();
    assert!(
        p.contains("only the xml")
            || p.contains("only xml")
            || p.contains("no commentary")
            || p.contains("no markdown"),
        "codegen prompt must tell the model to emit XML only: {p}"
    );
}

#[test]
fn codegen_user_prompt_handles_three_optional_sections_independently() {
    // Each optional section can appear alone or be skipped.
    let only_xml = codegen_user_prompt("Draw 3 nodes", Some("<mxfile/>"), None, None);
    assert!(only_xml.contains("<mxfile"));
    assert!(!only_xml.contains("Scope"));

    let only_scope = codegen_user_prompt("Color cells red", None, Some("<mxCell id=\"5\"/>"), None);
    assert!(only_scope.contains("Scope"));
    assert!(!only_scope.contains("Current diagram"));

    let only_feedback = codegen_user_prompt(
        "Fix overlap",
        None,
        None,
        Some(&["cell 5 overlaps 7".to_string()]),
    );
    assert!(only_feedback.contains("overlap"));
    assert!(!only_feedback.contains("Current diagram"));
    assert!(!only_feedback.contains("Scope"));
}

#[test]
fn codegen_user_prompt_combines_all_sections_in_order() {
    let out = codegen_user_prompt(
        "Make a flow",
        Some("<mxfile>v1</mxfile>"),
        Some("<mxCell id=\"5\"/>"),
        Some(&["fix cell 12".to_string()]),
    );
    let pos_xml = out.find("<mxfile>v1").expect("current_xml present");
    let pos_scope = out.find("Scope").expect("scope present");
    let pos_feedback = out.find("fix cell 12").expect("feedback present");
    // Order: user_prompt -> current_xml -> scope -> feedback
    assert!(pos_xml < pos_scope);
    assert!(pos_scope < pos_feedback);
}

#[test]
fn review_system_prompt_lists_all_five_issue_kinds() {
    let p = review_system_prompt();
    for kind in [
        "overlap",
        "text_overflow",
        "edge_crossing",
        "arrow_wrong",
        "layout_bad",
    ] {
        assert!(
            p.contains(kind),
            "review prompt missing issue kind '{kind}': {p}"
        );
    }
}

#[test]
fn review_system_prompt_defines_severity_levels() {
    let p = review_system_prompt();
    let mut found = 0;
    for lvl in ["high", "medium", "low"] {
        if p.contains(lvl) {
            found += 1;
        }
    }
    assert!(
        found >= 2,
        "review prompt must define at least 2 severity levels (high/medium/low): {p}"
    );
}

#[test]
fn review_system_prompt_specifies_json_output_schema() {
    let p = review_system_prompt();
    // Should mention JSON shape with verdict and issues array.
    assert!(p.contains("verdict"), "review prompt must mention verdict field: {p}");
    assert!(p.contains("issues"), "review prompt must mention issues array: {p}");
    assert!(
        p.contains("cell_ids") || p.contains("cell ids"),
        "review prompt must include cell_ids in issue schema: {p}"
    );
}

#[test]
fn review_system_prompt_emphasizes_visual_image_analysis() {
    let p = review_system_prompt().to_lowercase();
    assert!(
        p.contains("image") || p.contains("render") || p.contains("visual"),
        "review prompt must tell the model to look at the actual rendered image: {p}"
    );
}

#[test]
fn review_user_prompt_includes_xml_and_checks_in_stable_order() {
    let out = review_user_prompt(
        "<mxfile/>",
        &["overlap".to_string(), "text_overflow".to_string()],
    );
    let pos_xml = out.find("<mxfile/>").expect("xml present");
    let pos_checks = out.find("overlap").expect("checks present");
    assert!(pos_xml < pos_checks, "xml must come before checks list");
    assert!(out.contains("text_overflow"));
}

#[test]
fn prompts_are_substantive_non_trivial() {
    // Sanity: prompts must have meaningful content, not one-line stubs.
    assert!(
        codegen_system_prompt().len() >= 300,
        "codegen system prompt must be at least 300 chars, got {}",
        codegen_system_prompt().len()
    );
    assert!(
        review_system_prompt().len() >= 300,
        "review system prompt must be at least 300 chars, got {}",
        review_system_prompt().len()
    );
}

#[test]
fn prompts_are_stable_across_calls() {
    // Same call -> same output. Important for replay determinism.
    let a1 = codegen_system_prompt();
    let a2 = codegen_system_prompt();
    assert_eq!(a1, a2);
    let r1 = review_system_prompt();
    let r2 = review_system_prompt();
    assert_eq!(r1, r2);
}
