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
        _cell_ids: &[String],
        instructions: &str,
    ) -> Result<LlmResponse<String>, String> {
        self.llm
            .generate_xml(GenerateRequest {
                user_prompt: format!("Patch: {instructions}"),
                current_xml: Some(xml.to_string()),
                scope: Some(xml.to_string()),
                feedback: None,
                json_mode: false,
            })
            .await
            .map_err(|e| e.to_string())
    }
}