//! syno-clean — a terminal UI for reviewing and cleaning up Synology
//! Download Station tasks.
//!
//! This binary is deliberately thin: everything of substance lives in the
//! `syno_clean` library crate (`src/lib.rs`). What is left here is startup —
//! logging, config resolution, the two hidden `--dump-*` modes — and the event
//! loop, which owns the terminal for as long as the TUI is running.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{self as terminal_event, Event};
use syno_clean::api::auth::{self, Credentials};
use syno_clean::api::client::SynoClient;
use syno_clean::api::download_station;
use syno_clean::app::App;
use syno_clean::cli::Cli;
use syno_clean::config::{self, Config, Paths, ResolvedConfig, SessionCache};
use syno_clean::delete::DeleteOptions;
use syno_clean::error::{self, Error};
use syno_clean::event::{self, AppEvent, OpContext, Receiver, RefreshHandle};
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
        let result = run_fixture(&fixture).await;
        finish_tui(result, _log_guard);
    }

    let config_path = cli.config.clone().unwrap_or_else(|| paths.config_file());
    let file_config = Config::load(&config_path)?;
    let env_config = Config::from_env(&config::system_env)?;

    // A missing config *file* is not an error; unresolved *values* are. Only
    // when the whole CLI > env > file merge still leaves `host` or `username`
    // unset does this become a first run: write a template, explain, and stop
    // short of the TUI. `--host nas --user me` on a clean machine never gets
    // here.
    let missing = config::missing_required(&file_config, &env_config, &cli);
    if missing.any() {
        return Err(first_run(&config_path, missing));
    }
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

    if resolved.logout {
        return logout(&resolved, &paths).await;
    }

    if cli.is_dump() {
        return dump(&cli, &resolved, &paths).await;
    }

    let mut app = App::new(Vec::new()).with_delete_options(DeleteOptions::from_config(&resolved));
    app.set_status(format!(
        "{} as {} · logs: {}",
        resolved.base_url(),
        resolved.username,
        log_file.display()
    ));

    // Discovery and login happen *before* the alternate screen: both can prompt
    // (password, 2FA code) and both can fail with a message worth reading, and
    // neither is any use inside a terminal the TUI has already taken over.
    //
    // A failure here **exits non-zero with a diagnostic** rather than starting
    // the TUI: an empty table is what a NAS with no downloads looks like, so
    // entering it after a failed login would leave the user unable to tell a
    // wrong password from an idle Download Station.
    let mut client = SynoClient::new(&resolved)?;
    client
        .discover()
        .await
        .map_err(|err| startup_failure(&err, &resolved))?;
    let client = Arc::new(
        authenticate(client, &resolved, &paths)
            .await
            .map_err(|err| startup_failure(&err, &resolved))?,
    );

    let (tx, rx) = event::channel();
    let refresh = RefreshHandle::new();
    // What a confirmed delete runs against. The poller takes the other half of
    // the same client and channel — one session, one event stream.
    let ops = OpContext::new(Arc::clone(&client), tx.clone(), refresh.clone());
    let poller = event::spawn_poller(
        client,
        Duration::from_secs(resolved.refresh_secs),
        tx,
        refresh.clone(),
    );

    let result = run_tui(&mut app, rx, &refresh, Some(&ops), Some(&poller)).await;
    // The loop's own shutdown already aborted it — it *has* to, before waiting
    // on any in-flight op batch (see `shutdown`). This second abort covers the
    // paths that leave `run_tui` through a `?` instead: the poller owns no
    // terminal state, but leaving a task mid-request would make the runtime
    // wait out an in-flight HTTP timeout before the process could exit.
    poller.abort();

    // A session renewed by the transparent re-login is a different sid from the
    // one on disk; without this the cached entry stays dead forever and every
    // later run burns a round trip on a 119 before repairing itself again.
    persist_session(&ops.client, &resolved, &paths);

    finish_tui(result, _log_guard)
}

