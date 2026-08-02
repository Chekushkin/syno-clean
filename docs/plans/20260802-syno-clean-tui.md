# syno-clean — Synology Download Station TUI

## Overview

A terminal UI, written in Rust, for reviewing and cleaning up Synology Download Station tasks.

- **Problem it solves**: DSM's web Download Station UI is slow to load, awkward for bulk operations, and — critically — deleting a task there leaves the downloaded files sitting on the volume. Reclaiming space means a second trip through File Station to hunt down and delete each directory by hand.
- **What it does**: lists every Download Station task in a sortable/filterable table with live stats (size, progress, speed, ratio, seeds/peers, ETA, destination), lets you navigate with arrows, multi-select with space, and hit `d` to delete — which shows a confirmation listing exactly what will go, then removes both the DSM task *and* the files on disk.
- **Extras in v1**: sort by any column, filter by status (downloading / seeding / finished / paused / error), text search over titles, live auto-refresh, and pause/resume of selected tasks.
- **Integration**: pure DSM HTTP API client — nothing is installed on the NAS. Talks to `SYNO.API.Auth`, `SYNO.DownloadStation.Task`, and `SYNO.FileStation.*` over the same authenticated session.
- **Open source**: this is intended for public release, so licensing, README, CI, contributor docs, and cross-platform release binaries are part of the deliverable, not an afterthought.

## Context (from discovery)

- **Greenfield**: `/Users/eduardmacarov/Developer/syno-clean` is empty. Not a git repository. Everything below is created from scratch.
- **No Rust toolchain**: `cargo` and `rustc` are not on `PATH`. Bootstrapping rustup is Task 1.
- **No existing patterns to follow**: conventions are established by this plan (module layout, error handling, config precedence).
- **Dependencies identified**: `ratatui` (crossterm consumed via `ratatui::crossterm`, **not** as a separate dependency), `tokio` (async runtime), `reqwest` + `rustls` (HTTP), `serde`/`serde_json`/`toml` (data), `clap` (CLI), `thiserror`/`anyhow` (errors), `etcetera` (XDG paths), `unicode-width` (correct table truncation), `rpassword` (password prompt), `tracing` (file logging — the TUI owns stdout, so logs cannot go there).

## Development Approach

- **Testing approach**: Regular (code first, then tests), with **deliberately minimal test coverage** — see Testing Strategy. This is an explicit decision, not an oversight.
- Complete each task fully before moving to the next.
- Make small, focused changes.
- **Every task must end in a compiling, lint-clean state**: `cargo build` succeeds, `cargo clippy --all-targets -- -D warnings` is clean, `cargo fmt` applied.
- **All existing tests must pass before starting the next task** — no exceptions.
- **CRITICAL: update this plan file when scope changes during implementation.**
- Backward compatibility is not a concern before 1.0, but the config file format should not churn gratuitously once documented — unknown config keys are warned about and ignored, never a hard error.

## Testing Strategy

Coverage is intentionally scoped to **pure logic where bugs are silent and expensive**. The reasoning: TUI rendering and HTTP plumbing are cheap to verify by running the program and immediately obvious when broken, while a wrong sort comparator, a misparsed byte count, or — worst case — a mis-resolved delete path fails quietly and destroys data.

**Tested (unit tests, no network, no terminal):**
- `format.rs` — byte/speed/ETA/percent/ratio formatting and display-width truncation
- `model.rs` — DSM JSON → `Task` parsing, driven by a checked-in fixture
- `view.rs` — sort comparators, status filter, text search
- `error.rs` — DSM numeric error code → message mapping
- `api/client.rs` — response envelope deserialization (success and error shapes)
- **`delete.rs` — delete-path resolution, safety guards, and delete-op ordering.** This is the highest-value test in the project. It must be thorough.
- `app.rs` — selection set toggling and cursor/selection stability across a refresh

**Not tested (verified by running the binary):**
- ratatui widget rendering and layout
- key event wiring
- live HTTP calls against DSM

**Testability seam**: request-parameter construction is extracted into **pure builder functions** (`build_*_params`) returning `Vec<(&str, String)>`, and delete ordering into a pure `plan_delete_ops`. These are unit-tested without any network. No mocking framework and no trait abstraction over the HTTP client — a single implementation does not warrant one.

**No e2e/UI test framework.** CI runs `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test`.

## Progress Tracking

- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update this plan if implementation deviates from the original scope

## Solution Overview

**Architecture: tokio async + ratatui, with a background poller.**

```
  spawn_blocking   ┌────────────────────────────────┐
  (event::read)  ──▶│        main event loop       │
  one JoinHandle,  │   tokio::select! {             │
  held across      │     terminal event  → App      │──▶ ratatui render
  iterations       │     AppEvent::Tasks → App      │
                   │     AppEvent::OpDone→ App      │
  poller task ────▶│   }                            │
  (interval)       └────────────────────────────────┘
  op tasks    ────▶        mpsc::Sender<AppEvent>
  (delete/pause)
```

⚠️ **The input source is `spawn_blocking(event::read)`, not `crossterm::EventStream`** — the `event-stream` feature is not enabled by ratatui's crossterm re-export and adding crossterm directly is forbidden (Task 1's ⚠️ note). The pending `JoinHandle` is kept across loop iterations rather than re-created per pass: a blocking read cannot be cancelled, so a `select!` that dropped it whenever an `AppEvent` won the race would leave one orphaned stdin reader per poller tick, all of them then taking turns swallowing keystrokes. Exactly one read exists at any moment.

- A **poller task** refreshes the task list on an interval and pushes `AppEvent::Tasks(Vec<Task>)` down an mpsc channel. The UI never blocks on the network.
- **Delete and pause/resume run as spawned tasks**, reporting back via `AppEvent::OpProgress` / `OpDone`. Deleting twenty torrents does not freeze the terminal.
- **`App` holds all state**; rendering is a pure function of `&App`. Sorting/filtering produce a `Vec<usize>` of indices into the task list rather than cloning or reordering the source data.
- **Selection is keyed by task ID, not row index**, so a refresh that reorders or removes rows never silently reassigns what you have selected. Same for the cursor.

**Key design decisions:**

1. **DSM 7 only, using the documented `SYNO.DownloadStation.Task` API.** DSM 7 also ships the newer `SYNO.DownloadStation2.Task` (what the web UI uses), but its `list` method is undocumented and it returns numeric status codes and a different `additional` shape. The v1 API is documented, still present and supported in DSM 7, and returns the string statuses and object file lists this plan's model is built around. **All four operations — list, delete, pause, resume — come from the same v1 API**, so there is no mixed-API seam. DS2 is noted as a possible future migration, not a v1 concern. A DSM 6 NAS gets a clear, actionable error.
2. **No hardcoded API versions.** `SYNO.API.Info` is queried once at startup; each call uses the highest version within the discovered `minVersion..maxVersion` range that this client understands. A missing API produces a specific error ("File Station is not installed on this NAS").
3. **Three-phase delete, ordered for recoverability.** The DS API removes the task; it does **not** remove the payload. So: **(a) pause the task if it is active**, so Download Station is not holding file handles or re-creating directories underneath us; **(b) delete the files** via File Station; **(c) delete the task**. If any phase fails, the later phases are skipped and the task survives still pointing at its data — the recoverable ordering. See Technical Details for the per-status rules.
4. **Path resolution is treated as dangerous, and refuses rather than guesses.** Deriving "which directory holds this torrent" from the API is the one place this tool can destroy the wrong data. It gets a dedicated module, syntactic guards, a *semantic* existence check against the NAS before any recursive delete, and the bulk of the test suite. When the on-disk name is ambiguous, the item is **skipped**, never guessed at.
5. **Session reuse.** The `sid` is cached to a `0600` file, keyed by `host:port` and username so multiple NASes/accounts do not evict each other. On DSM error 106/107/119 (session timeout / interrupted / invalid) the client re-logs-in once transparently and retries. **Normal quit does not log out** — that would invalidate the cached sid and defeat the caching. Logout is available only via an explicit `--logout` flag.
6. **Logs go to a file, never stdout.** The TUI owns the terminal. `tracing` writes to the XDG cache dir (overridable with `--log-file`), which also makes bug reports from users useful.

## Technical Details

### Module layout

```
src/
  main.rs                  entrypoint, runtime setup, terminal guard
  cli.rs                   clap definitions
  config.rs                TOML config, env overrides, validation, sid cache
  error.rs                 Error enum + DSM code mapping
  format.rs                human-readable bytes/speed/eta/percent, width-correct truncation
  model.rs                 Task, TaskFile, TaskStatus, JSON → Task
  view.rs                  SortKey/SortDir/StatusFilter/search → visible indices
  delete.rs                delete-path resolution, safety guards, op ordering
  app.rs                   App state, key handling, selection
  event.rs                 AppEvent, poller task, op tasks
  ui/
    mod.rs                 frame layout + dispatch
    table.rs               task table widget
    dialog.rs              confirmation modal, help overlay
  api/
    mod.rs
    client.rs              reqwest client, API discovery, envelope, sid handling, retry
    auth.rs                login / logout
    download_station.rs    list / delete / pause / resume
    file_station.rs        path info lookup, delete files
tests/
  fixtures/task_list.json  DSM list response used by parser tests and --fixture mode
```

### DSM API surface

**Discovery is a special case**: `SYNO.API.Info` is served from a fixed `/webapi/query.cgi`, *not* `entry.cgi`. Every subsequent request is built as `{scheme}://{host}:{port}/webapi/{discovered path}` using the `path` and version range returned by that query.

| Purpose | API | Method | Notable params |
|---|---|---|---|
| Discovery | `SYNO.API.Info` | `query` | `query=all` — hardcoded to `/webapi/query.cgi`, version 1 |
| Login | `SYNO.API.Auth` | `login` | `account`, `passwd`, `session=DownloadStation`, `format=sid`, `otp_code` |
| Logout | `SYNO.API.Auth` | `logout` | `session=DownloadStation` — only on `--logout` |
| List tasks | `SYNO.DownloadStation.Task` | `list` | `additional=detail,transfer,file`, `offset`, `limit` |
| Delete task | `SYNO.DownloadStation.Task` | `delete` | `id=<comma-separated>`, `force_complete=false` |
| Pause / resume | `SYNO.DownloadStation.Task` | `pause` / `resume` | `id=<comma-separated>` |
| Path info | `SYNO.FileStation.List` | `getinfo` | `path=<json array>` — pre-delete existence/type check |
| Delete files | `SYNO.FileStation.Delete` | `start` → `status` | `path=<json array>`, `recursive=true`, then poll `taskid` |

Every response is the standard envelope:

```json
{ "success": true,  "data": { ... } }
{ "success": false, "error": { "code": 119 } }
```

⚠️ **List-valued parameter encoding differs between APIs and DSM builds** — Download Station v1 takes comma-separated strings, File Station takes JSON arrays. Task 5 includes hidden `--dump-api-info` and `--dump-tasks-json` flags that print the raw discovery and list responses so the real shapes are confirmed against the actual NAS. Encoding is built by pure `build_*_params` functions so it is unit-testable and changeable in one place.

