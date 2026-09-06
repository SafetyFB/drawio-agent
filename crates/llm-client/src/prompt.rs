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

Available shapes (use the `style` attribute):
- Rectangle: `rounded=0;whiteSpace=wrap;html=1;` (default for process steps)
- Rounded rectangle: `rounded=1;whiteSpace=wrap;html=1;` (for service / role nodes)
- Ellipse: `shape=ellipse;whiteSpace=wrap;html=1;` (for start/end nodes)
- Diamond / decision (rhombus): `shape=mxgraph.flowchart.decision;whiteSpace=wrap;html=1;` (for if/then branches)
- Cylinder: `shape=cylinder3;whiteSpace=wrap;boundedLbl=1;backgroundOutline=1;size=15;` (for databases / data stores)
- Hexagon: `shape=hexagon;perimeter=hexagonPerimeter2;whiteSpace=wrap;html=1;` (for preparation / initial states)
- Parallelogram: `shape=parallelogram;perimeter=parallelogramPerimeter;whiteSpace=wrap;html=1;` (for input/output)
- Document: `shape=note;whiteSpace=wrap;html=1;backgroundOutline=1;darkOpacity=0.05;` (for documents / records)
- Cloud: `ellipse;shape=cloud;whiteSpace=wrap;html=1;` (for external services)
- Swimlane (group): `swimlane;html=1;startSize=24;` (for grouping related steps)

Edges (mxCell edge="1"):
- Default: `endArrow=classic;html=1;rounded=0;`
- Decision branch (yes/no): add `endArrow=classic;startArrow=classic;` for the "yes" branch and `endArrow=none;` for the implicit "no" continuation
- Hierarchical: `endArrow=classic;html=1;rounded=1;edgeStyle=orthogonalEdgeStyle;`

Example — a 4-node flowchart with a decision:
<mxCell id="2" value="Start" style="rounded=1;whiteSpace=wrap;html=1;" vertex="1" parent="1">
  <mxGeometry x="200" y="80" width="120" height="60" as="geometry"/>
</mxCell>
<mxCell id="3" value="Process A" style="rounded=0;whiteSpace=wrap;html=1;" vertex="1" parent="1">
  <mxGeometry x="180" y="180" width="120" height="60" as="geometry"/>
</mxCell>
<mxCell id="4" value="Decision" style="shape=mxgraph.flowchart.decision;whiteSpace=wrap;html=1;" vertex="1" parent="1">
  <mxGeometry x="200" y="280" width="100" height="80" as="geometry"/>
</mxCell>
<mxCell id="5" value="Process B" style="rounded=0;whiteSpace=wrap;html=1;" vertex="1" parent="1">
  <mxGeometry x="360" y="290" width="120" height="60" as="geometry"/>
</mxCell>
<mxCell id="6" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="2" target="3">
  <mxGeometry relative="1" as="geometry"/>
</mxCell>
<mxCell id="7" style="endArrow=classic;html=1;rounded=0;" edge="1" parent="1" source="3" target="4">
  <mxGeometry relative="1" as="geometry"/>
</mxCell>
<mxCell id="8" value="Yes" style="endArrow=classic;startArrow=classic;html=1;" edge="1" parent="1" source="4" target="5">
  <mxGeometry relative="1" as="geometry"/>
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
/// a stable order (memory, current diagram, scope, feedback) and only when
/// present.
pub fn codegen_user_prompt(
    user_prompt: &str,
    current_xml: Option<&str>,
    scope: Option<&str>,
    feedback: Option<&[String]>,
) -> String {
    codegen_user_prompt_with_memory(user_prompt, current_xml, scope, feedback, &[])
}

/// [`codegen_user_prompt`] with a session-memory section (R2): summaries of
/// earlier turns, rendered as bullet context so follow-up requests remember
/// what the user already asked and what was already done.
pub fn codegen_user_prompt_with_memory(
    user_prompt: &str,
    current_xml: Option<&str>,
    scope: Option<&str>,
    feedback: Option<&[String]>,
    memory: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !memory.is_empty() {
        let bullets = memory
            .iter()
            .map(|m| format!("- {m}"))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "Earlier turns in this session (context — do NOT redo unless asked):\n{bullets}"
        ));
    }

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