/// Write back the sid the client ended the run with, if it is not the one the
/// cache already holds.
///
/// Best-effort by design: a cache is an optimization, and failing to update it
/// must never turn a successful run into a failed one.
fn persist_session(client: &SynoClient, resolved: &ResolvedConfig, paths: &Paths) {
    let Some(sid) = client.sid() else {
        return;
    };
    let session_file = paths.session_file();
    let key = resolved.session_key();
    let mut cache = SessionCache::load(&session_file);
    if cache.sid(&key) == Some(sid.as_str()) {
        return;
    }

    cache.set_sid(&key, sid);
    match cache.save(&session_file) {
        Ok(()) => tracing::info!(%key, "cached the renewed session"),
        Err(err) => tracing::warn!(%key, %err, "could not cache the renewed session"),
    }
}

/// End the process after the TUI has returned.
///
/// On the error path this **exits rather than returning**. The outstanding
/// `spawn_blocking(event::read)` is parked on stdin and cannot be cancelled;
/// dropping the runtime waits for started blocking tasks, so returning `Err`
/// would leave the process hanging until the user happened to press a key. The
/// log guard is dropped explicitly first, since `exit` runs no destructors.
fn finish_tui(result: Result<()>, log_guard: tracing_appender::non_blocking::WorkerGuard) -> ! {
    match result {
        Ok(()) => {
            drop(log_guard);
            std::process::exit(0)
        }
        Err(err) => {
            tracing::error!(%err, "exiting after a TUI failure");
            drop(log_guard);
            eprintln!("Error: {err:?}");
            std::process::exit(1)
        }
    }
}

/// Nothing to connect to: write a starter config, say what is missing, stop.
///
/// This is the *first-run* path, not the "no config file" path — the difference
/// matters. A missing file with `--host` and `--user` on the command line is a
/// perfectly good invocation and never reaches here; only values still
/// unresolved after the whole merge do. The template is written for the user to
/// edit, and an existing file is never overwritten, so a config that merely
/// lacks a username keeps everything else it had.
fn first_run(config_path: &Path, missing: config::MissingRequired) -> anyhow::Error {
    tracing::warn!(
        config = %config_path.display(),
        host = missing.host,
        username = missing.username,
        "required configuration is unresolved"
    );

    let mut message = missing.help_lines().join("\n");
    match config::write_config_template(config_path) {
        Ok(true) => message.push_str(&format!(
            "\n\nwrote a starter config to {} — uncomment and fill in `host` and `username` \
             there, or pass --host and --user",
            config_path.display()
        )),
        Ok(false) => message.push_str(&format!(
            "\n\nthe config file {} exists but does not set them",
            config_path.display()
        )),
        // The template is a convenience; failing to write it must not replace
        // the message that says what is actually wrong.
        Err(err) => message.push_str(&format!(
            "\n\ncould not write a starter config to {}: {err}",
            config_path.display()
        )),
    }
    anyhow::anyhow!(message)
}

/// A startup failure the user has to read before anything else.
///
/// The diagnostic names the host and port that were tried, what DSM said in
/// words rather than as a bare code, and one thing to try — see
/// [`syno_clean::error::connection_diagnostic`].
fn startup_failure(err: &Error, resolved: &ResolvedConfig) -> anyhow::Error {
    tracing::error!(target_url = %resolved.base_url(), %err, "startup failed");
    anyhow::anyhow!(error::connection_diagnostic(
        err,
        &resolved.base_url(),
        &resolved.username
    ))
}

/// The hidden `--fixture` mode: the TUI over a captured `list` response.
///
/// The way the UI is exercised without a NAS — the table, multi-select,
/// sorting, filtering and search are all verifiable from a checked-in JSON
/// file. No poller runs, so the event channel here only ever stays quiet: the
/// sender is held for the lifetime of the loop so `recv` pends rather than
/// reporting a closed channel on every pass.
async fn run_fixture(path: &Path) -> Result<()> {
    tracing::info!(fixture = %path.display(), "offline fixture mode");

    // Offline mode is a **dry run** by construction: there is no client here,
    // so no delete could reach a NAS even if one were confirmed. Saying so in
    // the confirmation dialog is honest; letting it promise a real recursive
    // delete it cannot perform is not.
    let mut app = App::from_fixture(path)
        .with_context(|| format!("could not load the fixture {}", path.display()))?
        .with_delete_options(DeleteOptions::dry_run());
    app.set_status(format!(
        "offline · {} · {} tasks",
        path.display(),
        app.tasks.len()
    ));

    let (_tx, rx) = event::channel();
    run_tui(&mut app, rx, &RefreshHandle::new(), None, None).await
}

