//! syno-clean — a terminal UI for reviewing and cleaning up Synology
//! Download Station tasks.
//!
//! This binary is deliberately thin: everything of substance lives in the
//! `syno_clean` library crate (`src/lib.rs`). What is left here is startup —
//! logging, config resolution, the two hidden `--dump-*` modes — and the event
//! loop, which owns the terminal for as long as the TUI is running.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{self, Event};
use syno_clean::api::auth::{self, Credentials};
use syno_clean::api::client::SynoClient;
use syno_clean::api::download_station;
use syno_clean::app::App;
use syno_clean::cli::Cli;
use syno_clean::config::{self, Config, Paths, ResolvedConfig, SessionCache};
use syno_clean::ui::{self, TerminalGuard};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::discover()?;

    // Logging is initialized before anything else that might want to warn
    // (unknown config keys, a corrupt session cache), and the guard is held
    // for the whole of `main` — dropping it early discards buffered lines.
    let log_file = cli.log_file.clone().unwrap_or_else(|| paths.log_file());
    let _log_guard = config::init_logging(&log_file)?;

    // Offline fixture mode short-circuits everything below: it makes no
    // network call, so requiring a host, a username and a password to look at
    // a captured response would defeat the point of the flag.
    if let Some(fixture) = cli.fixture.clone() {
        return run_fixture(&fixture).await;
    }

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

    if cli.is_dump() {
        return dump(&cli, &resolved, &paths).await;
    }

    let mut app = App::new(Vec::new());
    app.set_status(format!(
        "{} as {} · logs: {}",
        resolved.base_url(),
        resolved.username,
        log_file.display()
    ));

    run_tui(&mut app).await
}

/// The hidden `--fixture` mode: the TUI over a captured `list` response.
///
/// Until the poller lands in Task 11 this is the only way to see the table at
/// all, and afterwards it stays the way the UI is exercised without a NAS — the
/// table, multi-select, sorting, filtering and search are all verifiable from a
/// checked-in JSON file.
async fn run_fixture(path: &Path) -> Result<()> {
    tracing::info!(fixture = %path.display(), "offline fixture mode");

    let mut app = App::from_fixture(path)
        .with_context(|| format!("could not load the fixture {}", path.display()))?;
    app.set_status(format!(
        "offline · {} · {} tasks",
        path.display(),
        app.tasks.len()
    ));

    run_tui(&mut app).await
}

/// The main event loop: draw, wait for one event, hand it to [`App`], repeat.
///
/// The panic hook is installed **before** the guard, so a panic during setup is
/// covered too, and the guard is dropped on every exit path — including the `?`
/// below — restoring the terminal.
///
/// Shape note for Task 11: the loop deliberately reduces to *draw, await one
/// event, apply it*. Turning it into the planned
/// `tokio::select! { terminal event, poller event }` means replacing the single
/// `next_terminal_event().await` with the select and adding an arm for
/// `AppEvent`; nothing else here has to move.
async fn run_tui(app: &mut App) -> Result<()> {
    ui::install_panic_hook();
    let mut terminal = TerminalGuard::new().context(
        "could not take over the terminal — syno-clean needs an interactive TTY \
         (use --dump-api-info or --dump-tasks-json when piping output)",
    )?;

    while !app.should_quit() {
        terminal.draw(app)?;
        // A page jump is a screenful of the table, so the app is told how tall
        // that is after every draw — including after a resize.
        app.set_page_size(terminal.page_size()?);
        app.handle_event(next_terminal_event().await?);
    }

    tracing::info!("exiting");
    Ok(())
}

/// Wait for the next terminal event without blocking the async runtime.
///
/// crossterm's `event-stream` feature is **not** enabled by ratatui's crossterm
/// re-export, and adding crossterm directly is forbidden (see `CLAUDE.md`), so
/// the blocking `event::read` runs on the blocking pool instead. Exactly one
/// read is ever in flight — it is awaited immediately — so nothing lingers on
/// that pool when the loop exits.
async fn next_terminal_event() -> Result<Event> {
    Ok(tokio::task::spawn_blocking(event::read).await??)
}

/// The hidden `--dump-api-info` / `--dump-tasks-json` modes.
///
/// These exist so the real shapes can be captured from an actual NAS — in
/// particular `tests/fixtures/task_list.json`, which the parser tests and the
/// offline `--fixture` mode are built on. The bodies are printed verbatim,
/// with no reformatting, so the capture is exactly what DSM sent.
async fn dump(cli: &Cli, resolved: &ResolvedConfig, paths: &Paths) -> Result<()> {
    let mut client = SynoClient::new(resolved)?;
    client.discover().await?;

    // Discovery needs no session, so this half works before any credentials
    // exist — useful precisely when a login is what is being debugged.
    if cli.dump_api_info {
        println!("{}", client.discovery_json().await?);
    }

    if cli.dump_tasks_json {
        let client = authenticate(client, resolved, paths).await?;
        println!("{}", download_station::list_tasks_json(&client).await?);
    }
    Ok(())
}

/// Attach a session to a discovered client, reusing a cached `sid` when there
/// is one and logging in when there is not.
///
/// Credentials are resolved either way and handed to the client, so a cached
/// `sid` that DSM has since expired is repaired by the transparent
/// re-login-once retry in `api::client` rather than failing the run. The cost
/// is a password prompt when `SYNO_CLEAN_PASSWORD` is unset; the cache still
/// saves the login round-trip.
async fn authenticate(
    client: SynoClient,
    resolved: &ResolvedConfig,
    paths: &Paths,
) -> Result<SynoClient> {
    let session_file = paths.session_file();
    let key = resolved.session_key();
    let mut cache = SessionCache::load(&session_file);

    let password =
        config::resolve_password(&config::system_env, &resolved.username, &resolved.host)?;
    let mut credentials = Credentials::new(&resolved.username, password);
    if let Some(otp) = config::otp_from_env(&config::system_env) {
        credentials = credentials.with_otp(otp);
    }

    let sid = match cache.sid(&key) {
        Some(sid) => {
            tracing::info!(%key, "reusing the cached session");
            sid.to_string()
        }
        None => {
            let sid = match auth::login(&client, &credentials).await {
                Ok(sid) => sid,
                // DSM only asks for a one-time code after a login attempt, so
                // the prompt belongs here rather than up front.
                Err(err) if auth::is_otp_required(&err) => {
                    credentials = credentials.with_otp(prompt_otp()?);
                    auth::login(&client, &credentials).await?
                }
                Err(err) => return Err(err.into()),
            };
            cache.set_sid(&key, &sid);
            cache.save(&session_file)?;
            sid
        }
    };

    Ok(client.with_credentials(credentials).with_sid(sid))
}

/// Ask for a 2-step verification code on the terminal.
///
/// Not secret in the way a password is, so it is read as plain input — and it
/// must happen before any alternate screen is entered.
fn prompt_otp() -> Result<String> {
    use std::io::{BufRead, Write};

    print!("DSM 2-step verification code: ");
    std::io::stdout().flush()?;
    let mut code = String::new();
    std::io::stdin().lock().read_line(&mut code)?;
    Ok(code.trim().to_string())
}