File deletion uses `start` + `status` polling rather than the blocking `delete` method, because recursively removing a large torrent directory can exceed the HTTP timeout.

### Delete-path resolution (the dangerous part)

A task's `additional.detail.destination` is normally **share-relative with no leading slash** (e.g. `downloads`, or `video/movies`), though some configurations surface an absolute `/volumeN/share/...`. File Station needs a path rooted at the share: `/downloads/Some.Torrent.Name`.

`additional.file` is a **list of objects** (`filename`, `size`, `priority`, `selected`) — not strings. The model reflects this as `Vec<TaskFile>`.

Resolution rules, in order:
1. **File list present with a single common top-level component** → use that component. This is the authoritative on-disk name and correctly handles a torrent whose display title differs from the directory it actually wrote.
2. **File list present but entries share no single top-level component** → **REFUSE**. Report the item as skipped. Do *not* fall back to `title` — this is precisely the ambiguous case where a guessed path could match an unrelated existing folder and get recursively deleted.
3. **File list absent or empty** (non-BT tasks: HTTP/FTP/NZB downloads) → fall back to `title`.
4. Normalize `destination`: strip a leading `/volumeN` if present, then trim surrounding slashes.
5. Join: `format!("/{}/{}", normalized_destination, name)`.

**Syntactic guards** — a resolved path is refused (per-item error, task left untouched) if it:
- is empty, `/`, or has fewer than two path components (never delete a share root)
- contains a `..` component
- has a `name` component that is empty or `.`
- does not start with `/`

**Semantic guard** — before any recursive delete, `SYNO.FileStation.List` `getinfo` is called on the resolved path:
- **not found** → report as *skipped*, not as an error the user must chase (the files were probably already removed by hand); the DSM task is still deleted, since that is the harmless half
- **found** → proceed

**Snapshot semantics**: the `DeletePlan` is a **snapshot taken at the moment the confirmation dialog opens**. Execution operates only on that snapshot, and `validate_path` is re-run immediately before each File Station call. Task-list refreshes are suspended while `Mode::Confirm` is active, so what the user reads on screen is exactly what gets deleted.

### Delete ordering by task status

| Task status | Ordering |
|---|---|
| Downloading, Seeding, Waiting, Finishing, HashChecking, Extracting | pause → confirm paused → delete files → delete task |
| Paused, Finished, Error | delete files → delete task |

For **incomplete** tasks, Download Station cleans up its own partial/temp data when the task is deleted. The file-delete phase therefore treats "path not found" as success, not failure.

If pause fails → skip both deletes, report the item failed.
If file delete fails → skip the task delete, report the item failed (the task still points at its data, so nothing is orphaned).

### Config

Paths use **XDG semantics on all platforms** (via `etcetera`'s XDG strategy), so the documented paths are the real ones on macOS too:

- config: `$XDG_CONFIG_HOME/syno-clean/config.toml`, default `~/.config/syno-clean/config.toml` (`--config` overrides)
- cache/logs: `$XDG_CACHE_HOME/syno-clean/`, default `~/.cache/syno-clean/`

```toml
host         = "nas.local"
port         = 5001
https        = true
insecure     = false   # accept self-signed certificate
username     = "eduard"
refresh_secs = 3
delete_files = true    # false = remove the DSM task only, leave files
```

Precedence: **CLI flags > `SYNO_CLEAN_*` env vars > config file > defaults.** Unknown config keys are logged as a warning and ignored.

`host` and `username` are **required after the merge**. Validation lives in `config::merge`, so every later module can assume they are present. A missing config *file* is not itself an error — `syno-clean --host nas.local --user eduard` works on a clean machine. Only when required values are still unresolved after merging does the program write a commented config template and exit with an actionable message, without entering the TUI.

The password is never stored in the config file. It comes from `SYNO_CLEAN_PASSWORD`, or is prompted for interactively (`rpassword`, before the alternate screen is entered). 2FA via `SYNO_CLEAN_OTP` or prompt when DSM returns error 403.

Session cache: `~/.cache/syno-clean/session.json`, mode `0600`, keyed by `{host}:{port}/{username}`.

### Keybindings

| Key | Action |
|---|---|
| `↑`/`↓`, `k`/`j` | move cursor |
| `PgUp`/`PgDn`, `Home`/`End`, `g`/`G` | page / jump |
| `Space` | toggle selection on current row |
| `a` | toggle select-all (visible rows only) |
| `Esc` | clear selection / exit search / dismiss dialog |
| `d` | delete — opens confirmation |
| `p` / `u` | pause / resume selected |
| `s` / `S` | cycle sort column / reverse direction |
| `f` | cycle status filter |
| `/` | search titles (`Enter` apply, `Esc` cancel) |
| `r` | refresh now |
| `?` | help overlay |
| `q`, `Ctrl-C` | quit |

`d` with nothing selected acts on the row under the cursor.

### Table columns

`[sel] │ Name │ Status │ Size │ Progress │ ↓ Speed │ ↑ Speed │ Ratio │ Seeds/Peers │ ETA │ Destination`

Name absorbs slack and truncates with an ellipsis at correct **display width** (CJK and emoji occupy two cells — torrent titles are frequently CJK, and char-count truncation would break column alignment). The status bar shows selection count and the total size those selections will free.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): everything achievable in this repo — code, the minimal tests, CI config, docs.
- **Post-Completion** (no checkboxes): things requiring a real NAS, a GitHub repo, or a human eye.

## Implementation Steps

### Task 1: Bootstrap Rust toolchain and cargo project

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `CLAUDE.md`
- Create: `src/main.rs`

- [x] install rustup + stable toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`), confirm `cargo --version` — installed 1.97.1 (cargo 1.97.1)
- [x] run `cargo init --name syno-clean` **first** (it initializes git and writes its own `.gitignore`), then append `*.log` and `.DS_Store`; set the `edition` in `Cargo.toml` explicitly rather than relying on the cargo default — edition `2024`, existing git history and `docs/` preserved
- [x] add dependencies: `ratatui` (with its `crossterm` feature — **do not add crossterm as a separate dependency**; use `ratatui::crossterm` everywhere to avoid version-skew type errors), `tokio` (`rt-multi-thread`,`macros`,`sync`,`time`), `reqwest` (`json`,`rustls-tls`, `default-features = false`), `serde` (`derive`), `serde_json`, `toml`, `clap` (`derive`), `thiserror`, `anyhow`, `etcetera`, `unicode-width`, `rpassword`, `tracing`, `tracing-subscriber`, `tracing-appender`, `futures`
- [x] pin `rust-toolchain.toml` to the **explicit installed version** (e.g. `channel = "1.xx.y"`, not `"stable"`, so CI is reproducible), components `rustfmt` + `clippy` — `channel = "1.97.1"`
- [x] create a `CLAUDE.md` stub recording the conventions this plan establishes (module layout, config precedence, delete ordering, path-safety invariants) — filled in as they land, not written from scratch at the end
- [x] verify `cargo build` and `cargo clippy --all-targets -- -D warnings` are clean
- [x] no tests this task (scaffolding only)

⚠️ **Dependency notes discovered during Task 1** (plan text above kept verbatim; actuals recorded here):
- `reqwest` 0.13 renamed the `rustls-tls` feature to **`rustls`**. Resolved deps are `default-features = false, features = ["json", "rustls", "query", "form"]` — `query` and `form` are now separate features in 0.13 and are needed for the `build_*_params` request style (Tasks 4/5/15).
- `ratatui` resolved to **0.30.2**, which pulls crossterm 0.29 through `ratatui-crossterm`; `ratatui::crossterm` re-export confirmed present. Crossterm's **`event-stream` feature is NOT enabled** by that path, so `crossterm::event::EventStream` is unavailable without adding crossterm directly (which this plan forbids). **Task 11 must build the async input source another way** — e.g. a `spawn_blocking` reader on `event::read()` feeding the same mpsc channel as the poller. Noted in `CLAUDE.md`.

### Task 2: Error type and DSM error-code mapping

**Files:**
- Create: `src/error.rs`
- Modify: `src/main.rs`

- [x] define `Error` enum with `thiserror`: `Http`, `Dsm { code, api }`, `Config`, `Io`, `Parse`, `Auth`, `UnsafePath`, `ApiUnavailable`
- [x] implement `dsm_message(code, api) -> &'static str` for common codes: 100 unknown error, 101 invalid parameter, 102 API does not exist, 103 method does not exist, 105 insufficient user privilege, 106 session timeout, 107 session interrupted by duplicate login, 119 invalid SID — signature landed as `-> String` (see note below); 104 added as well
- [x] implement the **auth** code table correctly — 400 no such account or incorrect password, **401 account disabled**, **402 permission denied**, 403 2-step verification code required, 404 failed to authenticate 2-step verification code, 406 enforce 2-step verification, 407 blocked IP source, 408 expired password (cannot change), 409 expired password, 410 password must be changed
- [x] add `Result<T>` alias; wire `error` module into `main.rs` — via a new `src/lib.rs` (see note below)
- [x] write tests for `dsm_message` — known codes map to specific text (assert 400/401/402 individually, since these are the ones most easily transposed), unknown codes fall back to a generic message including the numeric code
- [x] run `cargo test` — must pass before task 3 — 13 tests pass

⚠️ **Decisions taken during Task 2** (plan text above kept verbatim; actuals recorded here):
- `dsm_message` returns **`String`, not `&'static str`**. The two bullets conflict: a `&'static str` cannot embed the numeric code that the fallback test requires. `String` satisfies both; the known-code tables are still `&'static str` internally.
- The auth 400-range table is applied **only when `api == "SYNO.API.Auth"`**. Download Station and File Station have their own, different 400-range meanings, so a DS 400 falls through to the generic "unrecognized DSM error code 400" rather than rendering as "incorrect password". Covered by a test.
- ➕ **Added `src/lib.rs`.** In a bin-only crate, every `pub` item not yet reachable from `main` is a `dead_code` warning, and `-D warnings` makes that a hard failure on *every* task until the module is wired in. A library root removes the friction without disabling the lint and lets `tests/` reach the code; `main.rs` is now a thin shell over `syno_clean::`. The module list in the layout above is otherwise unchanged. Recorded in `CLAUDE.md`.
- ➕ Added `is_session_error()` / `SESSION_ERROR_CODES` (106/107/119) and `OTP_REQUIRED_CODE` (403) here rather than in Task 4, so the retry and 2FA rules have one definition.
- ➕ `ApiUnavailable` carries an `ApiUnavailableReason` (`NotInstalled` / `VersionMismatch`) so Task 4's `pick_version` has a place to report a non-overlapping range, and missing APIs render by DSM package name ("File Station is not installed on this NAS").

### Task 3: Config file, CLI flags, env overrides, and validation

**Files:**
- Create: `src/config.rs`
- Create: `src/cli.rs`
- Modify: `src/main.rs`