/// The main event loop: draw, wait for whichever comes first — a key press or
/// something from the background — hand it to [`App`], repeat.
///
/// The panic hook is installed **before** the guard, so a panic during setup is
/// covered too, and the guard is dropped on every exit path — including the `?`
/// below — restoring the terminal.
///
/// **The pending terminal read is held across iterations.** `event::read` runs
/// on the blocking pool and cannot be cancelled: if the [`select!`] dropped its
/// future every time an [`AppEvent`] won the race, each poller tick would leave
/// another orphaned thread blocked on stdin, and they would then take turns
/// swallowing the user's keystrokes. Keeping the [`JoinHandle`] in
/// `pending_read` means exactly one read exists at any moment, and — since the
/// only thing that sets `quit` is a key press, which consumes it — none is left
/// over to stall the runtime at shutdown.
///
/// `ops` is `None` in offline `--fixture` mode, where there is no client to run
/// anything against, and so is `poller` — nothing polls there.
///
/// [`select!`]: tokio::select
/// [`JoinHandle`]: tokio::task::JoinHandle
async fn run_tui(
    app: &mut App,
    mut rx: Receiver,
    refresh: &RefreshHandle,
    ops: Option<&OpContext>,
    poller: Option<&tokio::task::JoinHandle<()>>,
) -> Result<()> {
    ui::install_panic_hook();
    let mut terminal = TerminalGuard::new().context(
        "could not take over the terminal — syno-clean needs an interactive TTY \
         (use --dump-api-info or --dump-tasks-json when piping output)",
    )?;

    // Handles for op batches still running. Kept so that (a) a second batch
    // cannot be started on top of a live one and (b) quitting does not abandon
    // one half-way through the three-phase delete.
    let mut in_flight: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // **The loop's result is captured, never returned from here.** A terminal
    // write that fails — an SSH connection dropped, the window closed, a resize
    // race — used to leave through a `?` that skipped the wait below entirely,
    // tearing the runtime down on top of whatever the delete batch was in the
    // middle of. That is the one outcome the wait exists to prevent, and a
    // broken terminal is a *more* likely way to reach it than a clean `q`.
    let loop_result = event_loop(app, &mut rx, refresh, ops, &mut terminal, &mut in_flight).await;

    // Restore the terminal *before* waiting: a batch that is mid-way between
    // "the files are gone" and "the task is gone" must be allowed to finish —
    // abandoning it there is the one outcome the whole three-phase ordering
    // exists to prevent — but the user should be looking at their shell while
    // it does, not at a frozen TUI.
    drop(terminal);
    let finished = shutdown(rx, poller, in_flight, IN_FLIGHT_GRACE).await;

    // The loop's own failure is the more informative one, so it wins.
    loop_result?;
    if !finished {
        anyhow::bail!(
            "an operation was still running on the NAS and did not finish in time — \
             check Download Station before assuming it completed"
        );
    }

    tracing::info!("exiting");
    Ok(())
}

