#![cfg(feature = "gui")]

//! Legacy module intentionally not compiled by the application.
//!
//! GUI output is the CLI child's stdout/stderr. Tool previews, diffs, and
//! terminal rendering stay in the CLI renderer (`src/pretty*.rs`), so the GUI
//! process shell does not maintain a second renderer.
