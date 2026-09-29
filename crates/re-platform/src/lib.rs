//! Platform services (UI-independent): per-user directories, the
//! single-instance lock, and forwarding file paths to the running editor.
//!
//! This is the only crate (besides the GPUI frontend) that contains
//! OS-specific code; everything is behind `#[cfg]` here.

pub mod instance;
pub mod paths;
