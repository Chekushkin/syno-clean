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
                 ┌────────────────────────────────┐
  crossterm      │        main event loop         │
  EventStream ──▶│   tokio::select! {             │
                 │     terminal event  → App      │──▶ ratatui render
  poller task ──▶│     AppEvent::Tasks → App      │
  (interval)     │     AppEvent::OpDone→ App      │
                 │   }                            │
  op tasks    ──▶└────────────────────────────────┘
  (delete/pause)          mpsc::Sender<AppEvent>
```

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

- [ ] install rustup + stable toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`), confirm `cargo --version`
- [ ] run `cargo init --name syno-clean` **first** (it initializes git and writes its own `.gitignore`), then append `*.log` and `.DS_Store`; set the `edition` in `Cargo.toml` explicitly rather than relying on the cargo default
- [ ] add dependencies: `ratatui` (with its `crossterm` feature — **do not add crossterm as a separate dependency**; use `ratatui::crossterm` everywhere to avoid version-skew type errors), `tokio` (`rt-multi-thread`,`macros`,`sync`,`time`), `reqwest` (`json`,`rustls-tls`, `default-features = false`), `serde` (`derive`), `serde_json`, `toml`, `clap` (`derive`), `thiserror`, `anyhow`, `etcetera`, `unicode-width`, `rpassword`, `tracing`, `tracing-subscriber`, `tracing-appender`, `futures`
- [ ] pin `rust-toolchain.toml` to the **explicit installed version** (e.g. `channel = "1.xx.y"`, not `"stable"`, so CI is reproducible), components `rustfmt` + `clippy`
- [ ] create a `CLAUDE.md` stub recording the conventions this plan establishes (module layout, config precedence, delete ordering, path-safety invariants) — filled in as they land, not written from scratch at the end
- [ ] verify `cargo build` and `cargo clippy --all-targets -- -D warnings` are clean
- [ ] no tests this task (scaffolding only)

### Task 2: Error type and DSM error-code mapping

**Files:**
- Create: `src/error.rs`
- Modify: `src/main.rs`

- [ ] define `Error` enum with `thiserror`: `Http`, `Dsm { code, api }`, `Config`, `Io`, `Parse`, `Auth`, `UnsafePath`, `ApiUnavailable`
- [ ] implement `dsm_message(code, api) -> &'static str` for common codes: 100 unknown error, 101 invalid parameter, 102 API does not exist, 103 method does not exist, 105 insufficient user privilege, 106 session timeout, 107 session interrupted by duplicate login, 119 invalid SID
- [ ] implement the **auth** code table correctly — 400 no such account or incorrect password, **401 account disabled**, **402 permission denied**, 403 2-step verification code required, 404 failed to authenticate 2-step verification code, 406 enforce 2-step verification, 407 blocked IP source, 408 expired password (cannot change), 409 expired password, 410 password must be changed
- [ ] add `Result<T>` alias; wire `error` module into `main.rs`
- [ ] write tests for `dsm_message` — known codes map to specific text (assert 400/401/402 individually, since these are the ones most easily transposed), unknown codes fall back to a generic message including the numeric code
- [ ] run `cargo test` — must pass before task 3

### Task 3: Config file, CLI flags, env overrides, and validation

**Files:**
- Create: `src/config.rs`
- Create: `src/cli.rs`
- Modify: `src/main.rs`