/// Draw, wait, dispatch — until the app says to stop or the terminal fails.
///
/// Split out of [`run_tui`] purely so that **every** way out of it, `?`
/// included, still reaches the shutdown wait.
async fn event_loop(
    app: &mut App,
    rx: &mut Receiver,
    refresh: &RefreshHandle,
    ops: Option<&OpContext>,
    terminal: &mut TerminalGuard,
    in_flight: &mut Vec<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let mut pending_read = None;

    while !app.should_quit() {
        in_flight.retain(|handle| !handle.is_finished());
        // The app refuses `d`, `p` and `u` while a batch is live, so the
        // refusal is on screen *before* the user commits to a plan. Refusing
        // only here — after `take_confirmed_delete` had already consumed it —
        // dropped the plan on the floor and said so in a footer line the
        // running batch overwrote a moment later.
        app.set_op_in_flight(!in_flight.is_empty());

        terminal.draw(app)?;
        // A page jump is a screenful of the table, so the app is told how tall
        // that is after every draw — including after a resize.
        app.set_page_size(terminal.page_size()?);

        let read =
            pending_read.get_or_insert_with(|| tokio::task::spawn_blocking(terminal_event::read));
        // The select's result is returned rather than acted on in the branch
        // bodies, so the mutable borrow of `pending_read` ends with the
        // expression and the arm below can clear it.
        let next = tokio::select! {
            result = read => Next::Terminal(result),
            Some(app_event) = rx.recv() => Next::Background(app_event),
        };

        match next {
            Next::Terminal(result) => {
                pending_read = None;
                app.handle_event(result??);
            }
            Next::Background(app_event) => app.apply_event(app_event),
        }

        // `r` is a request, not an action: the app records it and the poller —
        // which owns the interval and the client — does the work.
        if app.take_refresh_request() {
            refresh.request();
        }

        // A confirmed delete runs as its own task and reports back through the
        // same channel as the poller. The loop does not wait for it: deleting
        // twenty torrents must not freeze the terminal.
        //
        // **One batch at a time.** Two overlapping delete runs would interleave
        // their pause/delete phases against the same NAS and report two sets of
        // progress into one footer line, and the second could reach a task the
        // first had already removed. `App` is what says no (the flag above);
        // the guard here only decides whether to *drain* the hook, so a plan
        // that somehow arrived anyway stays armed for the next pass instead of
        // being taken and thrown away.
        if in_flight.is_empty()
            && let Some(plan) = app.take_confirmed_delete()
        {
            match ops {
                Some(ops) => {
                    in_flight.push(event::spawn_delete(ops.clone(), plan, app.delete_options));
                }
                // `--fixture` has no client at all, which is also why it forces
                // `DeleteOptions::dry_run()` — the dialog never promises a
                // delete that could not happen.
                None => tracing::warn!(
                    items = plan.len(),
                    "a delete was confirmed in offline fixture mode; there is nothing to run it against"
                ),
            }
        }

        // `p` / `u`, the same handshake: one batch call off the loop, reporting
        // through the same channel and refreshing the table when it finishes.
        if in_flight.is_empty()
            && let Some(request) = app.take_requested_op()
        {
            match ops {
                Some(ops) => {
                    in_flight.push(event::spawn_task_op(
                        ops.clone(),
                        request.op,
                        request.tasks,
                        app.delete_options.dry_run,
                    ));
                }
                None => tracing::warn!(
                    op = request.op.label(),
                    tasks = request.tasks.len(),
                    "an operation was requested in offline fixture mode; there is nothing to run it against"
                ),
            }
        }
    }

    Ok(())
}

/// How long a still-running batch may go **without reporting progress** before
/// the wait gives up on it.
///
/// Per item, not per batch: every item of a delete sends an
/// [`AppEvent::OpProgress`] as it finishes, and each one restarts this clock, so
/// a twenty-item batch quit part-way is allowed twenty times this rather than
/// being cut at item seven. Sized as long as a single File Station delete may
/// itself take ([`syno_clean::api::file_station::DELETE_TIMEOUT`]), which is the
/// longest one item can legitimately be silent for.
///
/// Finite, because a wait that cannot end is indistinguishable to the user from
/// the hang it replaced, and `Ctrl-C` out of it abandons the batch in exactly
/// the place this code exists to protect. When it does expire the process says
/// what was still running and **exits non-zero**.
const IN_FLIGHT_GRACE: Duration = Duration::from_secs(300);

/// Stop the background work, in the one order that cannot deadlock.
///
/// The poller and the op tasks **share one channel**, and the main loop has
/// stopped draining it. A poller left running keeps pushing a task list every
/// `refresh_secs` into that channel's [`EVENT_CHANNEL_CAPACITY`] slots; once
/// they are full the op task's next `OpProgress` send blocks forever and the
/// wait below never returns — with the terminal already restored, so the only
/// way out is the `Ctrl-C` that abandons the batch mid-delete. Hence: abort the
/// poller first, then wait *while draining*.
///
/// [`EVENT_CHANNEL_CAPACITY`]: syno_clean::event::EVENT_CHANNEL_CAPACITY
///
/// Returns whether everything finished; `false` means the wait expired and the
/// caller must exit non-zero.
#[must_use]
async fn shutdown(
    rx: Receiver,
    poller: Option<&tokio::task::JoinHandle<()>>,
    in_flight: Vec<tokio::task::JoinHandle<()>>,
    grace: Duration,
) -> bool {
    if let Some(poller) = poller {
        poller.abort();
    }
    await_in_flight(rx, in_flight, grace).await
}

