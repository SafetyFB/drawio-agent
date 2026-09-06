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

/// Process-wide flag: the configured provider rejected an image input at
/// least once (non-vision model, gateway policy, or an unparseable image).
/// Once set, all fix rounds in this process run text-only.
static VISION_REJECTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reset the process-wide vision-rejection flag (tests switch providers
/// between cases; a real server only changes models across restarts).
#[doc(hidden)]
pub fn reset_vision_rejection_flag() {
    VISION_REJECTED.store(false, std::sync::atomic::Ordering::Relaxed);
}

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
        // Process-level memory of a vision rejection: once the configured
        // provider refuses an image, every later fix round in this process
        // goes straight to text-only mode (no more 400 round-trips per
        // iteration). Reset by restarting the server after a model change.
        let text_only = VISION_REJECTED.load(std::sync::atomic::Ordering::Relaxed)
            || req.image_png.is_empty();
        let resp = self
            .llm
            .fix_diagram(ProviderFixRequest {
                instruction: req.instruction.clone(),
                current_xml: scope_xml.is_none().then(|| req.xml.clone()),
                scope_xml: scope_xml.clone(),
                issues: prior_issues,
                checks: req.checks.clone(),
                image_png: if text_only { Vec::new() } else { req.image_png.clone() },
                memory: req.memory.clone(),
            })
            .await;
        // Vision rejection fallback: when the configured model/provider
        // refuses image input (e.g. a non-vision model or an image the
        // gateway cannot parse), retry ONCE without the image so the user
        // can still get a text-based fix instead of a hard failure — and
        // remember the rejection so subsequent rounds skip the image too.
        let resp = match resp {
            Ok(r) => r,
            Err(e) if !text_only && is_vision_rejection(&e) => {
                VISION_REJECTED.store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    error = %e,
                    "vision input rejected by provider; retrying fix without image                      (later rounds will go text-only)"
                );
                self.llm
                    .fix_diagram(ProviderFixRequest {
                        instruction: req.instruction.clone(),
                        current_xml: scope_xml.is_none().then(|| req.xml.clone()),
                        scope_xml,
                        issues: req.prior_issues.clone(),
                        checks: req.checks.clone(),
                        image_png: Vec::new(), // text-only fallback
                        memory: req.memory.clone(),
                    })
                    .await
                    .map_err(|e2| {
                        FixError::Llm(format!(
                            "{e2} (image input was rejected and the text-only retry also failed; \
                             check that the configured model supports images, or switch to \
                             Generate/从头画 which never sends images)"
                        ))
                    })?
            }
            Err(e) => return Err(FixError::Llm(e.to_string())),
        };

        // The assistant content is the JSON envelope; the model is told to
        // always put the full resulting state in `xml`. Parse strictly and
        // classify anything unusable as Rejected so the loop can retry with
        // the reason instead of dying.
        let envelope = parse_fix_envelope(&resp.content).map_err(FixError::Rejected)?;
        let removed_refs: Vec<&str> = envelope.removed.iter().map(|s| s.as_str()).collect();

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
            if !removed_refs.is_empty() {
                model.remove_cells(&removed_refs);
            }
            file.to_xml()
                .map_err(|e| FixError::Llm(format!("serialize merged XML: {e}")))?
        } else {
            // Diff mode (no explicit scope): the model saw the full diagram
            // but must output ONLY the cells it changed (plus `removed` for
            // deletions). Every cell absent from the response stays
            // byte-for-byte untouched — that is what makes output tokens
            // scale with the change, not the diagram. A model that ignores
            // the rule and echoes everything back still merges correctly.
            let patched = drawio_agent_xml_core::MxFile::parse(envelope.xml.as_bytes())
                .map_err(|e| FixError::Rejected(format!("fix output is not valid mxfile: {e}")))?;
            let incoming = collect_visible_cells(&patched);
            let file = full_file
                .as_mut()
                .ok_or_else(|| FixError::Llm("original file lost".into()))?;
            let model = file
                .diagrams
                .first_mut()
                .and_then(|d| d.model.as_mut())
                .ok_or_else(|| FixError::Llm("no model in original".into()))?;
            model.apply_cell_diff(&incoming, &removed_refs);
            file.to_xml()
                .map_err(|e| FixError::Llm(format!("serialize merged XML: {e}")))?
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

/// Collect every visible cell (all except synthetic root "0" and default
/// layer "1") from a parsed fix-diff response document, in tree order.
/// These become the diff's `incoming` set.
fn collect_visible_cells(file: &drawio_agent_xml_core::MxFile) -> Vec<drawio_agent_xml_core::Cell> {
    fn walk(
        cell: &drawio_agent_xml_core::Cell,
        out: &mut Vec<drawio_agent_xml_core::Cell>,
    ) {
        if cell.id != "0" && cell.id != "1" {
            out.push(cell.clone());
        }
        for child in &cell.children {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    if let Some(model) = file.diagrams.first().and_then(|d| d.model.as_ref()) {
        walk(&model.root, &mut out);
    }
    out
}

/// Best-effort detection that a provider error means "image input not
/// accepted" (non-vision model / gateway policy), so the caller can retry
/// without the image. Matches on common gateway wording; anything else is
/// treated as a real transport/provider failure.
fn is_vision_rejection(err: &drawio_agent_llm_client::ProviderError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("image") && (msg.contains("400") || msg.contains("invalid") || msg.contains("parse") || msg.contains("format"))
        || msg.contains("图片") || msg.contains("1210") || msg.contains("vision")
}
