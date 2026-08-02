//! syno-clean — a terminal UI for reviewing and cleaning up Synology
//! Download Station tasks.
//!
//! This binary is deliberately thin: everything of substance lives in the
//! `syno_clean` library crate (`src/lib.rs`). Runtime setup, the terminal
//! guard and the event loop land in later tasks.

use syno_clean::error::Result;

fn main() -> Result<()> {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    Ok(())
}