/// Let any still-running op batch finish before the process goes away.
///
/// Dropping these handles would not cancel the tasks, but returning from
/// `main` would: the runtime shuts down and a delete stops wherever it had got
/// to. Between the File Station delete and the Download Station one, that is a
/// task left pointing at data it no longer has.
///
/// **The event channel keeps being drained for the whole wait.** The events
/// have nowhere to go — there is no terminal left to draw them on — but a task
/// reporting progress per item is a task that blocks on a full channel, and
/// blocking it here would wedge the very batch this is waiting for.
///
/// The drained events are *also* what bounds the wait. `grace` is a
/// **no-progress** timeout, restarted by every item the batch reports, rather
/// than one budget for the whole remaining queue: a twenty-item delete of large
/// directories would blow through a single budget and be cut somewhere between
/// "the files are gone" and "the task is gone", which is precisely the state
/// this function exists to avoid. Each item is echoed on stderr, so the user
/// watches the batch drain instead of staring at one line — and if the wait does
/// expire, the last thing printed names what was still running.
///
/// Returns `false` if it gave up.
#[must_use]
async fn await_in_flight(
    mut rx: Receiver,
    handles: Vec<tokio::task::JoinHandle<()>>,
    grace: Duration,
) -> bool {
    let outstanding: Vec<_> = handles.into_iter().filter(|h| !h.is_finished()).collect();
    if outstanding.is_empty() {
        return true;
    }

    eprintln!(
        "waiting for {} operation(s) still running on the NAS…",
        outstanding.len()
    );

    let joined = async {
        for handle in outstanding {
            if let Err(err) = handle.await {
                tracing::warn!(%err, "an operation task did not finish cleanly");
            }
        }
    };
    tokio::pin!(joined);

    // Once every sender is gone nothing can arrive and nothing can block, so
    // the `recv` branch switches itself off rather than spinning on the `None`
    // a closed channel returns immediately and forever.
    let mut open = true;
    let mut last_progress: Option<String> = None;

    loop {
        let step = tokio::time::timeout(grace, async {
            tokio::select! {
                () = &mut joined => Step::Finished,
                event = rx.recv(), if open => Step::Event(event),
            }
        })
        .await;

        match step {
            Ok(Step::Finished) => return true,
            Ok(Step::Event(Some(event))) => {
                if let Some(line) = progress_line(&event) {
                    eprintln!("  {line}");
                    last_progress = Some(line);
                }
            }
            Ok(Step::Event(None)) => open = false,
            Err(_) => {
                let running = last_progress
                    .as_deref()
                    .unwrap_or("an operation that reported no progress at all");
                tracing::error!(
                    secs = grace.as_secs(),
                    running,
                    "gave up waiting for an operation still running on the NAS"
                );
                eprintln!(
                    "nothing has completed for {}s; exiting anyway — still running after: \
                     {running}. Check Download Station: a task may have been removed while \
                     its files were not, or the other way round.",
                    grace.as_secs()
                );
                return false;
            }
        }
    }
}

/// What the wait saw: the batch finishing, or one event off the channel.
enum Step {
    Finished,
    Event(Option<AppEvent>),
}

/// How one drained event reads on stderr, or `None` for the events that say
/// nothing about progress (a poller task list, most of them).
fn progress_line(event: &AppEvent) -> Option<String> {
    match event {
        AppEvent::OpProgress {
            op,
            done,
            total,
            detail,
        } => Some(format!("[{done}/{total}] {} · {detail}", op.label())),
        AppEvent::OpDone {
            op,
            succeeded,
            skipped,
            failed,
        } => Some(format!(
            "{} finished: {succeeded} succeeded, {skipped} skipped, {failed} failed",
            op.label()
        )),
        _ => None,
    }
}

/// Which of the two event sources won a pass of the loop.
enum Next {
    Terminal(std::result::Result<std::io::Result<Event>, tokio::task::JoinError>),
    Background(AppEvent),
}