- [ ] define `Cli` with clap derive: `--config`, `--host`, `--user`, `--port`, `--insecure`, `--refresh-secs`, `--no-delete-files`, `--log-file`, `--dry-run`, `--logout`, `--version`
- [ ] define `Config` with serde defaults and `load(path)` reading TOML from the XDG config dir via `etcetera`; unknown keys warn and are ignored (**no** `deny_unknown_fields` — an older binary must tolerate a newer config)
- [ ] implement `apply_env(&mut Config)` for `SYNO_CLEAN_{HOST,PORT,HTTPS,INSECURE,USERNAME,PASSWORD,OTP,REFRESH_SECS}`
- [ ] implement `merge(config, env, cli) -> Result<ResolvedConfig>` enforcing CLI > env > file > default **and validating that `host` and `username` are present**, so later modules can assume them; plus `resolve_password()` (env, else `rpassword` prompt)
- [ ] implement sid cache read/write at the XDG cache dir with `0600` permissions, **keyed by `{host}:{port}/{username}`** so two NASes or accounts do not evict each other
- [ ] initialize `tracing` to the log file (never stdout) and **hold the `tracing_appender::WorkerGuard` for the process lifetime** — dropping it early silently discards buffered log lines
- [ ] write tests for precedence merge (CLI beats env beats file beats default), for validation failing when host/username are unresolved, and for TOML parsing (full file, minimal file, unknown key ignored not rejected)
- [ ] run `cargo test` — must pass before task 4

### Task 4: HTTP client, API discovery, and authentication

**Files:**
- Create: `src/api/mod.rs`
- Create: `src/api/client.rs`
- Create: `src/api/auth.rs`

- [ ] define `Envelope<T> { success, data: Option<T>, error: Option<DsmError> }` and a `parse_envelope` helper turning `success=false` into `Error::Dsm`
- [ ] build `SynoClient`: base URL from host/port/https, `danger_accept_invalid_certs` when `insecure`, sane timeouts, shared `reqwest::Client`
- [ ] implement `query_api_info()` against **`/webapi/query.cgi`** (hardcoded — `SYNO.API.Info` is not served from `entry.cgi`), caching each API's `path`, `minVersion`, `maxVersion`; implement `pick_version(api, supported_range)` used by every later call so **no version is hardcoded**; error clearly when a required API is absent
- [ ] implement `login()` / `logout()` against `SYNO.API.Auth` with `session=DownloadStation`, `format=sid`, optional `otp_code`; logout is invoked **only** by `--logout`, never on normal quit
- [ ] implement the request helper: build the URL from the discovered path, attach `_sid`, and on DSM **106/107/119** re-login once and retry exactly once
- [ ] write tests for envelope parsing (success payload, error payload, malformed body), for the API-info map lookup (present / missing API), and for `pick_version` (clamps to `maxVersion`, errors when the NAS range and the supported range do not overlap)
- [ ] run `cargo test` — must pass before task 5

### Task 5: Task model and Download Station list endpoint

**Files:**
- Create: `src/model.rs`
- Create: `src/api/download_station.rs`
- Create: `tests/fixtures/task_list.json`
- Modify: `src/cli.rs`

- [ ] add hidden `--dump-api-info` and `--dump-tasks-json` flags that log in, call the respective endpoint, print raw JSON, and exit
- [ ] capture `tests/fixtures/task_list.json` from the real NAS with `--dump-tasks-json`. **If the NAS is unreachable, do not stall**: hand-write a provisional fixture from the documented v1 response shape, mark it at the top of the file as `⚠️ PROVISIONAL — not captured from a real NAS`, proceed with the remaining tasks, and re-capture in Task 21
- [ ] define `TaskStatus` enum with `from_dsm_str` covering the v1 **string** statuses: `waiting`, `downloading`, `paused`, `finishing`, `finished`, `hash_checking`, `seeding`, `filehosting_waiting`, `extracting`, `error` — plus an `Unknown(String)` catch-all so an unrecognized status never panics or drops the row
- [ ] define `TaskFile { filename, size, priority, selected }` and `Task { id, title, status, size, downloaded, uploaded, download_speed, upload_speed, destination, files: Vec<TaskFile>, seeders, leechers, create_time }` with derived `progress()` / `ratio()` / `eta()`
- [ ] implement `build_list_params()` (pure, returns `Vec<(&str, String)>`) and `list_tasks()` requesting `additional=detail,transfer,file`
- [ ] write tests parsing the fixture: field mapping, every status variant plus an unknown one, zero-size task (no divide-by-zero in `progress`), missing `additional` blocks, and a task with an empty file list
- [ ] write a test for `build_list_params` (comma-separated `additional` encoding)
- [ ] run `cargo test` — must pass before task 6

