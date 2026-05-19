//! Editor subsystem state modules.
//!
//! Each module groups related fields from the monolithic `Editor` struct
//! into a focused struct, reducing cognitive load and making it easier
//! to reason about individual subsystems.

pub mod build;
pub mod git;
pub mod llm;
pub mod lsp;
pub mod popup;
pub mod search;