- [x] define `Cli` with clap derive: `--config`, `--host`, `--user`, `--port`, `--insecure`, `--refresh-secs`, `--no-delete-files`, `--log-file`, `--dry-run`, `--logout`, `--version`
- [x] define `Config` with serde defaults and `load(path)` reading TOML from the XDG config dir via `etcetera`; unknown keys warn and are ignored (**no** `deny_unknown_fields` — an older binary must tolerate a newer config)
- [x] implement `apply_env(&mut Config)` for `SYNO_CLEAN_{HOST,PORT,HTTPS,INSECURE,USERNAME,PASSWORD,OTP,REFRESH_SECS}` — as `Config::apply_env(&mut self, EnvLookup)` plus the pure `Config::from_env`; `PASSWORD`/`OTP` are not config fields and are read by `resolve_password` / `otp_from_env`
- [x] implement `merge(config, env, cli) -> Result<ResolvedConfig>` enforcing CLI > env > file > default **and validating that `host` and `username` are present**, so later modules can assume them; plus `resolve_password()` (env, else `rpassword` prompt)
- [x] implement sid cache read/write at the XDG cache dir with `0600` permissions, **keyed by `{host}:{port}/{username}`** so two NASes or accounts do not evict each other
- [x] initialize `tracing` to the log file (never stdout) and **hold the `tracing_appender::WorkerGuard` for the process lifetime** — dropping it early silently discards buffered log lines
- [x] write tests for precedence merge (CLI beats env beats file beats default), for validation failing when host/username are unresolved, and for TOML parsing (full file, minimal file, unknown key ignored not rejected)
- [x] run `cargo test` — must pass before task 4 — 50 tests pass

⚠️ **Decisions taken during Task 3** (plan text above kept verbatim; actuals recorded here):
- **Environment reads go through an injected lookup**, `pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>`, not `std::env` directly. `main` passes `config::system_env`. Tests build a `HashMap`-backed closure, so no test mutates process-global env and the suite stays parallel-safe.
- **Filesystem locations go through a `Paths` value** with `Paths::discover()` (etcetera XDG) in production and `Paths::with_base(dir)` in tests. No test reads or writes the real `~/.config/syno-clean` or `~/.cache/syno-clean`. Added `tempfile` as a **dev-dependency** for the temp roots.
- `Config` fields are all `Option<T>` with a container-level `#[serde(default)]` rather than concrete serde defaults: precedence needs "absent" and "explicitly set to the default value" to stay distinguishable. Concrete defaults live as `DEFAULT_*` consts applied in `merge`.
- `apply_env` and `from_env` return `Result`: an unparseable `SYNO_CLEAN_PORT=abc` is an error naming the variable, not a silent fall-through to the file value.
- **Default port follows the scheme** — 5001 when https (the default), 5000 when not — rather than a single constant.
- **`refresh_secs = 0` is rejected** in `merge` rather than clamped; a zero-second poll would hammer the NAS and is far more likely a typo than an intent.
- Boolean CLI flags are **one-way switches**: `--insecure`/`--dry-run` can only turn a setting on and `--no-delete-files` only off, so an unset flag falls through to env/file instead of overriding with `false`. There is no `--https`/`--no-https` flag (not in the plan's flag list), so HTTPS comes from env or file only.
- ➕ No `SYNO_CLEAN_DELETE_FILES`: it is absent from the plan's env list, and an env var that silently disables the tool's main function is a footgun. `--no-delete-files` and the config key cover it.
- A **corrupt session cache is discarded with a warning, not an error** — a cache is an optimization and must never block startup. `SessionCache::load` therefore returns `Self`, not `Result<Self>`.
- ➕ Added `ResolvedConfig::base_url()` and `session_key()` here (rather than in Task 4) so the host/port/scheme/key formatting has exactly one definition.
- `main` now returns `anyhow::Result` and initializes logging **before** loading the config, so unknown-key warnings actually reach the log file.

### Task 4: HTTP client, API discovery, and authentication

**Files:**
- Create: `src/api/mod.rs`
- Create: `src/api/client.rs`
- Create: `src/api/auth.rs`

- [x] define `Envelope<T> { success, data: Option<T>, error: Option<DsmError> }` and a `parse_envelope` helper turning `success=false` into `Error::Dsm`
- [x] build `SynoClient`: base URL from host/port/https, `danger_accept_invalid_certs` when `insecure`, sane timeouts, shared `reqwest::Client` — 10 s connect, 30 s request
- [x] implement `query_api_info()` against **`/webapi/query.cgi`** (hardcoded — `SYNO.API.Info` is not served from `entry.cgi`), caching each API's `path`, `minVersion`, `maxVersion`; implement `pick_version(api, supported_range)` used by every later call so **no version is hardcoded**; error clearly when a required API is absent — landed as `SynoClient::discover()` + `ApiInfoMap` (see note below)
- [x] implement `login()` / `logout()` against `SYNO.API.Auth` with `session=DownloadStation`, `format=sid`, optional `otp_code`; logout is invoked **only** by `--logout`, never on normal quit
- [x] implement the request helper: build the URL from the discovered path, attach `_sid`, and on DSM **106/107/119** re-login once and retry exactly once — `SynoClient::call_text` / `call` / `call_no_data`
- [x] write tests for envelope parsing (success payload, error payload, malformed body), for the API-info map lookup (present / missing API), and for `pick_version` (clamps to `maxVersion`, errors when the NAS range and the supported range do not overlap)
- [x] run `cargo test` — must pass before task 5 — 84 tests pass

⚠️ **Decisions taken during Task 4** (plan text above kept verbatim; actuals recorded here):
- Discovery landed as **`SynoClient::discover()` + an `ApiInfoMap` value** rather than a bare `query_api_info()`. The map owns the lookup, `pick_version` and `endpoint(base_url, api, supported) -> Endpoint { api, url, version }`, so URL and version resolution have exactly one definition and are all unit-testable without a client. `pick_version_in(api, nas, supported)` is the pure range intersection behind it.
- **Three envelope entry points** instead of one: `parse_envelope` (payload required), `parse_envelope_optional` (payload may be absent), and `check_envelope` (success only, payload ignored). Logout/pause/resume answer with a bare `{"success": true}`, and the retry path has to classify a response *before* committing to a payload type.
- **Protocol violations reuse `Error::Parse`**, constructed via `serde::de::Error::custom`, rather than adding an enum variant. "Success with no data", "failure with no error code" and "JSON that is not an envelope" are all "the body was not what this client can work with". No new error variant, and the plan's variant list is unchanged.
- `Envelope`'s `data`/`error` fields carry **no `#[serde(default)]`** — serde already treats `Option` fields as optional, and the attribute would drag a spurious `T: Default` bound into the derived `Deserialize`.
- The retry is `call_text`: fetch, `check_envelope`, and only on a session code (via `error::is_session_error`) clear the sid, re-login and re-send **once**. `auth::login` deliberately goes through the lower-level `SynoClient::send`, so a login can never recurse into the retry that called it. Re-login needs stored `Credentials`; without them the session error is returned as-is.
- ➕ `AUTH_SUPPORTED = (3, 6)`. The floor is 3 because `otp_code` does not exist below it, so a 2FA account could never log in; the ceiling is 6 because nothing here needs 7 and the negotiation clamps to the overlap regardless.
- ➕ `Credentials` has a **hand-written `Debug`** that redacts the password and OTP, so no `{:?}` on it (or on `SynoClient`, which holds it) can leak a credential into the log file.
- ➕ Added `SynoClient::discovery_json()` now, so Task 5's hidden `--dump-api-info` flag is a one-liner.
- Per the plan's Testing Strategy, **nothing in this task's tests touches the network**: the 34 new tests cover envelope deserialization from JSON strings, the API-info map, `pick_version`, URL/parameter construction and the login/logout param builders. The `async fn`s are verified by running the binary.

### Task 5: Task model and Download Station list endpoint

**Files:**
- Create: `src/model.rs`
- Create: `src/api/download_station.rs`
- Create: `tests/fixtures/task_list.json`
- Modify: `src/cli.rs`

- [x] add hidden `--dump-api-info` and `--dump-tasks-json` flags that log in, call the respective endpoint, print raw JSON, and exit — `--dump-api-info` deliberately skips the login (discovery needs no session)
- [x] capture `tests/fixtures/task_list.json` from the real NAS with `--dump-tasks-json`. **If the NAS is unreachable, do not stall**: hand-write a provisional fixture from the documented v1 response shape, mark it at the top of the file as `⚠️ PROVISIONAL — not captured from a real NAS`, proceed with the remaining tasks, and re-capture in Task 21 — ⚠️ **fallback path taken: the fixture is hand-written and PROVISIONAL**, marker in a top-level `_comment` key (JSON has no comments); Task 21 must re-capture
- [x] define `TaskStatus` enum with `from_dsm_str` covering the v1 **string** statuses: `waiting`, `downloading`, `paused`, `finishing`, `finished`, `hash_checking`, `seeding`, `filehosting_waiting`, `extracting`, `error` — plus an `Unknown(String)` catch-all so an unrecognized status never panics or drops the row
- [x] define `TaskFile { filename, size, priority, selected }` and `Task { id, title, status, size, downloaded, uploaded, download_speed, upload_speed, destination, files: Vec<TaskFile>, seeders, leechers, create_time }` with derived `progress()` / `ratio()` / `eta()`
- [x] implement `build_list_params()` (pure, returns `Vec<(&str, String)>`) and `list_tasks()` requesting `additional=detail,transfer,file`
- [x] write tests parsing the fixture: field mapping, every status variant plus an unknown one, zero-size task (no divide-by-zero in `progress`), missing `additional` blocks, and a task with an empty file list
- [x] write a test for `build_list_params` (comma-separated `additional` encoding)
- [x] run `cargo test` — must pass before task 6 — 115 tests pass

⚠️ **Decisions taken during Task 5** (plan text above kept verbatim; actuals recorded here):
- ⚠️ **The fixture is PROVISIONAL.** No Synology NAS was reachable from the implementation environment, so the plan's documented fallback was taken. It is a full `list` envelope with 14 tasks covering every known status plus an unknown one (`captcha_needed`), missing and partial `additional` blocks, an empty file list, a non-BT HTTP download with no `file` key, a zero-size task, a CJK title, an emoji title, a file list with **no common root** (pre-staged for Task 13's refusal test) and a `/volume1/...` destination. A `model.rs` test asserts the `PROVISIONAL` marker is still present, so Task 21 cannot silently forget to re-capture: delete that test along with the marker.
- The wire shape lives in **private `Raw*` structs** collapsed into a flat `Task` via `#[serde(from = "RawTask")]`. Nothing outside `model.rs` reaches through `additional.transfer.…`, and every `additional` sub-block is independently optional — one malformed task must not blank the whole table.
- ➕ **Permissive numeric deserializers** (`de_u64` / `de_u32` / `de_i64_opt`): DSM returns numbers as JSON numbers *or* as strings depending on the field, version and build (file sizes and `create_time` especially). Every numeric field accepts both. The fixture deliberately mixes the two forms so the tolerance is tested.
- `TaskStatus` derives `Ord` (declaration order) for Task 7's status sort, implements `Default` as `Unknown("")` so a task with no `status` key still parses, and `from_dsm_str` trims and case-folds. `TaskStatus::KNOWN` lists the ten documented variants so the fixture's coverage can be asserted exhaustively.
- **`DS_TASK_SUPPORTED` is pinned to `(1, 1)`**, not the NAS's advertised max (3 on DSM 7). v2/v3 change the status encoding and `additional` shape that `model.rs` is built around, so following the NAS upward would silently break parsing. Asserted by a test.
- `list_tasks` always requests `limit = -1` (every task). The poller reconciles the whole list each tick; paging would make the cursor/selection reconciliation lie.
- ➕ `main` is now `#[tokio::main] async` and grew a small `authenticate()` helper (cached-sid reuse, login, 403 → OTP prompt → retry) because the dump flags need a real session. It resolves credentials **even when a cached sid exists**, so a stale sid is repaired by the client's transparent re-login retry rather than failing the run; the cost is a password prompt when `SYNO_CLEAN_PASSWORD` is unset. Task 17 owns refining the first-run experience.
- ➕ Added `Cli::is_dump()` so `main` has one definition of "print JSON and exit instead of entering the TUI".

