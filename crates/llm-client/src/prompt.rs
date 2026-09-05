//! Prompt template rendering for code generation and visual review.
//!
//! Prompt content is intentionally Phase 2-simple: stable section headers the
//! tests and the LLM can rely on. Full prompt engineering happens later.

/// System prompt for Draw.io XML generation calls.
pub fn codegen_system_prompt() -> &'static str {
    r#"You are a Draw.io XML generator. Output complete <mxfile> XML.
- Use elastic containers (swimlane) for auto-wrapping when possible.
- Prefer relative layout hints over hardcoded coordinates that cause overlap.
- Output ONLY the XML — no commentary, no markdown fences."#
}

/// User prompt for a single XML generation call.
///
/// The raw `user_prompt` is always included. Optional sections are appended in
/// a stable order (current diagram, scope, feedback) and only when present.
pub fn codegen_user_prompt(
    user_prompt: &str,
    current_xml: Option<&str>,
    scope: Option<&str>,
    feedback: Option<&[String]>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "Generate Draw.io XML for the following request:\n{user_prompt}"
    ));

    if let Some(xml) = current_xml {
        parts.push(format!("Current diagram XML:\n{xml}"));
    }

    if let Some(selection) = scope {
        parts.push(format!(
            "Scope — selected subgraph XML to patch:\n{selection}"
        ));
    }

    if let Some(items) = feedback {
        if !items.is_empty() {
            let numbered = items
                .iter()
                .enumerate()
                .map(|(i, item)| format!("{}. {item}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!("Issues to address:\n{numbered}"));
        }
    }

    parts.join("\n\n")
}

/// System prompt for visual review calls.
pub fn review_system_prompt() -> &'static str {
    r#"You are a Draw.io diagram reviewer. Examine the rendered image and the XML.
Detect and report: overlap, text_overflow, edge_crossing, arrow_wrong, layout_bad.
Output JSON: {"verdict": "pass" | "issues", "issues": [{kind, severity, cell_ids, description}]}.
Be conservative — only flag real, visible issues. Do not modify the XML yourself."#
}

/// User prompt for a visual review call.
///
/// The `xml` is included verbatim. The `checks` list is rendered as bullet
/// points only when non-empty.
pub fn review_user_prompt(xml: &str, checks: &[String]) -> String {
    let mut out = format!(
        "Review the rendered diagram against the XML below.\n\nXML:\n{xml}"
    );

    if !checks.is_empty() {
        let bullets = checks
            .iter()
            .map(|check| format!("- {check}"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!("\n\nFocus on these checks:\n{bullets}"));
    }

    out
}