//! syno-clean — a terminal UI for reviewing and cleaning up Synology
//! Download Station tasks.
//!
//! This binary is deliberately thin: everything of substance lives in the
//! `syno_clean` library crate (`src/lib.rs`). The terminal guard and the event
//! loop land in later tasks; for now `main` performs startup — logging, config
//! resolution — and reports what it resolved.

use anyhow::Result;
use clap::Parser;
use syno_clean::cli::Cli;
use syno_clean::config::{self, Config, Paths};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;

    // Logging is initialized before anything else that might want to warn
    // (unknown config keys, a corrupt session cache), and the guard is held
    // for the whole of `main` — dropping it early discards buffered lines.
    let log_file = cli.log_file.clone().unwrap_or_else(|| paths.log_file());
    let _log_guard = config::init_logging(&log_file)?;

    let config_path = cli.config.clone().unwrap_or_else(|| paths.config_file());
    let file_config = Config::load(&config_path)?;
    let env_config = Config::from_env(&config::system_env)?;
    let resolved = config::merge(file_config, env_config, &cli)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        target = %resolved.base_url(),
        user = %resolved.username,
        refresh_secs = resolved.refresh_secs,
        delete_files = resolved.delete_files,
        dry_run = resolved.dry_run,
        "starting"
    );

    println!(
        "{} {} — {} as {} (logs: {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        resolved.base_url(),
        resolved.username,
        log_file.display()
    );
    Ok(())
}