### Task 6: Human-readable formatting helpers

**Files:**
- Create: `src/format.rs`

- [x] implement `bytes(u64) -> String` (B/KiB/MiB/GiB/TiB, one decimal above KiB)
- [x] implement `speed(u64) -> String` (`—` when zero), `duration(Option<u64>) -> String` (`2h 14m`, `∞` when unknown/stalled)
- [x] implement `percent(f64)`, `ratio(f64)`, and `truncate_ellipsis(&str, width)` using **`unicode-width` display width, not character count** — CJK and emoji occupy two terminal cells
- [x] write tests for each: boundary values (0, 1023, 1024), rounding, the zero/unknown sentinels
- [x] write tests for `truncate_ellipsis` with ASCII, a **CJK title** (asserting the result's display width fits), an emoji, and a width smaller than the ellipsis itself
- [x] run `cargo test` — must pass before task 7 — 136 tests pass

⚠️ **Decisions taken during Task 6** (plan text above kept verbatim; actuals recorded here):
- "one decimal above KiB" is read **literally**: `B` and `KiB` render as whole numbers (`1023 B`, `640 KiB`) and `MiB`/`GiB`/`TiB` get one decimal (`5.8 GiB`). A tenth of a KiB is 102 bytes — noise — and the integer form keeps the Size column narrower.
- The unit is chosen **after** rounding to the precision that will actually be printed, so a value that would format as `1024 KiB` or `1024.0 MiB` is promoted to the next unit instead. Without that, the largest number displayable in a unit is one the unit is not supposed to reach.
- `percent` takes a **fraction in `0.0..=1.0`**, not an already-multiplied percentage, because that is what `Task::progress()` returns. Out-of-range and non-finite inputs clamp to `0.0%`/`100.0%` rather than surfacing a `NaN` in the table.
- `duration` renders **at most two units** (`45s`, `1m 5s`, `2h 14m`, `1d 1h`). Seconds of precision on a four-hour download is false detail and costs column width. `None` (the stalled/unknown case from `Task::eta`) is `∞`; `Some(0)` is a *known* `0s` and deliberately reads differently.
- **Zero and unknown are different sentinels**: `speed(0)` is `—` (the task exists, it is idle), an unknown ETA is `∞`. Rendering both as `0` would tell the user a paused task is about to finish.
- ➕ Added `display_width(&str)` and the public `DASH` / `INFINITY` / `ELLIPSIS` consts. The table widget in Task 9 has to pad columns to a cell count, and `str::len` / `chars().count()` are both wrong for the CJK and emoji titles in the fixture; one definition of "how wide is this" avoids the mistake being re-made per column.
- `truncate_ellipsis` never exceeds the requested width but may come up **one cell short** when the cut lands on a double-width character — half an emoji cannot be printed. Truncation is per `char`, not per grapheme cluster: correct segmentation would mean another dependency, and the failure mode is one odd-looking character at a cut in a title that was being elided anyway.
- Tests run against the **real fixture titles** as well as hand-picked strings: every title is truncated at every width from 0 to 48 with the fit invariant asserted, plus targeted CJK and emoji cases proving a char-count truncation would have overflowed.

### Task 7: Sort, filter, and search view layer

**Files:**
- Create: `src/view.rs`

- [x] define `SortKey` (Name, Status, Size, Progress, DownSpeed, UpSpeed, Ratio, Added), `SortDir`, and `StatusFilter` (All, Downloading, Seeding, Finished, Paused, Error)
- [x] implement `View { sort_key, sort_dir, filter, search }` with `cycle_sort()`, `toggle_dir()`, `cycle_filter()`
- [x] implement `visible_indices(&[Task], &View) -> Vec<usize>` applying filter → case-insensitive substring search → stable sort
- [x] write tests for each sort key ascending and descending, including a tie-break that proves stability
- [x] write tests for each status filter and for search (case-insensitivity, no match, empty query returns everything)
- [x] run `cargo test` — must pass before task 8 — 166 tests pass

⚠️ **Decisions taken during Task 7** (plan text above kept verbatim; actuals recorded here):
- **`StatusFilter::Downloading` means "in progress", not the single `downloading` status.** It also matches `waiting`, `finishing`, `hash_checking`, `extracting` and `filehosting_waiting`. The plan names five filters but ten statuses exist; exact-matching would have left five statuses reachable only under `All`, which silently hides rows from a user who thinks they are filtering. The other four filters are exact single-status matches. A test asserts the non-`All` filters partition the fixture with no overlap.
- **A `TaskStatus::Unknown(_)` task is visible only under `All`** — deliberately. It cannot be classified without guessing, and filing it under `Error` would mislabel a task that may be perfectly healthy. Asserted by its own test so the choice cannot be made accidentally later.
- **Descending reverses the `Ordering`, never the result `Vec`.** `sort_by` is stable, so reversing the comparison preserves the incoming order of tied rows in *both* directions; reversing the vector would shuffle ties every time `S` is pressed. Three tests cover it: tied fixture pairs hold their order in both directions, `asc.reverse() != desc`, and an all-tied list keeps input order across every key × direction pair.
- Name comparison and search are **case-insensitive via `char::to_lowercase` over iterators**, not `to_lowercase()` per comparison — the comparator runs O(n log n) times per re-sort and would otherwise allocate two `String`s per call.
- `f64` keys (Progress, Ratio) use **`total_cmp`**, not `partial_cmp().unwrap()`. The values are guarded upstream, but a `NaN` must never panic mid-frame.
- **`Added` uses the derived `Option<i64>` ordering**, so a task DSM gave no `create_time` for (`dbid_010`, `dbid_011`) leads the ascending list rather than being treated as brand new.
- `cycle_sort()` **leaves the direction alone** — stepping across columns hunting for the largest task should not flip the sort underneath the user.
- ➕ Added `SortKey::label()`, `StatusFilter::label()`, `SortDir::arrow()` and `View::is_narrowed()` here rather than in Tasks 9/12/17: the column names come from this plan's Technical Details, and the sort header, the status bar and the "filter hides everything" empty state all need one definition of them.
- ➕ Added `SortKey::ALL` / `StatusFilter::ALL` (cycle order) and `SortKey::compare` / `StatusFilter::matches` as public pure functions, so the cycling and the predicates are testable without going through `visible_indices`.

### Task 8: Terminal bootstrap and ratatui skeleton

**Files:**
- Create: `src/ui/mod.rs`
- Create: `src/app.rs`
- Modify: `src/main.rs`

- [x] implement a terminal guard type that enables raw mode + alternate screen on construction and restores on `Drop` — `ui::TerminalGuard`, which **owns** the `Terminal` so a drawable terminal cannot outlive the restoration
- [x] install a panic hook that restores the terminal **before** printing the panic, so a crash never leaves a wrecked shell — `ui::install_panic_hook`, chaining to the previous hook, `Once`-guarded
- [x] define a minimal `App { tasks, view, cursor, selected, mode, status_message }` and `Mode` enum (Normal, Search, Confirm, Help)
- [x] render a bordered empty frame with a title bar and footer; `q` and `Ctrl-C` exit cleanly
- [x] verify manually: launch, resize the terminal, quit, confirm the shell is intact; also verify `panic!()` in the loop still restores — (verified by inspection + TestBackend; no TTY available in the execution environment). Non-interactively confirmed: `--help` works, and with stdin/stdout not a TTY the binary exits 1 with "syno-clean needs an interactive TTY", writes nothing to stdout and changes no terminal state (raw mode is the first thing attempted and it fails closed)
- [x] no unit tests this task (terminal lifecycle — verified by running) — ➕ 19 non-terminal tests added anyway (see note below): `App` key handling and `TestBackend` frame rendering, neither of which needs a TTY

⚠️ **Decisions taken during Task 8** (plan text above kept verbatim; actuals recorded here):
- **The guard and the renderer live in `src/ui/mod.rs`, not `main.rs`.** The module layout in Technical Details files the terminal guard under `main.rs`, but `main.rs` is a thin shell over the library (Task 2) and nothing in a binary crate is reachable from tests. The guard, `restore()`, `install_panic_hook()` and `render()` are all in `ui`; `main.rs` keeps the event loop, which is what the architecture diagram actually calls "main event loop".
- **The event loop is `draw → await one event → apply`**, deliberately shaped so Task 11 replaces exactly one line. `next_terminal_event()` is a `spawn_blocking(event::read)` because crossterm's `event-stream` feature is unavailable through the `ratatui::crossterm` re-export (Task 1's ⚠️ note). Exactly one read is ever in flight — it is awaited immediately — so no blocking task lingers to stall runtime shutdown at quit.
- ➕ **Tests were added despite "no unit tests this task".** The terminal *lifecycle* genuinely is not testable and was not tested; but `App::handle_key` is a pure state machine and `render` is a pure function of `&App` that ratatui's `TestBackend` will draw into an in-memory `Buffer` **with no TTY**. 19 tests: quit keys (including `Ctrl-C` from every mode, and that a bare `c` does not quit), key *releases* ignored, resize absorbed, and frame rendering — every row exactly the terminal width at three sizes, a 1x1 terminal clips instead of panicking, the counts and empty states read correctly, and `render` leaves the `App` untouched.
- `App::handle_key` filters on `KeyEventKind::Press`: Windows and the kitty protocol report releases too, and acting on both halves would run every binding twice — a bug that is invisible on macOS and immediate for a Windows user.
- A key pressed in `Mode::Search`/`Confirm`/`Help` currently falls back to `Mode::Normal`. Nothing can enter those modes before Tasks 12/14/17, and this way a mode reached by accident can never trap the user with no way out.
- The **panic hook is not unit-tested**: a panic hook is process-global, so a test that installs one would swallow the output of any test panicking concurrently and be flaky under the default parallel harness. It is eight lines, chains to the previous hook and is `Once`-guarded — reviewable by inspection.
- `TerminalGuard::new` unwinds its own partial setup: if entering the alternate screen or constructing the `Terminal` fails, raw mode is disabled again before returning the error. Otherwise a half-failed startup hands back a raw-mode terminal with no program left to read keys.
- ➕ The body placeholder already distinguishes "No tasks" from "No tasks match the current filter" (`View::is_narrowed`, added in Task 7). Task 17 owns the polished empty states; this keeps the distinction from being forgotten.
- ➕ `CLAUDE.md` gained a "Terminal lifecycle" section recording these invariants.

