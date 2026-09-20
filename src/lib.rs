//! The Real Explorer as a library.
//!
//! The crate was binary-only until the editor grew a virtualised large-file
//! view that had to be measured headlessly: nothing outside `src/editor.rs`
//! could reach [`editor::Editor`], so there was nowhere to put an allocation
//! or time-to-first-paint test. Everything now lives here and `src/main.rs`
//! is a two-line binary over [`app::run`], which keeps `cargo run`, the
//! packaged exe and `cargo test --test ...` all looking at the same code.
//!
//! Modules are public because the tests and `examples/` link them from
//! outside; nothing here is a stability promise to anyone else.

pub mod app;
pub mod attach;
pub mod chat;
pub mod config;
pub mod editor;
pub mod harness;
pub mod harness_ui;
pub mod highlight;
pub mod hooks;
pub mod hooks_ui;
pub mod icons;
pub mod lsp;
pub mod meta;
pub mod platform;
pub mod textpos;
pub mod theme;
pub mod titlebar;
pub mod trash_ops;
