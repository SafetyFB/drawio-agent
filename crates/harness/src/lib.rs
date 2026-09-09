//! drawio-harness: minimal chat + tools around one canonical drawio xml file.
//!
//! See `docs/harness-refactor.md` (repo root) for the design.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

/// Minimal empty drawio document used when creating a new session/file.
pub const EMPTY_TEMPLATE: &str = r#"<mxfile host="app.diagrams.net" agent="drawio-harness"><diagram id="page-1" name="Page-1"><mxGraphModel dx="800" dy="600" grid="1" gridSize="10" guides="1" tooltips="1" connect="1" arrows="1" fold="1" page="1" pageScale="1" pageWidth="1169" pageHeight="826"><root><mxCell id="0"/><mxCell id="1" parent="0"/></root></mxGraphModel></diagram></mxfile>"#;

pub mod chat;
pub mod config;
pub mod engine;
pub mod envelope;
pub mod guards;
pub mod history;
pub mod memory;
pub mod prompt;
pub mod refs;
pub mod tools;
pub mod turn_loop;
pub mod web;
pub mod xmlfile;

pub use chat::{CallOpts, Chat, ChatError, Message, OpenAiChat, Reply, Usage};
pub use config::{LlmSettings, ThinkingMode, mask_secret, usage_cost};

pub mod metrics;
pub use engine::{EngineEvent, Harness, ProgressFn, RunOpts, SessionStats, TurnOutcome};
pub use envelope::parse_envelope;
pub use guards::{check_budget, check_context_limit, price_opts};
pub use memory::{trim_memory, fold_old_images, fold_old_reads, estimate_tokens, DEFAULT_MEMORY_TOKENS};
pub use prompt::build_system_prompt;
pub use turn_loop::HarnessRunExt;
pub use history::{HistoryRec, SessionBundle};
pub use refs::{resolve_refs, ResolvedRef};
pub use tools::Tools;
pub use xmlfile::{CellSpan, CheckReport, EditReport, XmlDoc, XmlError};