### Task 9: Offline fixture mode, task table, and arrow navigation

**Files:**
- Create: `src/ui/table.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/app.rs`
- Modify: `src/cli.rs`

- [x] add a hidden `--fixture <path>` flag that loads a captured list response straight into `App` with no networking — **Tasks 9, 10 and 12 are otherwise unverifiable, since the poller does not exist until Task 11**; it also makes the 500+ task perf check in Task 21 trivial — `app::parse_fixture` / `App::from_fixture`, reusing `parse_envelope::<TaskList>`
- [x] render the task table with the columns from Technical Details, using `format.rs` helpers and per-status colour
- [x] add a header row indicating the active sort column and direction
- [x] implement column widths where Name absorbs slack and truncates (responsive column *dropping* is deferred past v1 — not required)
- [x] implement cursor movement (`↑`/`↓`/`k`/`j`/PgUp/PgDn/Home/End/`g`/`G`) with scroll offset clamped to the visible list
- [x] write tests for cursor movement clamping (empty list, single row, past-the-end, past-the-start)
- [x] run `cargo test` — must pass before task 10 — 224 tests pass

⚠️ **Decisions taken during Task 9** (plan text above kept verbatim; actuals recorded here):
- **`--fixture` short-circuits `main` before the config merge**, so it needs no config file, no `--host` and no password. Requiring credentials to look at a captured JSON file would defeat the flag's whole purpose (verified: `--fixture` on a machine with no config gets as far as the TTY check, while a bare invocation still exits on "no NAS host configured"). It is hidden like the dump flags but is **not** one of them — `Cli::is_dump()` stays false, since it enters the TUI rather than printing and exiting.
- ➕ `src/main.rs` is modified too (the plan lists only `cli`/`app`/`ui` files): the flag has to be dispatched from the startup path, and the event loop is where the page height is fed back to `App`.
- **The table is laid out by hand rather than with ratatui's `Table` widget.** The Name column must truncate at *display width* and every other cell must be padded to an exact cell count, or the first CJK title shears every column to its right; a widget that measures differently cannot be made to agree. Each row is one pre-composed `Line`, so the layout, truncation and padding all go through `format.rs`.
- **The scroll offset is derived, not stored**: `table::scroll_offset(cursor, rows, height)` is pure, so there is no second piece of state to fall out of step with a cursor that Task 11's refresh moved. The window is the smallest one containing the cursor and never scrolls past a full last page.
- ➕ `App` gained a private `page_size` (default 20) with `set_page_size`, pushed in by the event loop from `TerminalGuard::page_size()` after every draw, so `PageUp`/`PageDown` move a real screenful without `App` knowing anything about a terminal. `App::default()` is hand-written because a derived `0` would make the key silently dead.
- **Cursor movement clamps and never wraps.** Wrapping from the bottom of a long list to the top is how the wrong row gets deleted. `clamp_cursor()` is public because Tasks 11 and 12 both need it after the visible set shrinks.
- ➕ `App::cursor_task()` added here (rather than in Task 14) so "the row under the cursor" has one definition.
- Long DSM statuses are shortened in the Status column (`hash_checking` → `checking`, `filehosting_waiting` → `hosting`) so the column fits in 11 cells; an **unknown status is rendered verbatim** and coloured magenta, never renamed.
- The bordered body placeholder from Task 8 is gone: the table is full-bleed (the border cost two columns of an already-wide table) and the empty state is a centred message. Task 17 owns the polished wording.
- ➕ 45 tests added despite the terminal being "verified by running": column widths, the scroll-offset function, cell contents against the fixture, padding/truncation at every column, the sort marker, and three `TestBackend` frame tests. None needs a TTY.

### Task 10: Multi-select with spacebar

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/table.rs`

- [x] store selection as a `HashSet<String>` of **task IDs**, not row indices — the field already existed from Task 8; this task added `is_selected` / `selected_tasks` / `selected_count` / `selected_size` around it
- [x] implement `toggle_selection()` (Space), `toggle_select_all_visible()` (`a`), `clear_selection()` (Esc)
- [x] render a selection marker column and highlight selected rows distinctly from the cursor row — `table::SELECTED_MARKER` (`✓`) in the reserved column-0, and `table::row_style(selected, cursor)`: selection is a *colour* (bold yellow), the cursor a *reversal*, so all four combinations read differently
- [x] show selection count and the summed size of selected tasks in the footer — `ui::selection_summary`, prefixed to the status message/hints and omitted entirely when nothing is selected
- [x] write tests for toggle on/off, select-all over a *filtered* subset (must not touch hidden tasks), clear, and the size sum
- [x] run `cargo test` — must pass before task 11 — 240 tests pass

⚠️ **Decisions taken during Task 10** (plan text above kept verbatim; actuals recorded here):
- **`a` on a *partially* selected visible set selects the rest rather than clearing.** Only "every visible row is already selected" turns the key into a deselect. Selecting is the common intent, and a key that sometimes clears a half-built selection is the one that loses work.
- **`a` never touches a hidden task in either direction.** Selecting is confined to the visible IDs, and so is the deselect: a task selected before a filter was applied survives an `a`/`a` on the narrowed set. Two assertions in `select_all_never_touches_a_task_the_filter_hides` cover both halves.
- **`Esc` clears the *whole* set, hidden rows included** — the opposite of `a`, deliberately. `Esc` is the key a user reaches for when they are not sure what is armed, so leaving invisible selections behind would defeat it.
- **The footer counts and sums `selected_tasks()`** (selected IDs that still name a real task), not `selected.len()`. Task 11 prunes vanished IDs on refresh; until it runs, the raw length would over-report while the size sum did not, and the two disagreeing in the footer is worse than either being briefly low.
- `row_cells` and `row_line` gained a `selected: bool` parameter rather than taking `&App`: a `Task` does not know whether it is selected, and threading the flag keeps both functions assertable without constructing an app.
- The selection marker is asserted to be **exactly one cell wide** (`the_selection_marker_is_exactly_one_cell_wide`). A two-cell glyph in a one-cell column would shear every column to its right on selected rows only — the hardest layout bug of this table to spot by eye.
- ➕ 16 tests added (11 in `app`, 4 in `ui::table`, 2 in `ui`), and `every_cell_is_padded_to_exactly_its_column_width` now runs over both selected and unselected rows.
- Space deliberately does **not** advance the cursor after toggling. Vim-style "select and move" is convenient for runs of adjacent rows and wrong everywhere else; with `d` acting on the selection, a cursor that drifts is how the wrong row ends up under a later un-selected `d`.

### Task 11: Async poller and live refresh

**Files:**
- Create: `src/event.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`

- [x] define `AppEvent { Tasks(Vec<Task>), Error(String), OpProgress{..}, OpDone{..} }` and an mpsc channel (no `Tick` variant — the poller drives data and `EventStream` drives redraws) — plus `OpKind` (Delete/Pause/Resume) so Tasks 15/16 name their operations once
- [x] spawn the poller task on a `tokio::time::interval` of `refresh_secs`, sending `Tasks` or `Error` — `event::spawn_poller`, first tick immediate, `MissedTickBehavior::Delay`
- [x] merge crossterm's `EventStream` with the channel in a `tokio::select!` main loop — ⚠️ **`EventStream` is unavailable**; the select's terminal arm is the held `spawn_blocking(event::read)` `JoinHandle` (see the ⚠️ under Solution Overview)
- [x] implement `apply_tasks()`: reconcile new data while **preserving cursor position by task ID** and dropping selections for tasks that no longer exist; **ignore incoming `Tasks` events while `Mode::Confirm` is active** so a pending delete plan cannot go stale under the user
- [x] add `r` for manual refresh and a non-fatal error banner (poll failures must not kill the UI — it should recover on the next successful tick) — `App::request_refresh` + `event::RefreshHandle`; the banner is `App::error`, rendered red in the footer and cleared by the next successful tick
- [x] write tests for `apply_tasks` reconciliation: reordered list keeps the cursor on the same task, a removed task drops from the selection set, a removed cursor task clamps sanely, and an update is a no-op while in `Confirm` mode
- [x] run `cargo test` — must pass before task 12 — 264 tests pass

⚠️ **Decisions taken during Task 11** (plan text above kept verbatim; actuals recorded here):
- 🔺 **`EventStream` → `spawn_blocking(event::read)`** (the deviation Task 1 predicted and Task 8 already implemented). The architecture diagram above is updated. The *new* constraint this task adds is that the `JoinHandle` must live in a variable outside the loop: with a second `select!` arm that can now win, dropping the future per iteration would spawn an orphaned blocking stdin reader on every poller tick. The `select!` therefore yields a small `Next` enum instead of acting inside its branch bodies, so the mutable borrow of `pending_read` ends with the expression and the terminal arm can clear it.
- **`r` sets a flag; the event loop forwards it.** `App::request_refresh` / `take_refresh_request` keep `App` free of any tokio handle, so every key press stays a pure state transition and the whole keymap remains testable without a runtime. The loop pokes an `event::RefreshHandle` (an `Arc<Notify>`), which **coalesces** — leaning on `r` cannot queue one round trip per keystroke — and the poller `reset()`s its interval after a manual tick.
- **The error banner is its own field**, `App::error`, not a status message. It has different lifetime rules (cleared automatically by the next successful `Tasks` event, which is what "recovers on the next tick" means) and different styling (red, `⚠`, not dimmed). `status_message` survives underneath and comes back when the banner clears.
- **A refresh that cannot find the cursor's task holds the row number** rather than jumping to the top: the cursor stays where the user's eye is, then clamps into the new list. Two tests cover it — a removed *last* row clamps up, a removed middle row keeps its index.
- `apply_tasks` in `Mode::Confirm` returns **before** touching anything, including the error banner; the test asserts the whole `{app:?}` is byte-identical, so no field can be added later that quietly leaks through.
- ➕ The poller ends only when the channel closes or it is aborted. A failed poll is a `tracing::warn!` plus an `AppEvent::Error`, never a `return` — `main` also `abort()`s the handle after the loop so an in-flight 30-second HTTP timeout cannot delay process exit.
- ➕ Discovery and login moved into `main`'s live path **before** the alternate screen (they can prompt for a password or a 2FA code). `--fixture` still short-circuits above all of it and runs the same loop with a channel nothing ever sends on.
- ➕ 24 tests added (13 in `app`, 3 in `ui`, 5 in `event`, plus helpers). Nothing in them touches a network or a real timer: the poller is exercised only through its pure parts (the refresh handshake, the channel), and the reconciliation through `apply_tasks` directly.

### Task 12: Sort, filter, and search keybindings

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/mod.rs`