/// System prompt for the single-call "look at the latest render and fix"
/// step (v2 single-context loop). Merges the reviewer role into the editor
/// role: the model sees the rendered PNG, self-reviews it, and produces the
/// fixed XML in ONE call — no separate review round-trip.
///
/// The response must be a JSON envelope (the provider sets json_object
/// response format): `{"done", "xml", "issues", "reasoning"}`. The loop
/// re-renders and feeds the result back, so `done=false` + `issues` is how
/// the model asks for a follow-up look at its own fix.
pub fn fix_system_prompt() -> &'static str {
    r#"You are a Draw.io XML editor WITH VISION. Attached is the LATEST rendered
image of the diagram, plus the current XML in the user message.

Your job in ONE response:
1. Visually self-review the rendered image: cell overlap, text overflow,
   arrows pointing at the wrong cell, edges crossing for no reason, layout
   that is confusing or unbalanced.
2. Fix every issue you find directly in the XML.

Draw.io conventions:
- Cell id="0" is the synthetic root (not rendered); visible cells declare a parent.
- Preserve existing cell ids EXACTLY. Never invent ids for existing cells.
- When a Scope section is present, ONLY cells inside the scope may be
  modified (the server merges them back). Never touch cells outside the scope.
- New cells (e.g. an added arrow) get fresh unique ids and a parent.

DIFF OUTPUT RULE (critical for cost and safety):
- Put ONLY the cells you changed or added into the "xml" field. Cells you
  leave out are left byte-for-byte untouched by the server — never echo the
  whole diagram back.
- To DELETE a cell, do NOT include it in xml; list its id in "removed".
  Descendants and edges referencing a removed cell are cleaned up
  automatically, but you may list edges explicitly too.
- A document containing just one changed cell is a perfectly valid answer.
- The document structure may be minimal but MUST include the synthetic
  root and the default layer, or the server rejects it:
  <mxfile><diagram id="d"><mxGraphModel><root><mxCell id="0"/><mxCell id="1" parent="0"/>{changed cells here}</root></mxGraphModel></diagram></mxfile>

Respond with ONLY this JSON object (no commentary, no markdown fences):
{"done": true|false, "xml": "<mxfile>...</mxfile>", "removed": ["cell_id", ...],
 "issues": [{"kind": "...", "severity": "high|medium|low",
             "cell_ids": [...], "description": "..."}],
 "reasoning": "short summary of what you changed and why"}

- Set done=true when you have completed all the changes you intend to make
  in this response. done=false is ONLY for when something remains that you
  genuinely cannot resolve without seeing the next render.
- issues kinds: overlap | text_overflow | edge_crossing | arrow_wrong | layout_bad.
  Be conservative: only flag real, visible problems."#
}

/// User prompt for the single-call fix step.
///
/// The rendered image is attached separately as a message part; this text
/// carries the instruction plus the current state: full XML when no scope is
/// given (free edit), or ONLY the scope subgraph when cells are targeted
/// (Plan-B: the model must never see or rewrite the full diagram).
pub fn fix_user_prompt(
    instruction: &str,
    current_xml: Option<&str>,
    scope: Option<&str>,
    issues: &[crate::ReviewIssue],
    checks: &[String],
    memory: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !memory.is_empty() {
        let bullets = memory
            .iter()
            .map(|m| format!("- {m}"))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!(
            "Earlier turns in this session (context — do NOT redo unless asked):\n{bullets}"
        ));
    }

    parts.push(format!(
        "The rendered image of the current diagram is attached.\nTask: {instruction}"
    ));

    if let Some(xml) = current_xml {
        parts.push(format!("Current XML (full diagram state):\n{xml}"));
    }
    if let Some(selection) = scope {
        parts.push(format!(
            "Scope — the ONLY cells you may modify (server merges these back):\n{selection}"
        ));
    }
    if !issues.is_empty() {
        let numbered = issues
            .iter()
            .enumerate()
            .map(|(i, it)| {
                format!(
                    "{}. [{}|{}|cells:{}] {}",
                    i + 1,
                    it.kind,
                    it.severity,
                    it.cell_ids.join(","),
                    it.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("Issues from the previous round (verify/fix):\n{numbered}"));
    }
    if !checks.is_empty() {
        let bullets = checks
            .iter()
            .map(|check| format!("- {check}"))
            .collect::<Vec<_>>()
            .join("\n");
        parts.push(format!("Focus checks:\n{bullets}"));
    }

    parts.join("\n\n")
}