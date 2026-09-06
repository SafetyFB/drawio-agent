//! drawio-harness: minimal chat + tools around one canonical drawio xml file.
//!
//! See `docs/harness-refactor.md` (repo root) for the design.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod chat;
pub mod engine;
pub mod refs;
pub mod tools;
pub mod web;
pub mod xmlfile;

pub use chat::{Chat, ChatError, Message, OpenAiChat};
pub use engine::{parse_envelope, Harness, TurnOutcome};
pub use refs::{resolve_refs, ResolvedRef};
pub use tools::Tools;
pub use xmlfile::{CellSpan, CheckReport, EditReport, XmlDoc, XmlError};
