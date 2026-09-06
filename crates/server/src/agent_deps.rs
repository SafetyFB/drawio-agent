//! Adapter wiring the server's shared `LlmProvider` + `RenderDriver` into
//! the Agent Loop's `AgentDeps` trait.

use std::sync::Arc;

use async_trait::async_trait;
use drawio_agent_agent::AgentDeps;
use drawio_agent_llm_client::{
    GenerateRequest, LlmProvider, LlmResponse, ReviewRequest, ReviewResponse,
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

    async fn review(&self, xml: &str, png: &[u8]) -> Result<ReviewResponse, String> {
        self.llm
            .review_visual(ReviewRequest {
                image_png: png.to_vec(),
                xml: xml.to_string(),
                checks: vec![],
            })
            .await
            .map(|r| r.content)
            .map_err(|e| e.to_string())
    }

    async fn patch(
        &self,
        xml: &str,
        cell_ids: &[String],
        instructions: &str,
    ) -> Result<LlmResponse<String>, String> {
        // Plan B: extract the subgraph for the targeted cells and send ONLY
        // that as the LLM's scope (never the full diagram), then apply the
        // returned subgraph back onto the full model. Falls back to the old
        // full-context call when the XML can't be parsed or no cells are
        // targeted (nothing to scope by).
        let mut file = match drawio_agent_xml_core::MxFile::parse(xml.as_bytes()) {
            Ok(f) => f,
            Err(_e) => {
                return self
                    .llm
                    .generate_xml(GenerateRequest {
                        user_prompt: format!("Patch: {instructions}"),
                        current_xml: Some(xml.to_string()),
                        scope: Some(xml.to_string()),
                        feedback: None,
                        json_mode: false,
                    })
                    .await
                    .map_err(|e| e.to_string());
            }
        };
        let cell_id_refs: Vec<&str> = cell_ids.iter().map(|s| s.as_str()).collect();
        let (scope_xml, subgraph) = if cell_id_refs.is_empty() {
            (None, None)
        } else {
            let model = file
                .diagrams
                .first_mut()
                .ok_or_else(|| "no diagram in current XML".to_string())?
                .model
                .as_mut()
                .ok_or_else(|| "no model in current XML".to_string())?;
            let sub = model.extract_subgraph(&cell_id_refs);
            let scope = drawio_agent_xml_core::serialize_subgraph(&sub)
                .map_err(|e| format!("subgraph serialize: {e}"))?;
            (Some(scope), Some(sub))
        };

        let req = match &scope_xml {
            Some(scope) => GenerateRequest {
                user_prompt: format!("Patch: {instructions}"),
                current_xml: None,
                scope: Some(scope.clone()),
                feedback: None,
                json_mode: false,
            },
            None => GenerateRequest {
                user_prompt: format!("Patch: {instructions}"),
                current_xml: Some(xml.to_string()),
                scope: Some(xml.to_string()),
                feedback: None,
                json_mode: false,
            },
        };

        let mut resp = self
            .llm
            .generate_xml(req)
            .await
            .map_err(|e| e.to_string())?;

        // Plan B: stitch the LLM's patched subgraph back into the full model
        // so the returned XML is the complete updated diagram (the runner
        // stores it as the session's new current XML).
        if subgraph.is_some() {
            let patched = drawio_agent_xml_core::MxFile::parse(resp.content.as_bytes())
                .map_err(|e| format!("parse LLM patch response: {e}"))?;
            let patched_subgraph = patched
                .diagrams
                .first()
                .ok_or_else(|| "no diagram in LLM response".to_string())?
                .model
                .as_ref()
                .ok_or_else(|| "no model in LLM response".to_string())?
                .extract_subgraph(&cell_id_refs);
            let model = file
                .diagrams
                .first_mut()
                .ok_or_else(|| "no diagram".to_string())?
                .model
                .as_mut()
                .ok_or_else(|| "no model".to_string())?;
            model.apply_subgraph(&patched_subgraph);
            resp.content = file
                .to_xml()
                .map_err(|e| format!("serialize updated XML: {e}"))?;
        }
        Ok(resp)
    }
}
