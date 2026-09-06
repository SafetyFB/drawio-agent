//! TDD tests for prompt template rendering.

use drawio_agent_llm_client::{
    codegen_system_prompt, codegen_user_prompt, patch_system_prompt, review_system_prompt,
    review_user_prompt,
};

#[test]
fn codegen_system_prompt_mentions_xml() {
    let p = codegen_system_prompt();
    assert!(!p.is_empty(), "system prompt must not be empty");
    assert!(
        p.to_lowercase().contains("xml") || p.to_lowercase().contains("mxfile"),
        "system prompt should reference XML/mxfile: {p}"
    );
}

#[test]
fn codegen_user_prompt_includes_user_text() {
    let out = codegen_user_prompt("Draw a circle", None, None, None);
    assert!(out.contains("Draw a circle"));
}

#[test]
fn codegen_user_prompt_includes_current_xml_when_provided() {
    let out = codegen_user_prompt(
        "Add a node",
        Some("<mxfile><diagram id='x'/></mxfile>"),
        None,
        None,
    );
    assert!(out.contains("<mxfile"));
    assert!(out.contains("Add a node"));
}

#[test]
fn codegen_user_prompt_omits_current_xml_section_when_none() {
    let out = codegen_user_prompt("Just a fresh prompt", None, None, None);
    // Without current_xml, no "Current diagram:" header should appear.
    assert!(!out.contains("Current diagram"));
}

#[test]
fn codegen_user_prompt_includes_scope_when_provided() {
    let out = codegen_user_prompt(
        "Change color",
        None,
        Some("<mxCell id='5'/>"),
        None,
    );
    assert!(out.contains("<mxCell id='5'/>"));
    assert!(
        out.to_lowercase().contains("scope") || out.to_lowercase().contains("selection"),
        "scope section must be labeled: {out}"
    );
}

#[test]
fn codegen_user_prompt_includes_feedback_when_provided() {
    let feedback = vec![
        "Cell 5 overlaps cell 7".to_string(),
        "Text overflow on cell 12".to_string(),
    ];
    let out = codegen_user_prompt("Fix issues", None, None, Some(&feedback));
    assert!(out.contains("Cell 5 overlaps cell 7"));
    assert!(out.contains("Text overflow on cell 12"));
    assert!(out.contains("1."), "feedback should be numbered");
    assert!(out.contains("2."));
}

#[test]
fn codegen_user_prompt_handles_empty_feedback_list() {
    let empty: Vec<String> = vec![];
    let out = codegen_user_prompt("Just generate", None, None, Some(&empty));
    // Empty list = no feedback section.
    assert!(!out.to_lowercase().contains("feedback"));
    assert!(out.contains("Just generate"));
}

#[test]
fn codegen_user_prompt_preserves_user_xml_special_chars() {
    // The prompt must not break when current_xml contains characters
    // that would matter in markdown or shell contexts.
    let xml = "<mxCell value=\"a < b && c > d\"/>";
    let out = codegen_user_prompt("x", Some(xml), None, None);
    assert!(out.contains(xml));
}

#[test]
fn review_system_prompt_mentions_review_or_visual() {
    let p = review_system_prompt();
    assert!(!p.is_empty());
    assert!(
        p.to_lowercase().contains("review")
            || p.to_lowercase().contains("visual")
            || p.to_lowercase().contains("diagram"),
        "review system prompt should be relevant: {p}"
    );
}

#[test]
fn review_user_prompt_includes_xml() {
    let xml = "<mxfile><diagram/></mxfile>";
    let out = review_user_prompt(xml, &[]);
    assert!(out.contains(xml));
}

#[test]
fn review_user_prompt_includes_checks_list() {
    let checks = vec![
        "overlap".to_string(),
        "text_overflow".to_string(),
        "edge_crossing".to_string(),
    ];
    let out = review_user_prompt("<mxfile/>", &checks);
    assert!(out.contains("overlap"));
    assert!(out.contains("text_overflow"));
    assert!(out.contains("edge_crossing"));
}

#[test]
fn review_user_prompt_handles_empty_checks() {
    let out = review_user_prompt("<mxfile/>", &[]);
    // Empty list is fine; just don't crash and don't include a
    // spurious "Focus on these checks:" header for an empty list.
    assert!(!out.contains("- \n"));
    assert!(out.contains("<mxfile/>"));
}

#[test]
fn patch_system_prompt_mentions_scope_and_id_preservation() {
    let p = patch_system_prompt();
    assert!(!p.is_empty(), "patch system prompt must not be empty");
    let lower = p.to_lowercase();
    assert!(
        lower.contains("scope"),
        "must mention the <scope> section: {p}"
    );
    assert!(
        lower.contains("id") && lower.contains("preserve"),
        "must require preserving cell ids: {p}"
    );
    assert!(
        lower.contains("<mxfile>"),
        "must instruct a complete <mxfile> output: {p}"
    );
    assert!(
        lower.contains("add"),
        "must address adding new elements: {p}"
    );
    // Keep it lean — the prompt is sent on every patch call.
    assert!(
        p.split_whitespace().count() < 600,
        "patch system prompt should be under ~600 tokens"
    );
}

#[test]
fn prompts_are_deterministic() {
    // Same inputs -> same output. Important for trajectory / replay.
    let a = codegen_user_prompt("x", Some("<a/>"), Some("<b/>"), None);
    let b = codegen_user_prompt("x", Some("<a/>"), Some("<b/>"), None);
    assert_eq!(a, b);

    let c = review_user_prompt("<x/>", &["check1".to_string()]);
    let d = review_user_prompt("<x/>", &["check1".to_string()]);
    assert_eq!(c, d);
}