- [x] wire `s` (cycle sort column), `S` (reverse direction), `f` (cycle status filter) — `App::cycle_sort` / `toggle_sort_dir` / `cycle_filter`, all three through the private `App::change_view`
- [x] implement search mode: `/` enters it, characters/backspace edit the query, `Enter` applies, `Esc` restores the previous query — `App::begin_search` / `search_push` / `search_pop` / `commit_search` / `cancel_search` plus `App::handle_search_key`; matching is **live**, so `Enter` commits rather than applies
- [x] render the search input line and show the active sort/filter state in the status bar — `ui::search_bar` takes over the footer in `Mode::Search`; `ui::view_summary` adds `sort Name▲` (always) and `filter …` / `search "…"` (only while they hide rows)
- [x] ensure cursor and selection survive a filter change (cursor clamps into the new visible set; selection is untouched) — `change_view` follows the cursor's task by ID, else holds the row number, then clamps; the selection is never read or written
- [x] write tests for the search-mode state machine (enter, type, backspace, cancel restores prior query, apply commits)
- [x] run `cargo test` — must pass before task 13 — 290 tests pass

⚠️ **Decisions taken during Task 12** (plan text above kept verbatim; actuals recorded here):
- **Search matches on every keystroke, not on `Enter`.** The plan says "`Enter` applies"; with a live-narrowing table there is nothing left for it to apply, so it *commits* — it drops the backup that `Esc` would have restored. The visible behaviour of both keys is exactly as specified; what changed is that the table updates while typing, which is also what makes `Esc`'s restore meaningful.
- **`/` keeps the committed query rather than clearing it**, so a search can be refined (`/`, backspace, retype) instead of retyped from scratch. `Esc` still restores whatever it was on entry, so nothing is lost either way.
- ➕ **All five view mutations share one private `App::change_view`**, which reproduces `apply_tasks`' cursor rules: follow the task by **ID**, else hold the row number, then clamp. The plan only requires the cursor to clamp on a filter change, but a `s` that silently slides the cursor onto a different torrent is the same hazard `apply_tasks` was careful about, and one helper is cheaper than four copies of the rule. Selection is untouched by construction — `change_view` never reads it.
- **In `Mode::Search` every printable key is text.** `q` types a `q`; only `Enter`, `Esc`, `Backspace` and the global `Ctrl-C` are commands, and `Ctrl`/`Alt` chords are dropped rather than typed (`Shift` is not — it is how a capital arrives). Backspacing past the start of an empty query is inert rather than an exit: the key that widens a search must not occasionally cancel it.
- **`Esc` is dispatched per mode** — `cancel_search` in `Mode::Search`, `clear_selection` in `Mode::Normal` — and both halves have their own test so a later mode cannot quietly break one.
- **The search box takes over the footer** rather than claiming a fourth layout row. A dedicated row would shrink the table only while typing and put `ui::table_page_size` (which does not know the mode) out of step with the real body height. The sort/filter summary is displaced for those few seconds; the query itself is the state being edited.
- The caret is a **glyph** (`ui::SEARCH_CARET`), not the terminal cursor: `render` stays a pure function of `&App` that `TestBackend` can assert on, and the cursor stays hidden for the whole session instead of being shown and hidden per mode.
- `filter …` and `search "…"` appear in the footer **only when they are hiding rows** — a permanent `filter All` is noise, and the segment disappearing is precisely the feedback that `f` has wrapped back round to showing everything. The sort segment is always shown, since the header marker alone cannot say which way an off-screen column points.
- ➕ 26 tests added (21 in `app`, 5 in `ui`), and ➕ `CLAUDE.md` gained a "Sort, filter and search" section recording these invariants.

### Task 13: Delete-path resolution and safety guards

**Files:**
- Create: `src/delete.rs`

- [x] implement `common_root(&[TaskFile]) -> Option<String>` returning the shared top-level component of a torrent's file list
- [x] implement `normalize_destination(&str) -> String` stripping a leading `/volumeN` and trimming surrounding slashes
- [x] implement `resolve_delete_path(&Task) -> Result<String>` following the four-rule order in Technical Details — **crucially, refuse when the file list is non-empty but has no single common root**; fall back to `title` only when the file list is absent or empty
- [x] implement `validate_path(&str) -> Result<()>` rejecting: empty, `/`, fewer than two components, any `..` component, empty or `.` name component, missing leading slash — plus control characters and blank components (see the ⚠️ note below)
- [x] implement `DeletePlan { items: Vec<DeleteItem> }` as an owned snapshot, so unresolvable tasks surface as per-item skips rather than aborting the batch — `DeleteItem { id, title, size, status, target: Target::Path | Target::Refused }`
- [x] **write thorough resolution tests** — multi-file torrent resolves to its directory; single-file torrent resolves to the file; nested destination (`video/movies`); destination with leading/trailing slashes; absolute `/volume1/downloads` destination; title differing from the on-disk root; empty file list falls back to title
- [x] **write the critical refusal test** — a file list whose entries share no common root is REFUSED, not resolved via `title` — `a_file_list_with_no_common_root_is_refused_and_never_guessed_from_the_title`, driven by the pre-staged fixture task `dbid_013`
- [x] **write thorough guard tests** — every rejection case above, including a `""` destination (would yield `/name`) and a `..` traversal attempt
- [x] run `cargo test` — must pass before task 14 — 346 tests pass (56 new in `delete`)

⚠️ **Decisions taken during Task 13** (plan text above kept verbatim; actuals recorded here):
- ➕ **Two guards the plan did not enumerate**, both because they turn a merely-wrong path into a *share-destroying* one if anything downstream normalizes it. **Control characters are rejected anywhere in the path**: a NUL truncates the string in any C-based consumer, so `/downloads\0/Some.Torrent` arrives as `/downloads` — the share root, deleted recursively. **Whitespace-only components are rejected** as well as empty ones: if any layer trims, `/   /Some.Torrent` collapses to `/Some.Torrent`, again a share root. Incidental leading/trailing spaces *inside* an otherwise real name are deliberately left alone — those are legitimate on the NAS filesystem, and refusing them would skip real torrents for a hazard that needs server-side trimming to exist at all.
- **No glob guard.** Rejecting `*`/`?`/`[`/`]` was considered and rejected: File Station's `path` parameter is a literal path (searching is a separate API), while scene release names contain brackets constantly — the guard would refuse most real torrents to defend against a behaviour DSM does not have.
- ➕ **The on-disk name is guarded separately (`validate_name`) before it is joined.** `validate_path` would catch most of it afterwards, but a `title` fallback of `Some/Release` passes every path guard while pointing one level *deeper* than the task's own directory, at something that may belong to someone else. The name must be a single non-blank component that is not `.`/`..` and holds no control characters.
- **`common_root` compares components exactly** — no case folding. The NAS filesystem is case-sensitive, so `Some.Release/` and `some.release/` are two directories and picking either is a guess.
- **An entry with an empty or absolute `filename` makes the whole list unresolvable**, rather than being skipped. Splitting `/volume1/downloads/X/a.mkv` naively would report `volume1` (or an empty component) as the shared root; refusing the task is the fail-closed direction and costs one skipped row.
- **A deselected file still counts towards the common root.** `selected` describes what was downloaded, not what is on disk, and a list that disagrees with itself must refuse either way. Filtering to selected entries would have *resolved* some of the ambiguous cases the plan's rule 2 exists to refuse.
- **Only the absolute `/volumeN` form is stripped** by `normalize_destination`; a relative `volume1/downloads` is passed through untouched, since a share may legally be named `volume1` and mangling a relative path is how a delete lands one directory away from where it was aimed. Unrecognized forms (`/volumeUSB1/…`) are likewise passed through — the resulting path simply fails the executor's existence check and is skipped.
- **An empty normalized destination is refused with its own reason** rather than being left to the component-count guard: `/{name}` names a *share*, and "the task reports no destination" is the message the user can act on. Fixture tasks `dbid_010` (no `additional` at all) and `dbid_011` (a `file` block but no `detail`) are both refused for this.
- `Target::Refused(reason)` carries the `Error::UnsafePath` *reason* only, not the full `Display`, so Task 14's dialog is not repeating the path back inside every skip line.
- ➕ 56 tests, none of which touch a network. Beyond the plan's list: a snapshot test that mutates and then clears the source `Vec` and asserts the plan is byte-identical (this is what "owned snapshot" has to mean), a whole-fixture sweep asserting every resolvable task's path re-passes `validate_path`, and an exact assertion that the fixture refuses precisely `dbid_010`, `dbid_011`, `dbid_013` — so a later change that makes one of them resolve cannot pass unnoticed.

### Task 14: Delete confirmation dialog

**Files:**
- Create: `src/ui/dialog.rs`
- Modify: `src/app.rs`
- Modify: `src/ui/mod.rs`

- [x] implement `build_confirmation(&DeletePlan) -> ConfirmSummary` listing each title, size, resolved path, the total count and total bytes freed, and any unresolvable items flagged as skipped with the reason — signature is `build_confirmation(&DeletePlan, DeleteOptions)` (see note below)
- [x] render a centred modal over the table showing that summary, scrollable when the list exceeds the modal height — `dialog::render_confirm`, drawn last over the whole frame behind a `Clear`
- [x] state whether files will be deleted or only the DSM task (per `delete_files` / `--no-delete-files`), and label the modal clearly when `--dry-run` is active — in the border title *and* the effect line, plus a yellow (dry run) / red (real) border
- [x] wire `d` to build the snapshot and open the modal (falling back to the cursor row when nothing is selected); `y`/`Enter` confirms, `n`/`Esc`/`q` cancels — **cancel must be the default focus** — 🔺 `Enter` presses the *focused* button, which starts on Cancel; `y` is the unconditional confirm (see the deviation note below)
- [x] write tests for `build_confirmation`: total size sum, unresolvable items reported as skipped and excluded from the total, an **empty task list** produces no dialog (an empty *selection* still opens one for the cursor row)
- [x] run `cargo test` — must pass before task 15 — 386 tests pass (40 new: 13 in `ui::dialog`, 19 in `app`, 8 in `ui`)

