//! drawio-harness: minimal chat + tools around one canonical drawio xml file.
//!
//! See `docs/harness-refactor.md` (repo root) for the design.

#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

pub mod xmlfile;

pub use xmlfile::{CellSpan, CheckReport, EditReport, XmlDoc, XmlError};
