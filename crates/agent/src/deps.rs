//! Dependency trait the Agent Loop drives. Tests provide stub impls;
//! production wiring forwards to the real LlmProvider / RenderDriver.

use async_trait::async_trait;
use drawio_agent_llm_client::{GenerateRequest, LlmResponse, ReviewIssue, Usage};
use drawio_agent_renderer::RenderError;

/// One fix round of the v2 single-context loop.
///
/// The loop renders the current XML, then asks the LLM to self-review the
/// image and fix problems in a SINGLE multimodal call. `cell_ids` decides
/// the edit mode: non-empty targets a Plan-B subgraph (the deps impl must
/// send only the scope and merge the result back); empty means the model
/// edits the full document.
#[derive(Debug, Clone)]
pub struct FixRequest {
    /// The full, currently-applied XML.
    pub xml: String,
    /// Latest rendered PNG of `xml` — the model's visual input.
    pub image_png: Vec<u8>,
    /// User ask / loop feedback for this round.
    pub instruction: String,
    /// Scope target cells. Empty = free (full-document) edit.
    pub cell_ids: Vec<String>,
    /// Self-reported issues from the previous round (stateless bridge
    /// between rounds until conversation memory lands in R2).
    pub prior_issues: Vec<ReviewIssue>,
    /// Optional reviewer focus checks.
    pub checks: Vec<String>,
    /// Session memory (R2): summaries of earlier turns, rendered as stable
    /// background context for this round.
    pub memory: Vec<String>,
}

/// Result of one fix round, already merged/validated by the deps impl so
/// the runner never parses XML itself.
#[derive(Debug, Clone)]
pub struct FixOutcome {
    /// `true` when the model reports it is done (nothing left to fix).
    pub done: bool,
    /// Items the model could not resolve / wants visually re-verified.
    pub issues: Vec<ReviewIssue>,
    pub reasoning: Option<String>,
    /// The new full current XML (post-merge when scoped).
    pub xml: String,
    /// `false` when the round produced no change at all (no-op).
    pub changed: bool,
    pub usage: Usage,
    pub duration_ms: u64,
    pub finish_reason: Option<String>,
}

/// Errors from a fix round.
#[derive(Debug)]
pub enum FixError {
    /// Provider/API failure — fatal for the loop.
    Llm(String),
    /// The LLM answered but its output was unusable (bad envelope, invalid
    /// XML, dropped target cells…). The runner records it, feeds the reason
    /// back to the model next round, and retries — it is NOT fatal.
    Rejected(String),
}

impl std::fmt::Display for FixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Llm(msg) => write!(f, "llm: {msg}"),
            Self::Rejected(msg) => write!(f, "rejected: {msg}"),
        }
    }
}

/// One step in the Agent Loop. Each method corresponds to a single
/// capability (LLM call, render, multimodal fix).
#[async_trait]
pub trait AgentDeps: Send + Sync {
    /// Run codegen (baseline) and return the resulting XML.
    async fn generate(
        &self,
        req: GenerateRequest,
    ) -> Result<LlmResponse<String>, String>;

    /// Render XML to PNG bytes.
    async fn render(&self, xml: &str) -> Result<Vec<u8>, RenderError>;

    /// One multimodal "look at the render and fix" round. Must return the
    /// complete new XML (merging the Plan-B scope back when `cell_ids` is
    /// non-empty) plus whether anything actually changed.
    async fn fix(&self, req: &FixRequest) -> Result<FixOutcome, FixError>;
}
