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
pub mod history;
pub mod refs;
pub mod tools;
pub mod web;
pub mod xmlfile;

pub use chat::{CallOpts, Chat, ChatError, Message, OpenAiChat, Reply, Usage};
pub use config::{LlmSettings, ThinkingMode, mask_secret, preset_prices, usage_cost};
pub use engine::{parse_envelope, EngineEvent, Harness, ProgressFn, RunOpts, SessionStats, TurnOutcome};
pub use history::{HistoryRec, SessionBundle};
pub use refs::{resolve_refs, ResolvedRef};
pub use tools::Tools;
pub use xmlfile::{CellSpan, CheckReport, EditReport, XmlDoc, XmlError};
