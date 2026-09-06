//! drawio-harness: minimal chat + tools around one canonical drawio xml file.
//!
//! See `docs/harness-refactor.md` (repo root) for the design.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod chat;
pub mod config;
pub mod engine;
pub mod refs;
pub mod tools;
pub mod web;
pub mod xmlfile;

pub use chat::{CallOpts, Chat, ChatError, Message, OpenAiChat, Reply, Usage};
pub use config::{LlmSettings, ThinkingMode, mask_secret, preset_prices, usage_cost};
pub use engine::{parse_envelope, Harness, RunOpts, SessionStats, TurnOutcome};
pub use refs::{resolve_refs, ResolvedRef};
pub use tools::Tools;
pub use xmlfile::{CellSpan, CheckReport, EditReport, XmlDoc, XmlError};
