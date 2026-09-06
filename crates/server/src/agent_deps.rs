//! Adapter wiring the server's shared `LlmProvider` + `RenderDriver` into
//! the Agent Loop's `AgentDeps` trait (v2 single-context loop).

use std::sync::Arc;

use async_trait::async_trait;
use drawio_agent_agent::{AgentDeps, FixError, FixOutcome, FixRequest};
use drawio_agent_llm_client::{
    parse_fix_envelope, FixRequest as ProviderFixRequest, GenerateRequest, LlmProvider,
    LlmResponse, ReviewIssue,
};
use drawio_agent_renderer::{RenderDriver, RenderError, RenderOptions};

/// Bridges the Agent Loop to the server's shared LLM + renderer.
pub struct ServerAgentDeps {
    pub llm: Arc<dyn LlmProvider>,
    pub renderer: Arc<dyn RenderDriver>,
}

impl std::fmt::Debug for ServerAgentDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerAgentDeps").finish_non_exhaustive()
    }
}

#[async_trait]
impl AgentDeps for ServerAgentDeps {
    async fn generate(&self, req: GenerateRequest) -> Result<LlmResponse<String>, String> {
        self.llm.generate_xml(req).await.map_err(|e| e.to_string())
    }

    async fn render(&self, xml: &str) -> Result<Vec<u8>, RenderError> {
        self.renderer.render(xml, &RenderOptions::default()).await
    }

    async fn fix(&self, req: &FixRequest) -> Result<FixOutcome, FixError> {
        // Plan-B scoping: when cells are targeted, the model sees ONLY the
        // subgraph (never the full diagram) and its scope-only result is
        // merged back cell-by-cell; untouched cells stay byte-identical.
        let full_parse = drawio_agent_xml_core::MxFile::parse(req.xml.as_bytes());
        let mut full_file = full_parse.ok();
        // Unparseable current state (shouldn't happen — the state was validated
        // on the way in) — falls back to a plain full-document fix and lets
        // the loop's own validation catch problems.

        let cell_id_refs: Vec<&str> = req.cell_ids.iter().map(|s| s.as_str()).collect();
        let (scope_xml, subgraph) = if cell_id_refs.is_empty() {
            (None, None)
        } else {
            let file = full_file
                .as_mut()
                .ok_or_else(|| FixError::Llm("current XML unparseable; cannot scope fix".into()))?;
            let model = file
                .diagrams
                .first_mut()
                .and_then(|d| d.model.as_mut())
                .ok_or_else(|| FixError::Llm("no diagram/model in current XML".into()))?;
            let sub = model.extract_subgraph(&cell_id_refs);
            // A targeted fix with unknown cells would silently no-op — be
            // loud instead so the runner feeds it back.
            let missing: Vec<&str> = sub.missing.iter().map(|s| s.as_str()).collect();
            if !missing.is_empty() {
                return Err(FixError::Rejected(format!(
                    "scope cells not found in current diagram: {}",
                    missing.join(", ")
                )));
            }
            let scope = drawio_agent_xml_core::serialize_subgraph(&sub)
                .map_err(|e| FixError::Llm(format!("subgraph serialize: {e}")))?;
            (Some(scope), Some(sub))
        };

        let prior_issues: Vec<ReviewIssue> = req.prior_issues.clone();
        let resp = self
            .llm
            .fix_diagram(ProviderFixRequest {
                instruction: req.instruction.clone(),
                current_xml: scope_xml.is_none().then(|| req.xml.clone()),
                scope_xml,
                issues: prior_issues,
                checks: req.checks.clone(),
                image_png: req.image_png.clone(),
                memory: req.memory.clone(),
            })
            .await
            .map_err(|e| FixError::Llm(e.to_string()))?;

        // The assistant content is the JSON envelope; the model is told to
        // always put the full resulting state in `xml`. Parse strictly and
        // classify anything unusable as Rejected so the loop can retry with
        // the reason instead of dying.
        let envelope = parse_fix_envelope(&resp.content).map_err(FixError::Rejected)?;

        let new_full_xml: String = if subgraph.is_some() {
            // Scope mode: the returned document should contain (at least)
            // the targeted cells; merge them onto the untouched original.
            let patched = drawio_agent_xml_core::MxFile::parse(envelope.xml.as_bytes())
                .map_err(|e| FixError::Rejected(format!("fix output is not valid mxfile: {e}")))?;
            let patched_model = patched
                .diagrams
                .first()
                .and_then(|d| d.model.as_ref())
                .ok_or_else(|| {
                    FixError::Rejected("fix output has no diagram/model".into())
                })?;
            let patched_subgraph = patched_model.extract_subgraph(&cell_id_refs);
            if !patched_subgraph.missing.is_empty() {
                return Err(FixError::Rejected(format!(
                    "fix output dropped targeted cells: {}",
                    patched_subgraph.missing.join(", ")
                )));
            }
            let file = full_file
                .as_mut()
                .ok_or_else(|| FixError::Llm("original file lost".into()))?;
            let model = file
                .diagrams
                .first_mut()
                .and_then(|d| d.model.as_mut())
                .ok_or_else(|| FixError::Llm("no model in original".into()))?;
            model.apply_subgraph(&patched_subgraph);
            file.to_xml()
                .map_err(|e| FixError::Llm(format!("serialize merged XML: {e}")))?
        } else {
            // Full-document mode: the model returns the whole diagram. It
            // must at least parse; round-trip through the canonical
            // serializer so the no-op comparison below is formatting-blind.
            let parsed = drawio_agent_xml_core::MxFile::parse(envelope.xml.as_bytes())
                .map_err(|e| FixError::Rejected(format!("fix output is not valid mxfile: {e}")))?;
            parsed
                .to_xml()
                .map_err(|e| FixError::Llm(format!("serialize fix output: {e}")))?
        };

        // No-op detection on canonical forms: both sides pass through
        // parse → to_xml so pure whitespace/formatting drift does not count
        // as a change (and does not burn a version).
        let canonical_prev = canonicalize(&req.xml);
        let changed = canonical_prev.as_deref() != Some(new_full_xml.as_str());

        Ok(FixOutcome {
            done: envelope.done,
            issues: envelope.issues,
            reasoning: envelope.reasoning,
            xml: new_full_xml,
            changed,
            usage: resp.usage,
            duration_ms: resp.duration_ms,
            finish_reason: resp.finish_reason,
        })
    }
}

/// Parse-and-reserialize an XML document to its canonical form, or `None`
/// when it doesn't parse (raw comparison then falls back to byte equality).
fn canonicalize(xml: &str) -> Option<String> {
    let file = drawio_agent_xml_core::MxFile::parse(xml.as_bytes()).ok()?;
    file.to_xml().ok()
}
