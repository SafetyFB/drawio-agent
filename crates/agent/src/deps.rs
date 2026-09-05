//! Dependency trait the Agent Loop drives. Tests provide stub impls;
//! production wiring would forward to the real LlmProvider / RenderDriver.

use async_trait::async_trait;
use drawio_agent_llm_client::{GenerateRequest, LlmResponse, ReviewResponse};
use drawio_agent_renderer::RenderError;

/// One step in the Agent Loop. Each method corresponds to a single
/// capability (LLM call, render, VLM review, scoped patch).
#[async_trait]
pub trait AgentDeps: Send + Sync {
    /// Run codegen and return the resulting XML.
    async fn generate(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, String>;

    /// Render XML to PNG bytes.
    async fn render(&self, xml: &str) -> Result<Vec<u8>, RenderError>;

    /// Run visual review on the current XML + PNG. Returns the parsed
    /// `ReviewResponse` (verdict + issues).
    async fn review(
        &self,
        xml: &str,
        png: &[u8],
    ) -> Result<ReviewResponse, String>;

    /// Re-run codegen scoped to the given cells (typically after a review
    /// surfaced issues). Returns the new XML.
    async fn patch(
        &self,
        xml: &str,
        cell_ids: &[String],
        instructions: &str,
    ) -> Result<LlmResponse<String>, String>;
}
