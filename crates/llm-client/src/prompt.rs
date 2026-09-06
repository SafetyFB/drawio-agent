//! Prompt template rendering for code generation and visual review.
//!
//! Prompt content is intentionally Phase 2-simple: stable section headers the
//! tests and the LLM can rely on. Full prompt engineering happens later.

/// System prompt for Draw.io XML generation calls.
pub fn codegen_system_prompt() -> &'static str {
    r#"You are a Draw.io XML generator. Output a complete <mxfile> document.

Draw.io conventions (must follow):
- Cell id="0" is the synthetic root (not rendered). Every visible cell must declare a parent.
- Use elastic containers (swimlane or container) so cells auto-wrap when adjacent.
- Prefer orthogonalEdgeStyle for hierarchical diagrams — avoid free-form angles.

Example fragment:
<mxCell id="2" value="Hello" style="rounded=0;whiteSpace=wrap;" vertex="1" parent="1">
  <mxGeometry x="100" y="100" width="120" height="60" as="geometry"/>
</mxCell>

Output ONLY the XML — no commentary, no markdown fences, no preamble."#
}

/// System prompt for patching selected cells in an existing diagram.
///
/// The LLM receives a `<scope>` section containing ONLY the cells it should
/// modify (plus their immediate neighborhood), and must return a complete
/// `<mxfile>` containing just those cells — with ids preserved so the server
/// can match them back. New children (e.g. an added arrow) are allowed with
/// fresh ids.
pub fn patch_system_prompt() -> String {
    r#"You are a Draw.io XML editor. The user has selected specific cells in an existing
diagram and asked you to modify them. You will receive a `<scope>` section containing
ONLY the cells you should change, plus their immediate neighborhood (edges, parent
containers).

Your job: output a complete <mxfile> document containing ONLY the modified versions of
the targeted cells. Do NOT include cells that weren't in the scope.

Rules:
- Preserve cell ids EXACTLY as they appear in the scope (the server matches by id).
- Preserve cell attributes (style, vertex/edge type, parent) unless the user asked
to change them.
- If the user asked to ADD a new element (e.g. 'add an arrow from A to B'), include it
with a new unique id (e.g. id="100") and parent pointing to the appropriate parent.
- Output valid Draw.io XML. Wrap in <mxfile host="app.diagrams.net"><diagram
id="patch" name="Page-1"><mxGraphModel>...</mxGraphModel></diagram></mxfile>.

Output ONLY the <mxfile>...</mxfile> document, no commentary."#
        .to_string()
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
    r#"You are a Draw.io diagram reviewer. Look at the rendered image AND the XML.

Detect ONLY these issue kinds (use exactly these strings):
- overlap: cells overlap by more than ~5px
- text_overflow: label spills outside its container box
- edge_crossing: edges cross without an obvious reason (orthogonal routing preferred)
- arrow_wrong: arrow direction or endpoint is wrong
- layout_bad: overall layout is confusing, asymmetric, or unbalanced

Severity levels (use the closest match):
- high: blocks understanding of the diagram
- medium: clearly visible flaw, should be fixed
- low: cosmetic or stylistic, optional

Output JSON with this exact shape:
{"verdict": "pass" | "issues", "issues": [{"kind": ..., "severity": ..., "cell_ids": [...], "description": ...}]}

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