//! syno-clean — a terminal UI over the Synology DSM HTTP API for reviewing
//! Download Station tasks and reclaiming the space they left behind.
//!
//! The crate is split into a thin binary (`src/main.rs`) and this library so
//! that every module is reachable from unit tests and from `tests/`, and so a
//! module that has landed but is not yet wired into the event loop does not
//! trip `dead_code` while the rest of the program is still being built.

pub mod api;
pub mod cli;
pub mod config;
pub mod error;