⚠️ **Decisions taken during Task 14** (plan text above kept verbatim; actuals recorded here):
- 🔺 **`Enter` activates the focused button rather than always confirming.** The two halves of the plan's bullet contradict each other: a dialog whose default focus is Cancel cannot also delete on `Enter`. Resolved toward safety, since this is the keystroke that loses data — `ConfirmFocus` defaults to `Cancel`, `Tab`/`←`/`→`/`h`/`l` move it, and `Enter` presses whichever is focused. `y` still confirms in one key from either focus, so the plan's "y confirms" is unaffected; what changed is that a reflexive `Enter` on a modal that just appeared cancels instead of deleting. Covered by `the_dialog_opens_with_cancel_focused` and `enter_confirms_only_after_the_focus_is_moved_to_delete`.
- **`build_confirmation` takes `DeleteOptions` as a second parameter.** `delete_files` and `dry_run` are session state, not per-task state, and the modal cannot state what it will do without them. Folding them into `DeletePlan` was the alternative and was rejected: the plan is a *snapshot of which tasks*, its `snapshot()` signature and equality are already covered by Task 13's tests, and two things with different lifetimes in one struct is how one of them goes stale. ➕ `delete::DeleteOptions` (with `from_config` / `dry_run`) is the single definition, held on `App::delete_options`.
- **The dialog performs no I/O whatsoever.** `confirm_delete` parks the snapshot in `confirmed_delete`; the event loop drains it with `take_confirmed_delete`, mirroring the `r` refresh handshake from Task 11. `main::spawn_delete` currently only logs — Task 15 replaces that function body and nothing else. This keeps the entire confirmation flow testable with no runtime, no client and no NAS.
- ➕ **`--fixture` mode forces `DeleteOptions::dry_run()`.** There is no client in offline mode, so a modal promising a real recursive delete would be describing something that cannot happen. The item list, the totals and the skip lines all still render exactly as they do live.
- **Refused items keep their place in snapshot order** rather than being grouped at the end, so a row in the dialog maps to the row the user selected (the ordering Task 13's `DeleteItem` comment already promised). They render as `SKIPPED <title>` + the reason in yellow, carry **no size** — a number beside a skip reads as bytes about to be freed — and are excluded from the total.
- **The totals line changes wording with `delete_files`**: `5.8 GiB to free` when the files go, `5.8 GiB left on disk` when only the task does. Reporting "to free" for a task-only delete would be the single most misleading number the program could print.
- The modal body scroll is **clamped twice**: in `App` against the line count (so a held `j` cannot run the offset off into the distance) and again in `render_confirm` against the modal's height, which is a property of the frame rather than of the state — the same split as `table::scroll_offset`.
- ➕ `NORMAL_HINTS` now leads with `d delete`, since the key does something as of this task.
- ➕ 40 tests, none needing a TTY: the summary's arithmetic and wording in `ui::dialog`, the whole key state machine in `app` (cursor fallback, stale-selection fallback, every cancel key, `q` not quitting, an unbound key changing *nothing*, focus reset on reopen, scroll clamping), and `TestBackend` frame assertions in `ui` (the modal appears over the table, a refusal is visible with its reason, the dry-run and task-only wording reach the screen, and no row overflows at five terminal sizes).

### Task 15: Delete execution — pause, files, then task

**Files:**
- Create: `src/api/file_station.rs`
- Modify: `src/api/download_station.rs`
- Modify: `src/delete.rs`
- Modify: `src/event.rs`

- [x] implement pure `build_fs_path_params(&[String])` (JSON-array encoding) and `build_ds_id_params(&[String])` (comma-separated) so parameter construction is testable without HTTP
- [x] implement `file_station::path_info(path)` via `SYNO.FileStation.List` `getinfo` for the pre-delete existence check, and `file_station::delete_paths(paths)` via `method=start` then polling `method=status` on the returned `taskid` with a bounded overall timeout — `DELETE_TIMEOUT` 300 s, `DELETE_POLL_INTERVAL` 500 ms
- [x] implement `download_station::delete_tasks(ids)` with `force_complete=false`
- [x] implement pure `plan_delete_ops(item, status) -> Vec<Op>` encoding the status-dependent ordering table (pause first for active tasks; skip later phases on any failure) — 🔺 signature is `plan_delete_ops(&DeleteItem, DeleteOptions)` (see note below); the cancellation half is the pure `ops_cancelled_by`
- [x] implement the delete op task driving those ops: re-validate the path, existence-check (not found ⇒ skip file phase, still delete the task), pause if active, delete files, delete task; honour `--dry-run` by logging intended operations and issuing no destructive call — `event::spawn_delete` / `OpContext`, wired into `main::run_tui` in place of the log-only hook
- [x] report per-item outcomes via `OpProgress`/`OpDone`; render successes, skips and failures distinctly in the status bar and trigger an immediate refresh afterwards — `app::op_summary`, one refresh per batch
- [x] write tests for both param builders (encoding differences between the two APIs)
- [x] write tests for `plan_delete_ops`: an active task gets a pause op first, a paused task does not, a file-delete failure leaves the task-delete op unissued, a pause failure leaves both unissued
- [x] run `cargo test` — must pass before task 16 — 436 tests pass (50 new), none touching a network or a real timer

⚠️ **Decisions taken during Task 15** (plan text above kept verbatim; actuals recorded here):
- 🔺 **`plan_delete_ops(&DeleteItem, DeleteOptions)`, not `(item, status)`.** `DeleteItem` already carries the snapshot-time status, so a separate `status` parameter would be a second source of truth for the value the ordering keys on; what the function genuinely cannot derive is `delete_files`/`dry_run`, which are session state. Same shape and same reasoning as Task 14's `build_confirmation(&DeletePlan, DeleteOptions)`.
- ➕ **`ops_cancelled_by(ops, failed_at)`** carries the other half of the ordering rule ("a failed phase cancels every later phase") as a pure function, so the plan's two failure-ordering tests need no HTTP. The executor uses it for the log line naming what it just cancelled.
- **`delete_files = false` drops the pause as well as the file phase.** The pause exists only to keep Download Station out of the way of the recursive delete; with no recursive delete there is nothing to keep it out of, and a pause that failed would otherwise block a task-only removal for no reason at all.
- **The two statuses the plan's table does not name are treated as active.** The table lists nine of `TaskStatus`'s ten variants; `filehosting_waiting` and `Unknown(_)` fall through it. `requires_pause` is therefore written as "everything except Paused/Finished/Error" — pausing an idle task costs one round trip, while failing to pause a live one risks DS writing into the directory mid-delete. Its cost is that an unrecognized status whose pause DSM rejects can never be deleted by this tool, which is the fail-closed direction.
- **A refused item gets an empty op list — the DSM task is not deleted either.** The dialog showed the row as SKIPPED, and removing the task would orphan precisely the data whose location is in doubt. Holds under `--no-delete-files` too.
- ⚠️ **`delete` / `pause` / `resume` report failure per task, not in the envelope** (`{"success": true, "data": [{"id": …, "error": 544}]}`). ➕ `TaskOpResult` + `check_task_results` turn a non-zero per-item code into an `Error::Dsm`; reading only `success` would report a failed delete as a success, and the ordering depends on knowing a pause actually happened.
- ➕ **"Confirm paused" is a real re-read**, not the pause call's return: `download_station::task_info` (`getinfo`) is polled until `requires_pause` is false, bounded at 15 s. DSM accepting a `pause` says the request was queued, not that the task stopped writing. ➕ `pause_tasks` therefore lands here rather than in Task 16, which still owns `resume_tasks` and the `p`/`u` keys.
- ➕ **`PathInfo` has three answers, not two**: `Missing`, `Found`, and `Error(code)`. Collapsing a permission failure into "not found" would delete the task and strand the files — the exact orphaning the ordering exists to prevent. `Missing` is reported per entry (`code: 408`), as a bare entry with no `isdir`, or at the envelope level depending on the DSM build; `classify_getinfo` is pure and covers all three.
- **`--dry-run` issues no call whatsoever, including the `getinfo` existence check** — a read, but still a round trip the user did not ask for. Dry-run items are counted as **skipped**, never as successes, so the footer cannot read "3 succeeded" for a run that deleted nothing.
- **A File Station failure with no DSM code behind it reuses `Error::Io`** (a `status` poll reporting `path_err_num > 0`, or the bounded wait expiring), the same way Task 4 reuses `Error::Parse` for protocol violations. The plan's error-variant list is unchanged.
- **`finished: true` is not on its own success**: `classify_delete_status` fails the phase when `path_err_num > 0`, which is how DSM reports "the delete task completed and did not delete some of it".
- ➕ **`event::OpContext { client, tx, refresh }`** is the handle every op task takes, and the op reports through the same channel as the poller. The batch pokes `refresh` **once at the end**, not per item — twenty deletes must not be twenty full task-list round trips. `main::run_tui` takes it as `Option<&OpContext>`; `--fixture` passes `None`, which is also why that mode forces `DeleteOptions::dry_run()`.
- **The op report goes in `status_message`, not the error banner**, even when items failed: the batch's own refresh clears the banner, so the report would vanish about a second after appearing. `app::op_summary` names only the non-zero categories and prefixes `⚠` when anything failed.
- ➕ 50 tests: both param builders including a cross-API test pinning the JSON-array/comma-separated difference and a path containing a comma (the reason one builder could not serve both), every `classify_getinfo` and `classify_delete_status` shape, the per-task result array, the full ordering table over all nine named statuses plus the two unnamed ones, the two cancellation cases, and the footer wording. None touches a network or a real timer — the suite still finishes in well under a second.

### Task 16: Pause and resume

**Files:**
- Modify: `src/api/download_station.rs`
- Modify: `src/app.rs`
- Modify: `src/event.rs`

- [x] implement `pause_tasks(ids)` / `resume_tasks(ids)` reusing `build_ds_id_params` — `pause_tasks` landed in Task 15 (the delete ordering needed it); this task added `resume_tasks` as its exact mirror
- [x] wire `p` and `u` to run them as op tasks over the selection (or the cursor row) — `App::pause_target` / `resume_target` park an `app::TaskOpRequest`, `event::spawn_task_op` runs it off the loop through `OpContext`
- [x] report per-item results in the status bar and refresh immediately after completion — per-item `OpProgress`, one `OpDone` through `app::op_summary`, then a single `refresh.request()` for the batch
- [x] write a test for target-ID selection (selection when non-empty, else cursor row, empty list is a no-op) — plus a hidden-but-selected task, a stale selection, take-once, and `p`/`u` being Normal-mode-only
- [x] run `cargo test` — must pass before task 17 — 452 tests pass (16 new), none touching a network or a real timer

⚠️ **Decisions taken during Task 16** (plan text above kept verbatim; actuals recorded here):
- ➕ **`App::target_tasks` is now the single definition of "what the current key acts on"** — the selection when non-empty, the cursor row otherwise, nothing when the table is empty. `d`'s `delete_target` was rewritten on top of it rather than `p`/`u` getting a second copy of the rule: three keys that disagreed about the target is how a user who armed a selection ends up pausing whatever the cursor was resting on. A selected task the filter is hiding is included, exactly as it already was for `d`.
- **Neither key is confirmed.** `d` gets a modal because it destroys data; pause and resume are each undone by the other key, and a modal in front of a reversible operation only trains the user to dismiss modals.
- **One round trip for the whole batch, not one per task.** Download Station takes the comma-separated id list and answers with a result *per task*, so per-item outcomes come from ➕ the pure `event::task_op_outcome`, which runs each entry through Task 15's `check_task_results` (`{"success": true, "data": [{"error": 544}]}` is still the trap). The delete executor stays per item because its *ordering* is per item.
- **An id DSM returned no result for is reported as a failure**, not a success. The refresh that follows shows what really happened either way, and a false "3 paused" is the answer the user cannot correct.
- 🔺 **`--dry-run` suppresses pause and resume too**, reporting every item as *skipped*. The plan only ever discusses dry-run in terms of the destructive delete, but a flag that promises the NAS is untouched and then pauses somebody's whole download list is a trap; `spawn_task_op` therefore takes a plain `dry_run: bool` (not `DeleteOptions`, which is delete-specific state).
- ➕ **`ItemOutcome::Deleted` became `ItemOutcome::Done(&'static str)`** carrying ➕ `OpKind::past_tense()` ("deleted"/"paused"/"resumed"), so one outcome enum and one footer wording serve all three operations instead of the delete executor and the new one drifting apart.
- `spawn_task_op` given `OpKind::Delete` logs an error and does nothing rather than panicking — the three-phase ordering belongs to `spawn_delete`, and a panic in an op task would take the terminal down with it.
- ➕ `NORMAL_HINTS` now names `p/u pause/resume`; the footer test that asserts on the hint text renders 90 cells wide instead of 60 to fit it (the footer is clipped, never wrapped).
- ➕ 16 tests: the target rule in every shape the plan asks for, the per-result outcome mapping (success, a per-task code inside a success envelope, a missing entry), the dry-run accounting and an empty batch making no call. `CLAUDE.md` gained a "Pause and resume" section.

### Task 17: Help overlay and first-run experience

**Files:**
- Modify: `src/ui/dialog.rs`
- Modify: `src/app.rs`
- Modify: `src/config.rs`

- [x] implement the `?` help overlay listing every keybinding, dismissed with any key — `dialog::HELP_SECTIONS` (data) + `render_help`, two columns, `App::show_help` / `close_help`
- [x] when required values are unresolved after the merge (per Task 3), write a commented config template to the config path and print an actionable message — do **not** enter the TUI; a merely-missing config file with sufficient CLI/env values must still run — `config::missing_required` + `CONFIG_TEMPLATE` + `write_config_template`, dispatched by `main::first_run`
- [x] make connection/auth failures print a clear diagnostic (host, port, DSM error meaning) and exit non-zero rather than showing an empty table — `error::connection_diagnostic` / `connection_hint`, applied to discovery and login in `main::startup_failure`
- [x] show a helpful empty state when there are zero tasks, and a distinct one when a filter/search hides everything — `ui::empty_state` keyed on `App::tasks.is_empty()`, plus `ui::narrowing_summary`
- [x] write a test for config-template generation (the produced template round-trips through the parser) — parses as written (an empty layer) **and** parses with every key uncommented, plus a check that it documents all of `KNOWN_KEYS`
- [x] run `cargo test` — must pass before task 18 — 479 tests pass (27 new)

⚠️ **Decisions taken during Task 17** (plan text above kept verbatim; actuals recorded here):
- ➕ **Three files beyond the plan's list**: `src/main.rs` (the first-run dispatch and the startup diagnostic both live in the startup path), `src/ui/mod.rs` (the empty states and the overlay's draw call), and `src/error.rs` (the diagnostic belongs with the DSM code table it renders).
- **"Required values unresolved" is asked with the same resolution `merge` enforces, not a second copy.** `merge`'s host/username resolution was factored into `resolved_host`/`resolved_username`, and ➕ `config::missing_required` reports on those; a test asserts `missing_required(..).any()` and `merge(..).is_err()` never disagree. `main` asks *before* merging so it can write a template instead of surfacing an error, and `merge` still validates for every other caller. **No error-enum variant was added** — same restraint as Tasks 2 and 4.
- **The template has every key commented out.** A starter config shipping a live `host = "nas.local"` would send the next invocation at a host the user never named. It therefore parses as an *empty* layer, which is what one test asserts; a second uncomments every example line and asserts the result is a full, unknown-key-free `Config`, so a typo in a key name cannot ship.
- **An existing config file is never overwritten** (`create_new`), and a template that could not be written appends its own error rather than replacing the message about what is actually missing.
- 🔺 **`Enter` on the confirmation and in the search box are documented as implemented, not as the plan's Keybindings table describes** (Tasks 14 and 12 already deviated; the overlay is what users read). The table's `Esc`/`d`/`p`/`u`/`s`/`S`/`f`/`/`/`r`/`?`/`q` all match. The overlay additionally documents keys the table never listed: `Tab`/`←`/`→`/`h`/`l` focus switching, `y`/`n` in the modal, `Backspace` in the search box and `Ctrl-C`.
- **Any key closes the help and does nothing else.** Dismissing with `d` must not also open a delete confirmation — the screen that exists to remove surprises cannot be the one that causes one. `?` is Normal-mode only: in the search box it is a character, in the confirmation it is unbound.
- **The overlay drops its inter-section blank lines rather than clipping** when the terminal is short, and `split_columns` minimizes the *taller* column rather than the difference between them. Both exist so the whole card fits **80x24**, which two tests pin.
- **The empty state is chosen by `tasks.is_empty()`, not `View::is_narrowed()`.** With zero tasks and a filter set both are true, and blaming the filter sends the user pressing `f` at a NAS that has nothing to show. The narrowed state names how many rows are hidden and by what (`filter Error and search "zzz"`), so the fix is on screen rather than guessed at.
- **Startup failures exit non-zero with a three-line diagnostic** (what failed and where, the error including the DSM code's meaning, one hint) and never reach the TUI; a poll failure *during* a session stays the non-fatal banner from Task 11. `main::authenticate` now returns `error::Result` so the same diagnostic covers discovery, login and the `--dump-*` modes.
- ➕ 27 tests, none needing a TTY or a network: the template round-trip and the missing-required/merge agreement in `config`, the diagnostic's shape and every auth-code hint in `error`, the help data and layout in `ui::dialog` (including the cross-check that every bound key is documented), the `?` state machine in `app`, and `TestBackend` frames for both empty states and the overlay at six terminal sizes. `CLAUDE.md` gained a "Help overlay and first run" section.

### Task 18: Open-source scaffolding

**Files:**
- Create: `LICENSE`
- Create: `README.md`
- Create: `CONTRIBUTING.md`
- Create: `CHANGELOG.md`
- Create: `.github/ISSUE_TEMPLATE/bug_report.md`
- Create: `.github/ISSUE_TEMPLATE/feature_request.md`
- Create: `.github/pull_request_template.md`
- Modify: `Cargo.toml`

- [ ] add the MIT `LICENSE` and complete `Cargo.toml` metadata (description, license, repository, keywords, categories, readme)
- [ ] write `README.md`: what it does, a screenshot placeholder, install (from source + release binary), DSM requirements (DSM 7, Download Station and File Station installed), config reference with the **actual XDG paths**, env var table, keybinding table, and a prominent warning that `d` deletes files irreversibly
- [ ] document the delete safety model in the README — refuses ambiguous paths, existence-checks before deleting, `--dry-run` and `--no-delete-files` escape hatches
- [ ] write `CONTRIBUTING.md`: toolchain setup, `cargo fmt`/`clippy`/`test` expectations, the deliberately narrow testing philosophy, and how to test offline with `--fixture` versus against a real NAS
- [ ] write `CHANGELOG.md` seeded with an Unreleased → 0.1.0 entry (Keep a Changelog format)
- [ ] add issue and PR templates
- [ ] no unit tests this task (documentation)

### Task 19: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] add a workflow on push and pull_request running on `ubuntu-latest` and `macos-latest`
- [ ] steps: checkout, install the toolchain from `rust-toolchain.toml` with rustfmt+clippy, cache cargo registry and target
- [ ] run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`
- [ ] verify the workflow passes locally by running the same three commands
- [ ] no unit tests this task (CI config)

### Task 20: Release automation

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] add a workflow triggered on `v*` tags
- [ ] cross-build a release matrix: `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`
- [ ] **set up the aarch64 Linux cross toolchain** — either use `cross`, or install `gcc-aarch64-linux-gnu` and add the linker to `.cargo/config.toml`; the build fails without it
- [ ] strip binaries, package each as `syno-clean-<version>-<target>.tar.gz`, generate SHA256 checksums
- [ ] create the GitHub release and attach all artifacts plus the matching CHANGELOG section
- [ ] validate the workflow syntax (`actionlint`) before tagging
- [ ] no unit tests this task (CI config)

### Task 21: Verify acceptance criteria

- [ ] **if the Task 5 fixture is still marked PROVISIONAL, re-capture it from the real NAS now** with `--dump-tasks-json` and re-run the parser tests against the real response; also confirm the discovered API versions with `--dump-api-info`
- [ ] verify every requirement from Overview: table with full stats, arrow navigation, spacebar multi-select, `d` → confirmation listing selections → deletes both task and files, sort options, status filters, live refresh, pause/resume
- [ ] verify edge cases: zero tasks, a single task, 500+ tasks via `--fixture` (scrolling and render performance), very long CJK/emoji titles (column alignment holds), a task with zero size, a paused task with no speed, network drop mid-session (banner appears, recovers on reconnect), a task whose files were already removed manually (skipped cleanly, task still deleted)
- [ ] verify the safety path end to end: `--dry-run` issues no destructive calls; a file list with no common root is skipped rather than guessed at; `--no-delete-files` leaves files intact; deleting an actively seeding task pauses it first
- [ ] run the full suite: `cargo test --all`
- [ ] run `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`
- [ ] confirm every module listed under Testing Strategy as "tested" actually has tests

### Task 22: [Final] Update documentation

- [ ] update `README.md` with a real screenshot/asciinema recording of the working TUI
- [ ] finish `CLAUDE.md` (stubbed in Task 1): module layout, CLI/env/file precedence, the three-phase delete ordering, and the path-safety invariants
- [ ] finalize the `CHANGELOG.md` 0.1.0 entry
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Manual verification:**
- Test against a real DSM 7 NAS with a mix of active, seeding, finished, paused, and errored tasks. **Delete something disposable first** and confirm on the NAS via File Station that both the task and the files are actually gone.
- **Check whether deleted data lands in the share's `#recycle` folder.** If Recycle Bin is enabled on that share, "deleted" may reclaim no space at all — which would silently defeat the tool's entire purpose. If it does, document it in the README and consider surfacing a warning.
- Verify behaviour with a DSM account that has Download Station access but *not* File Station permission — the error must be clear, and the task must not be deleted when its files could not be.
- Verify the active-task path: delete a seeding torrent and confirm no files are left behind and Download Station does not re-create the directory.
- Test 2FA login end to end, and a self-signed certificate with and without `--insecure`.
- Confirm session caching works: a second invocation should start noticeably faster, should recover transparently after the cached sid expires, and should not thrash when alternating between two hosts.
- Sanity-check rendering over SSH and in a few terminals (Ghostty/iTerm2/Terminal.app, and a Linux terminal).

**External system updates:**
- Create the public GitHub repository, push, and enable Actions.
- Add repository topics (`synology`, `download-station`, `tui`, `rust`, `ratatui`) and a description.
- Cut the `v0.1.0` tag once CI is green, then verify the release workflow produced working binaries on both macOS and Linux.
- Optional later: crates.io publish, a Homebrew tap, and migrating to `SYNO.DownloadStation2.Task` — all deliberately excluded from v1.
