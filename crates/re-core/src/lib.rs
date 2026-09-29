//! UI-independent editor core: document storage, file I/O, edits, undo.
//!
//! This crate must never depend on a UI framework.

pub mod change;
pub mod document;
pub mod editor;
pub mod history;
pub mod huge;
pub mod paste;
pub mod selection;
pub mod text_io;