### Task 6: Human-readable formatting helpers

**Files:**
- Create: `src/format.rs`

- [ ] implement `bytes(u64) -> String` (B/KiB/MiB/GiB/TiB, one decimal above KiB)
- [ ] implement `speed(u64) -> String` (`—` when zero), `duration(Option<u64>) -> String` (`2h 14m`, `∞` when unknown/stalled)
- [ ] implement `percent(f64)`, `ratio(f64)`, and `truncate_ellipsis(&str, width)` using **`unicode-width` display width, not character count** — CJK and emoji occupy two terminal cells
- [ ] write tests for each: boundary values (0, 1023, 1024), rounding, the zero/unknown sentinels
- [ ] write tests for `truncate_ellipsis` with ASCII, a **CJK title** (asserting the result's display width fits), an emoji, and a width smaller than the ellipsis itself
- [ ] run `cargo test` — must pass before task 7

### Task 7: Sort, filter, and search view layer

**Files:**
- Create: `src/view.rs`

- [ ] define `SortKey` (Name, Status, Size, Progress, DownSpeed, UpSpeed, Ratio, Added), `SortDir`, and `StatusFilter` (All, Downloading, Seeding, Finished, Paused, Error)
- [ ] implement `View { sort_key, sort_dir, filter, search }` with `cycle_sort()`, `toggle_dir()`, `cycle_filter()`
- [ ] implement `visible_indices(&[Task], &View) -> Vec<usize>` applying filter → case-insensitive substring search → stable sort
- [ ] write tests for each sort key ascending and descending, including a tie-break that proves stability
- [ ] write tests for each status filter and for search (case-insensitivity, no match, empty query returns everything)
- [ ] run `cargo test` — must pass before task 8

### Task 8: Terminal bootstrap and ratatui skeleton

**Files:**
- Create: `src/ui/mod.rs`
- Create: `src/app.rs`
- Modify: `src/main.rs`

- [ ] implement a terminal guard type that enables raw mode + alternate screen on construction and restores on `Drop`
- [ ] install a panic hook that restores the terminal **before** printing the panic, so a crash never leaves a wrecked shell
- [ ] define a minimal `App { tasks, view, cursor, selected, mode, status_message }` and `Mode` enum (Normal, Search, Confirm, Help)
- [ ] render a bordered empty frame with a title bar and footer; `q` and `Ctrl-C` exit cleanly
- [ ] verify manually: launch, resize the terminal, quit, confirm the shell is intact; also verify `panic!()` in the loop still restores
- [ ] no unit tests this task (terminal lifecycle — verified by running)

### Task 9: Offline fixture mode, task table, and arrow navigation

**Files:**
- Create: `src/ui/table.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/app.rs`
- Modify: `src/cli.rs`

- [ ] add a hidden `--fixture <path>` flag that loads a captured list response straight into `App` with no networking — **Tasks 9, 10 and 12 are otherwise unverifiable, since the poller does not exist until Task 11**; it also makes the 500+ task perf check in Task 21 trivial
- [ ] render the task table with the columns from Technical Details, using `format.rs` helpers and per-status colour
- [ ] add a header row indicating the active sort column and direction
- [ ] implement column widths where Name absorbs slack and truncates (responsive column *dropping* is deferred past v1 — not required)
- [ ] implement cursor movement (`↑`/`↓`/`k`/`j`/PgUp/PgDn/Home/End/`g`/`G`) with scroll offset clamped to the visible list
- [ ] write tests for cursor movement clamping (empty list, single row, past-the-end, past-the-start)
- [ ] run `cargo test` — must pass before task 10

### Task 10: Multi-select with spacebar

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/table.rs`

- [ ] store selection as a `HashSet<String>` of **task IDs**, not row indices
- [ ] implement `toggle_selection()` (Space), `toggle_select_all_visible()` (`a`), `clear_selection()` (Esc)
- [ ] render a selection marker column and highlight selected rows distinctly from the cursor row
- [ ] show selection count and the summed size of selected tasks in the footer
- [ ] write tests for toggle on/off, select-all over a *filtered* subset (must not touch hidden tasks), clear, and the size sum
- [ ] run `cargo test` — must pass before task 11

### Task 11: Async poller and live refresh

**Files:**
- Create: `src/event.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`

- [ ] define `AppEvent { Tasks(Vec<Task>), Error(String), OpProgress{..}, OpDone{..} }` and an mpsc channel (no `Tick` variant — the poller drives data and `EventStream` drives redraws)
- [ ] spawn the poller task on a `tokio::time::interval` of `refresh_secs`, sending `Tasks` or `Error`
- [ ] merge crossterm's `EventStream` with the channel in a `tokio::select!` main loop
- [ ] implement `apply_tasks()`: reconcile new data while **preserving cursor position by task ID** and dropping selections for tasks that no longer exist; **ignore incoming `Tasks` events while `Mode::Confirm` is active** so a pending delete plan cannot go stale under the user
- [ ] add `r` for manual refresh and a non-fatal error banner (poll failures must not kill the UI — it should recover on the next successful tick)
- [ ] write tests for `apply_tasks` reconciliation: reordered list keeps the cursor on the same task, a removed task drops from the selection set, a removed cursor task clamps sanely, and an update is a no-op while in `Confirm` mode
- [ ] run `cargo test` — must pass before task 12

### Task 12: Sort, filter, and search keybindings

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui/mod.rs`

- [ ] wire `s` (cycle sort column), `S` (reverse direction), `f` (cycle status filter)
- [ ] implement search mode: `/` enters it, characters/backspace edit the query, `Enter` applies, `Esc` restores the previous query
- [ ] render the search input line and show the active sort/filter state in the status bar
- [ ] ensure cursor and selection survive a filter change (cursor clamps into the new visible set; selection is untouched)
- [ ] write tests for the search-mode state machine (enter, type, backspace, cancel restores prior query, apply commits)
- [ ] run `cargo test` — must pass before task 13

### Task 13: Delete-path resolution and safety guards

**Files:**
- Create: `src/delete.rs`

- [ ] implement `common_root(&[TaskFile]) -> Option<String>` returning the shared top-level component of a torrent's file list
- [ ] implement `normalize_destination(&str) -> String` stripping a leading `/volumeN` and trimming surrounding slashes
- [ ] implement `resolve_delete_path(&Task) -> Result<String>` following the four-rule order in Technical Details — **crucially, refuse when the file list is non-empty but has no single common root**; fall back to `title` only when the file list is absent or empty
- [ ] implement `validate_path(&str) -> Result<()>` rejecting: empty, `/`, fewer than two components, any `..` component, empty or `.` name component, missing leading slash
- [ ] implement `DeletePlan { items: Vec<DeleteItem> }` as an owned snapshot, so unresolvable tasks surface as per-item skips rather than aborting the batch
- [ ] **write thorough resolution tests** — multi-file torrent resolves to its directory; single-file torrent resolves to the file; nested destination (`video/movies`); destination with leading/trailing slashes; absolute `/volume1/downloads` destination; title differing from the on-disk root; empty file list falls back to title
- [ ] **write the critical refusal test** — a file list whose entries share no common root is REFUSED, not resolved via `title`
- [ ] **write thorough guard tests** — every rejection case above, including a `""` destination (would yield `/name`) and a `..` traversal attempt
- [ ] run `cargo test` — must pass before task 14

### Task 14: Delete confirmation dialog

**Files:**
- Create: `src/ui/dialog.rs`
- Modify: `src/app.rs`
- Modify: `src/ui/mod.rs`

- [ ] implement `build_confirmation(&DeletePlan) -> ConfirmSummary` listing each title, size, resolved path, the total count and total bytes freed, and any unresolvable items flagged as skipped with the reason
- [ ] render a centred modal over the table showing that summary, scrollable when the list exceeds the modal height
- [ ] state whether files will be deleted or only the DSM task (per `delete_files` / `--no-delete-files`), and label the modal clearly when `--dry-run` is active
- [ ] wire `d` to build the snapshot and open the modal (falling back to the cursor row when nothing is selected); `y`/`Enter` confirms, `n`/`Esc`/`q` cancels — **cancel must be the default focus**
- [ ] write tests for `build_confirmation`: total size sum, unresolvable items reported as skipped and excluded from the total, an **empty task list** produces no dialog (an empty *selection* still opens one for the cursor row)
- [ ] run `cargo test` — must pass before task 15

### Task 15: Delete execution — pause, files, then task

**Files:**
- Create: `src/api/file_station.rs`
- Modify: `src/api/download_station.rs`
- Modify: `src/delete.rs`
- Modify: `src/event.rs`

- [ ] implement pure `build_fs_path_params(&[String])` (JSON-array encoding) and `build_ds_id_params(&[String])` (comma-separated) so parameter construction is testable without HTTP
- [ ] implement `file_station::path_info(path)` via `SYNO.FileStation.List` `getinfo` for the pre-delete existence check, and `file_station::delete_paths(paths)` via `method=start` then polling `method=status` on the returned `taskid` with a bounded overall timeout
- [ ] implement `download_station::delete_tasks(ids)` with `force_complete=false`
- [ ] implement pure `plan_delete_ops(item, status) -> Vec<Op>` encoding the status-dependent ordering table (pause first for active tasks; skip later phases on any failure) — this keeps the ordering rule unit-testable with no network
- [ ] implement the delete op task driving those ops: re-validate the path, existence-check (not found ⇒ skip file phase, still delete the task), pause if active, delete files, delete task; honour `--dry-run` by logging intended operations and issuing no destructive call
- [ ] report per-item outcomes via `OpProgress`/`OpDone`; render successes, skips and failures distinctly in the status bar and trigger an immediate refresh afterwards
- [ ] write tests for both param builders (encoding differences between the two APIs)
- [ ] write tests for `plan_delete_ops`: an active task gets a pause op first, a paused task does not, a file-delete failure leaves the task-delete op unissued, a pause failure leaves both unissued
- [ ] run `cargo test` — must pass before task 16

### Task 16: Pause and resume

**Files:**
- Modify: `src/api/download_station.rs`
- Modify: `src/app.rs`
- Modify: `src/event.rs`

- [ ] implement `pause_tasks(ids)` / `resume_tasks(ids)` reusing `build_ds_id_params`
- [ ] wire `p` and `u` to run them as op tasks over the selection (or the cursor row)
- [ ] report per-item results in the status bar and refresh immediately after completion
- [ ] write a test for target-ID selection (selection when non-empty, else cursor row, empty list is a no-op)
- [ ] run `cargo test` — must pass before task 17

### Task 17: Help overlay and first-run experience

**Files:**
- Modify: `src/ui/dialog.rs`
- Modify: `src/app.rs`
- Modify: `src/config.rs`

- [ ] implement the `?` help overlay listing every keybinding, dismissed with any key
- [ ] when required values are unresolved after the merge (per Task 3), write a commented config template to the config path and print an actionable message — do **not** enter the TUI; a merely-missing config file with sufficient CLI/env values must still run
- [ ] make connection/auth failures print a clear diagnostic (host, port, DSM error meaning) and exit non-zero rather than showing an empty table
- [ ] show a helpful empty state when there are zero tasks, and a distinct one when a filter/search hides everything
- [ ] write a test for config-template generation (the produced template round-trips through the parser)
- [ ] run `cargo test` — must pass before task 18

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