/// The hidden `--dump-api-info` / `--dump-tasks-json` modes.
///
/// These exist so the real shapes can be captured from an actual NAS — in
/// particular `tests/fixtures/task_list.json`, which the parser tests and the
/// offline `--fixture` mode are built on. The bodies are printed verbatim,
/// with no reformatting, so the capture is exactly what DSM sent.
async fn dump(cli: &Cli, resolved: &ResolvedConfig, paths: &Paths) -> Result<()> {
    let mut client = SynoClient::new(resolved)?;
    client
        .discover()
        .await
        .map_err(|err| startup_failure(&err, resolved))?;

    // Discovery needs no session, so this half works before any credentials
    // exist — useful precisely when a login is what is being debugged.
    if cli.dump_api_info {
        println!("{}", client.discovery_json().await?);
    }

    if cli.dump_tasks_json {
        let client = authenticate(client, resolved, paths)
            .await
            .map_err(|err| startup_failure(&err, resolved))?;
        println!("{}", download_station::list_tasks_json(&client).await?);
    }
    Ok(())
}

/// The `--logout` mode: invalidate the cached session and stop.
///
/// This is the **only** thing that ever calls `SYNO.API.Auth` `logout`. A normal
/// quit deliberately leaves the session alive — that is what makes the next
/// start fast — so forgetting it has to be something the user asks for.
///
/// No password is resolved: with nothing cached there is no session to end, and
/// prompting for credentials in order to throw a session away would be an odd
/// trade. The local entry is dropped **whether or not DSM accepted the call**,
/// since a sid this program has just tried to invalidate is not one worth
/// keeping; the DSM error is still surfaced afterwards.
async fn logout(resolved: &ResolvedConfig, paths: &Paths) -> Result<()> {
    let session_file = paths.session_file();
    let key = resolved.session_key();
    let mut cache = SessionCache::load(&session_file);

    let Some(sid) = cache.sid(&key).map(str::to_string) else {
        println!("no cached session for {key}");
        return Ok(());
    };

    let mut client = SynoClient::new(resolved)?;
    client
        .discover()
        .await
        .map_err(|err| startup_failure(&err, resolved))?;
    let result = auth::logout(&client.with_sid(sid)).await;

    cache.remove(&key);
    cache.save(&session_file)?;

    result.map_err(|err| startup_failure(&err, resolved))?;
    println!("logged out of {key}");
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
) -> error::Result<SynoClient> {
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
                Err(err) => return Err(err),
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
fn prompt_otp() -> std::io::Result<String> {
    use std::io::{BufRead, Write};

    print!("DSM 2-step verification code: ");
    std::io::stdout().flush()?;
    let mut code = String::new();
    std::io::stdin().lock().read_line(&mut code)?;
    Ok(code.trim().to_string())
}

#[cfg(test)]
mod tests {
    //! Startup, the terminal and the loop itself all need a TTY or a NAS. What
    //! is testable here is the shutdown — and it is worth testing, because
    //! getting it wrong hangs the process *after* the terminal has been
    //! restored, which looks like a crash and is escapable only by the
    //! `Ctrl-C` that abandons a delete half-way through.

    use super::*;
    use syno_clean::event::{EVENT_CHANNEL_CAPACITY, OpKind};

    /// Every wait in these tests is bounded: a shutdown that does not finish is
    /// exactly the bug, and an unbounded `await` would express it as a hung
    /// suite rather than as a failure.
    const TEST_LIMIT: Duration = Duration::from_secs(10);

    /// A batch task that reports progress per item, like `event::run_delete`.
    ///
    /// It sends **more events than the channel has slots**, so it can only run
    /// to completion if somebody is draining the receiving end.
    fn chatty_batch(tx: event::Sender, items: usize) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            for done in 1..=items {
                let progress = AppEvent::OpProgress {
                    op: OpKind::Delete,
                    done,
                    total: items,
                    detail: String::new(),
                };
                if tx.send(progress).await.is_err() {
                    return;
                }
            }
        })
    }

    #[tokio::test]
    async fn quitting_during_a_batch_neither_deadlocks_nor_leaves_the_poller_running() {
        // The regression: the poller kept filling the shared channel while
        // nothing drained it, so the delete's next progress send blocked on
        // backpressure and the wait never returned.
        let (tx, rx) = event::channel();

        let poller_tx = tx.clone();
        let poller = tokio::spawn(async move {
            while poller_tx.send(AppEvent::Tasks(Vec::new())).await.is_ok() {}
        });
        let batch = chatty_batch(tx, EVENT_CHANNEL_CAPACITY * 3);

        let finished = tokio::time::timeout(
            TEST_LIMIT,
            shutdown(rx, Some(&poller), vec![batch], IN_FLIGHT_GRACE),
        )
        .await
        .expect("the shutdown must drain the channel rather than deadlock on it");
        assert!(finished, "the batch ran to completion");

        assert!(
            poller.await.is_err_and(|err| err.is_cancelled()),
            "the poller must be aborted *before* the wait, not after run_tui returns"
        );
    }

    #[tokio::test]
    async fn a_batch_that_never_finishes_does_not_wedge_the_process_forever() {
        // The bound on the wait. A `Ctrl-C` out of an unbounded one abandons
        // the batch in the very place the wait exists to protect, so the
        // program gives up on its own terms and says so.
        let (tx, rx) = event::channel();
        let stuck = tokio::spawn(async move {
            let _tx = tx;
            std::future::pending::<()>().await;
        });

        let finished = tokio::time::timeout(
            TEST_LIMIT,
            shutdown(rx, None, vec![stuck], Duration::from_millis(50)),
        )
        .await
        .expect("the wait is bounded");
        assert!(
            !finished,
            "giving up on a live batch must be reported, so the process exits non-zero"
        );
    }

    #[tokio::test]
    async fn the_bound_is_per_item_not_per_batch() {
        // A twenty-item delete of large directories cannot fit in one budget
        // sized for a *single* File Station delete, and cutting it at item
        // seven is exactly the mid-batch abandonment the wait exists to
        // prevent. Every item reported restarts the clock, so a batch that
        // keeps making progress runs as long as it needs to.
        let (tx, rx) = event::channel();
        let grace = Duration::from_millis(120);
        let items = 8;

        let batch = tokio::spawn(async move {
            for done in 1..=items {
                tokio::time::sleep(grace / 2).await;
                let progress = AppEvent::OpProgress {
                    op: OpKind::Delete,
                    done,
                    total: items,
                    detail: format!("item {done}"),
                };
                if tx.send(progress).await.is_err() {
                    return;
                }
            }
        });

        // The whole batch takes four grace periods; only its silences are
        // shorter than one.
        let finished = tokio::time::timeout(TEST_LIMIT, shutdown(rx, None, vec![batch], grace))
            .await
            .expect("the wait is bounded");
        assert!(
            finished,
            "a batch reporting progress must not be cut off by a per-batch budget"
        );
    }

    #[tokio::test]
    async fn a_batch_that_goes_silent_mid_way_still_expires() {
        // The other half: progress *stops* meaning the item in flight has gone
        // quiet for longer than a single delete may take.
        let (tx, rx) = event::channel();
        let grace = Duration::from_millis(80);
        let stalled = tokio::spawn(async move {
            let progress = AppEvent::OpProgress {
                op: OpKind::Delete,
                done: 1,
                total: 9,
                detail: "Some.Release: deleted".to_string(),
            };
            let _ = tx.send(progress).await;
            std::future::pending::<()>().await;
        });

        let finished = tokio::time::timeout(TEST_LIMIT, shutdown(rx, None, vec![stalled], grace))
            .await
            .expect("the wait is bounded");
        assert!(!finished);
    }

    #[test]
    fn the_expiry_message_can_name_what_was_still_running() {
        // What the user is told when the wait gives up: which item the batch
        // had got to, not just that "an operation" was running.
        let line = progress_line(&AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 7,
            total: 20,
            detail: "Some.Release: deleted".to_string(),
        })
        .expect("progress reads as a line");
        assert!(line.contains("7/20"), "{line}");
        assert!(line.contains("Some.Release"), "{line}");
        // A poller task list says nothing about progress and must not restart
        // the clock's *report* — the poller is aborted first anyway.
        assert!(progress_line(&AppEvent::Tasks(Vec::new())).is_none());
    }

    #[tokio::test]
    async fn nothing_in_flight_waits_for_nothing() {
        let (tx, rx) = event::channel();
        drop(tx);
        let finished = tokio::spawn(async {});
        tokio::time::timeout(TEST_LIMIT, async {
            while !finished.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a task that does nothing finishes");

        // A handle that has already resolved is not something to wait on, and
        // the quit path must not print the "still running" line for it — nor
        // may the closed channel spin the drain loop.
        let completed = tokio::time::timeout(
            TEST_LIMIT,
            await_in_flight(rx, vec![finished], Duration::from_millis(50)),
        )
        .await
        .expect("an empty wait returns immediately");
        assert!(completed, "nothing outstanding is not a timeout");
    }
}
