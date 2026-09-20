//! Binary entry point. Everything it used to hold is in the library (see
//! `src/lib.rs`); this stays a separate file only because the Windows
//! subsystem attribute and `fn main` have to live on the binary crate root.

// A GUI build must not pop a console window. Debug builds keep one so panics
// and `println!` are visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    the_real_explorer::app::run()
}
