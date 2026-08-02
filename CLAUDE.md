# CLAUDE.md — syno-clean conventions

Working notes for anyone (human or agent) touching this repo. Sections marked
_(pending)_ are filled in as the corresponding task lands; see
`docs/plans/20260802-syno-clean-tui.md` for the full plan.

## What this is

A Rust terminal UI over the Synology DSM HTTP API for reviewing Download
Station tasks and deleting **both** the DSM task and the files it left on the
volume. Nothing is installed on the NAS.

## Toolchain

- Pinned in `rust-toolchain.toml` to an **explicit version** (currently
  `1.97.1`), not `stable`, so CI is reproducible. Components: `rustfmt`,
  `clippy`.
- Edition is set explicitly in `Cargo.toml` (`2024`).

## Validation gate (every task must end here)

```sh
cargo fmt --all
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
```

All four must be clean before the next task starts. Warnings are errors.

## Dependency rules

- **Never add `crossterm` as a direct dependency.** It is consumed through
  `ratatui::crossterm` so there is exactly one crossterm in the tree and no
  version-skew type errors. ratatui 0.30 pulls crossterm 0.29 via
  `ratatui-crossterm` with default features (`events`, `bracketed-paste`) —
  note that crossterm's `event-stream` feature is **not** enabled, so the async
  input source has to be built without `crossterm::event::EventStream`
  (e.g. a `spawn_blocking` reader feeding the same mpsc channel the poller
  uses).
- `reqwest` is `default-features = false` with `rustls` (reqwest 0.13 renamed
  the old `rustls-tls` feature to `rustls`), plus `json`, `query`, `form`. No
  OpenSSL, no system TLS.
- `tracing` writes to a **file**, never stdout — the TUI owns the terminal.

## Module layout

```
src/
  main.rs                  thin binary: entrypoint, runtime setup, terminal guard
  lib.rs                   library root, declares every module below
  cli.rs                   clap definitions
  config.rs                TOML config, env overrides, validation, sid cache
  error.rs                 Error enum + DSM code mapping
  format.rs                human-readable bytes/speed/eta/percent, width-correct truncation
  model.rs                 Task, TaskFile, TaskStatus, JSON -> Task
  view.rs                  SortKey/SortDir/StatusFilter/search -> visible indices
  delete.rs                delete-path resolution, safety guards, op ordering
  app.rs                   App state, key handling, selection
  event.rs                 AppEvent, poller task, op tasks
  ui/{mod,table,dialog}.rs frame layout, task table, modals
  api/{mod,client,auth,download_station,file_station}.rs
tests/fixtures/task_list.json
```

**Why both `lib.rs` and `main.rs`:** the crate is built up module by module, and
in a bin-only crate every `pub` item that main cannot reach yet is a `dead_code`
warning — which `-D warnings` turns into a hard failure on every task before the
module is wired in. Splitting the library out removes that friction without
switching the lint off, and lets `tests/` reach the code. Add new modules to
`lib.rs`; keep `main.rs` a thin shell that calls into `syno_clean::`.

## Error handling

- One crate-wide `Error` enum in `error.rs` (`thiserror`) plus a
  `Result<T>` alias. Variants: `Http`, `Dsm { code, api }`, `Config`, `Io`,
  `Parse`, `Auth`, `UnsafePath { path, reason }`, `ApiUnavailable { api, reason }`.
- `anyhow` is for the top of `main` only; library code returns
  `error::Result<T>`.
- DSM reports failures as a bare integer, so `dsm_message(code, api) -> String`
  owns the translation. The 100-119 codes are common to every API; the 400-range
  is **API-specific**, and only the `SYNO.API.Auth` table is implemented — a 400
  from Download Station must *not* render as "incorrect password", so unknown
  (code, api) pairs fall back to a message naming the raw number.
- `error::is_session_error(code)` is the single definition of "re-login and
  retry once" (106 / 107 / 119). `OTP_REQUIRED_CODE` (403) drives the 2FA prompt.
- Missing APIs are reported by DSM *package* ("File Station is not installed on
  this NAS"), not by raw API name, via `Error::api_missing`.

## Configuration precedence

**CLI flags > `SYNO_CLEAN_*` env vars > config file > defaults.**

- XDG semantics on *all* platforms (via `etcetera`'s XDG strategy), so the
  documented paths are the real ones on macOS too: config at
  `~/.config/syno-clean/config.toml`, cache and logs at `~/.cache/syno-clean/`.
- Unknown config keys are **warned about and ignored**, never a hard error — an
  older binary must tolerate a newer config file. Do not use
  `deny_unknown_fields`.
- `host` and `username` are validated as present in `config::merge`, so every
  later module may assume them.
- The password is never written to the config file. It comes from
  `SYNO_CLEAN_PASSWORD` or an interactive `rpassword` prompt, taken **before**
  the alternate screen is entered.
- Session `sid` cache lives at `~/.cache/syno-clean/session.json`, mode `0600`,
  keyed by `{host}:{port}/{username}` so multiple NASes/accounts never evict
  each other. Normal quit does **not** log out; only `--logout` does. A corrupt
  cache is discarded with a warning — it is an optimization and must never
  block startup.
- Config layers are `Option`-per-field (`config::Config`) so "absent" stays
  distinguishable from "set to the default"; the concrete defaults are the
  `config::DEFAULT_*` consts, applied in `merge`. The default port follows the
  scheme (5001 https / 5000 http). Boolean CLI flags are **one-way switches** —
  an unset `--insecure` never overrides a config `insecure = true`.

### Two injection seams (keep them — the tests depend on them)

- **Environment**: nothing outside `config::system_env` calls `std::env::var`.
  Config reads take `EnvLookup<'_> = &dyn Fn(&str) -> Option<String>`, so
  precedence tests are pure and the suite stays parallel-safe. Never write a
  test that sets a process env var.
- **Filesystem**: paths come from a `config::Paths` value —
  `Paths::discover()` in `main`, `Paths::with_base(tempdir)` in tests. **No test
  may read or write the real `~/.config/syno-clean` or `~/.cache/syno-clean`.**
  `tempfile` is a dev-dependency for exactly this.

`main` initializes logging *before* loading the config, so config warnings
reach the log file, and holds the `WorkerGuard` for the whole of `main`.

## DSM API conventions

- **DSM 7 only**, using the documented v1 `SYNO.DownloadStation.Task` API for
  all four operations (list / delete / pause / resume) — no mixed-API seam.
- **No hardcoded API versions.** `SYNO.API.Info` is queried once at startup
  from the fixed `/webapi/query.cgi` (it is *not* served from `entry.cgi`);
  every later call picks the highest version inside the discovered
  `minVersion..maxVersion` range that this client understands.
- On DSM error **106 / 107 / 119** the client re-logs-in once and retries
  exactly once.
- List-valued parameters are encoded differently per API: Download Station v1
  takes **comma-separated** strings, File Station takes **JSON arrays**. All
  encoding lives in pure `build_*_params() -> Vec<(&str, String)>` functions so
  it is unit-testable and changeable in one place.

### Using the client (`api::client`)

- Never build a URL or pick a version by hand. Call
  `client.call::<T>(api, method, SUPPORTED, &params)` — it resolves the
  endpoint from the discovery map, attaches `_sid`, and owns the re-login
  retry. `call_no_data` is the variant for methods that answer with a bare
  `{"success": true}`. `SynoClient::send` is the no-retry escape hatch and
  exists for `auth::login`, which must not recurse into the retry.
- Each API module declares its own `SUPPORTED: VersionRange` const (inclusive
  `(min, max)`); `pick_version_in` takes the top of the overlap with what the
  NAS advertises and errors naming both ranges when there is none.
- Three envelope readers: `parse_envelope` (payload required),
  `parse_envelope_optional` (payload may be absent), `check_envelope` (success
  only). A protocol violation — success with no data, failure with no code, a
  body that is not an envelope — is an `Error::Parse` built with
  `serde::de::Error::custom`; do not add error variants for these.
- Credentials are redacted in `Debug`. Keep it that way: `SynoClient` derives
  `Debug` and holds them.

### Task model (`model.rs`)

- The DSM wire shape lives in **private `Raw*` structs** and is collapsed into
  a flat `Task` by `From<RawTask>` (`#[serde(from = "RawTask")]`). Nothing
  outside `model.rs` reaches through `additional.transfer.…`.
- **Every `additional` sub-block is optional.** A task with no `additional` at
  all, or with only some of `detail`/`transfer`/`file`, parses with zeroed
  counters. One odd task must never blank the whole table.
- **Numbers may arrive as JSON numbers or as strings** (DSM is inconsistent per
  field and per build — file sizes and timestamps especially). Every numeric
  field goes through the permissive `de_u64` / `de_u32` / `de_i64_opt`
  deserializers. Do not add a plain `u64` field.
- `TaskStatus::Unknown(String)` keeps an unrecognized status verbatim so a row
  is never dropped; `from_dsm_str` trims and case-folds. `TaskStatus::KNOWN`
  lists the ten documented variants.
- `progress()` / `ratio()` / `eta()` all guard their denominators — a zero-size
  task is ordinary, not an error.

### Formatting (`format.rs`)

- Sizes are **binary** (1 KiB = 1024 B) because that is what DSM reports.
  `B`/`KiB` print as whole numbers, `MiB` and up get one decimal. The unit is
  picked *after* rounding, so nothing ever renders as `1024 KiB`.
- **Zero and unknown are different sentinels.** `speed(0)` is `DASH` (`—`) — the
  task is idle, not unknown; an ETA that cannot be computed is `INFINITY` (`∞`).
  Do not collapse them into `0`.
- `percent` takes a **fraction** (`0.0..=1.0`), matching `Task::progress()`, not
  an already-multiplied percentage.
- **Never size or pad a column with `str::len` or `chars().count()`.** Use
  `format::display_width` and `format::truncate_ellipsis`, which measure
  terminal cells via `unicode-width`; the fixture's CJK and emoji titles are
  there to keep that honest. `truncate_ellipsis` never exceeds the requested
  width and may stop one cell short rather than clip a double-width character
  in half.

### The task-list fixture

`tests/fixtures/task_list.json` is a full `list` envelope covering every known
status plus an unknown one, missing/partial `additional` blocks, an empty file
list, a non-BT download, a zero-size task, a CJK title, an emoji title, a file
list with **no common root** (the `delete.rs` refusal case) and a
`/volume1/...` destination. It drives the `model.rs` parser tests and the
offline `--fixture` mode.

⚠️ It is currently **hand-written and marked `PROVISIONAL`** in a top-level
`_comment` key — no NAS was reachable when it was written. Re-capture with
`syno-clean --dump-tasks-json > tests/fixtures/task_list.json` and drop the
marker (and the test asserting it) once it comes from a real DSM 7 NAS.

### Hidden debugging flags

`--dump-api-info` and `--dump-tasks-json` print a raw DSM response verbatim and
exit. They are `hide = true` — debugging aids, not advertised interface.
`--dump-api-info` deliberately does **not** log in, since discovery needs no
session and that is exactly the case where a login is what is broken.

`--fixture <path>` runs the whole TUI over a captured `list` response with no
network call and — deliberately — **no configuration at all**: it short-circuits
`main` before the config merge, so it works on a machine with no config file,
no host and no password. The file is a full DSM envelope, read through the same
`parse_envelope::<TaskList>` the live path uses (`app::parse_fixture`), never a
second, laxer parser: a fixture only the fixture loader can read would prove
nothing about what the NAS sends.

## Delete ordering (three phases, ordered for recoverability)

| Task status | Ordering |
|---|---|
| Downloading, Seeding, Waiting, Finishing, HashChecking, Extracting | pause → confirm paused → delete files → delete task |
| Paused, Finished, Error | delete files → delete task |

- Any phase failing **skips all later phases**. The task then survives still
  pointing at its data — nothing is orphaned.
- The DS API removes the task but never the payload; files go via
  `SYNO.FileStation.Delete` `start` + `status` polling (a recursive delete of a
  big torrent directory can outlive the HTTP timeout).
- For **incomplete** tasks, "path not found" during the file phase counts as
  success — Download Station cleans up its own partial data.

## Path-safety invariants (the dangerous part)

Resolution order in `delete.rs`, and it **refuses rather than guesses**:

1. File list present with a single common top-level component → use it. That is
   the authoritative on-disk name, even when the display title differs.
2. File list present but entries share **no** single top-level component →
   **REFUSE**, report the item as skipped. Never fall back to `title` here; a
   guessed path could match an unrelated folder and be recursively deleted.
3. File list absent or empty (HTTP/FTP/NZB tasks) → fall back to `title`.
4. Normalize `destination`: strip a leading `/volumeN`, trim surrounding
   slashes. Join as `/{destination}/{name}`.

Syntactic guards (`delete::validate_path`) — a resolved path is refused if it is
empty, `/`, has fewer than two components, contains a `..` or `.` component
(anywhere, not just at the end), has an empty component, or lacks a leading `/`.
Two further guards are **not** in the plan and exist because each turns a merely
wrong path into a *share-destroying* one if anything downstream normalizes it:

- **no control characters** — a NUL truncates the path in any C-based consumer,
  so `/downloads\0/Some.Torrent` arrives as `/downloads`, the share root;
- **no blank (whitespace-only) components** — if any layer trims,
  `/   /Some.Torrent` collapses to `/Some.Torrent`, again a share root.
  Incidental leading/trailing spaces *inside* a real name are left alone.

The on-disk name is guarded separately before it is joined
(`delete::validate_name`): it must be a single component, so the `title`
fallback cannot smuggle a `/` in and delete one level deeper than the task's own
directory. `common_root` compares components **exactly** — the NAS filesystem is
case-sensitive, and an entry with an empty or absolute `filename` makes the whole
list unresolvable rather than letting a leading `/` report the volume as the
shared root. A deselected file still counts towards the common root: `selected`
describes what was downloaded, not what is on disk.

Semantic guard — `SYNO.FileStation.List` `getinfo` runs against the resolved
path before any recursive delete. Not found ⇒ report *skipped* (the files were
probably already removed by hand) and still delete the DSM task, which is the
harmless half.

Snapshot semantics — the `DeletePlan` is an owned snapshot taken when the
confirmation dialog opens. Refreshes are suspended while `Mode::Confirm` is
active, and `validate_path` is re-run immediately before each File Station
call, so what the user read on screen is exactly what gets deleted.

## State conventions

- `App` holds all state; rendering is a pure function of `&App`.
- Sorting/filtering produce a `Vec<usize>` of indices into the task list rather
  than cloning or reordering the source data.
- **Cursor and selection are keyed by task ID, not row index**, so a refresh
  that reorders or removes rows never silently reassigns what is selected.
  `App::cursor` is a position in the *visible* list; the reconciliation that
  keeps it on the same task lives in Task 11's `apply_tasks`.
- **`a` (`toggle_select_all_visible`) touches only the visible rows**, in both
  directions — a filtered-out task is never armed for deletion by a key press
  the user aimed at what was on screen, and never quietly *un*armed either.
  `Esc` (`clear_selection`) is the opposite and clears everything, hidden rows
  included; it is the "I do not know what is armed" key.
- The selection footer counts and sums **`App::selected_tasks()`** — the
  selected IDs that still name a real task — not `selected.len()`. Between a
  task vanishing on the NAS and Task 11's refresh pruning the set, the raw
  length would over-report while the size sum did not.
- `App::handle_key` ignores anything that is not `KeyEventKind::Press` —
  Windows and the kitty protocol report releases too, and acting on both halves
  runs every binding twice. `Ctrl-C` is handled before the mode dispatch so it
  works from inside a modal.

### Sort, filter and search (`App::change_view`)

- **Every view change goes through `App::change_view`** (`s`, `S`, `f`, and
  every keystroke in the search box). It follows the cursor's task by **ID**
  through the re-sort or the re-filter, falls back to holding the row number
  when the change hides that task, and then clamps — the same rules
  `apply_tasks` uses for a refresh, for the same reason: a cursor that lands on
  a different torrent is how the wrong thing gets deleted.
- **A view change never touches the selection.** A filter is a question about
  what to look at, not an instruction to disarm rows that scrolled off screen.
- **Search matches live, on every keystroke**, so `Enter` *commits* rather than
  applies. The query being edited is `view.search` itself; `App::search_backup`
  holds what it was when `/` was pressed and is the only way `Esc` can undo an
  abandoned edit. `/` deliberately keeps the committed query so a search can be
  refined.
- **`Esc` is mode-specific**: cancel-and-restore in `Mode::Search`, clear the
  selection in `Mode::Normal`. Keep both halves correct when adding modes.
- **In `Mode::Search` every printable key is text**, never a binding — a box
  that cannot type `q` cannot search. Only `Enter`, `Esc`, `Backspace` and the
  global `Ctrl-C` are commands; `Ctrl`/`Alt` chords are dropped rather than
  typed, and `Shift` is not (it is how a capital letter arrives).

### The delete confirmation (`ui::dialog`, `App::begin_delete`)

- **`d` never deletes.** It snapshots (`DeletePlan::snapshot`) the selection —
  or, when nothing is selected, the row under the cursor — and opens
  `Mode::Confirm`. An empty plan opens no dialog at all.
- **Cancel is the default focus** (`ConfirmFocus::default() == Cancel`), so
  `Enter` on an untouched dialog *cancels*. `y` is the deliberate one-key
  confirm; `n` / `Esc` / `q` cancel; `q` closes the dialog rather than the
  program (`Ctrl-C` still quits). Every unrecognized key does nothing — never
  "defaults to confirming".
- **The dialog performs no I/O.** `App::confirm_delete` parks the snapshot for
  `App::take_confirmed_delete`, which the event loop drains — the same
  request/take shape as `r`. Task 15 hangs the three-phase execution off that
  hook; keep the state machine free of the network so it stays testable without
  a runtime.
- **Refused items are rendered, never dropped**: `Target::Refused` shows as
  `SKIPPED` with its reason and its bytes are excluded from the total. The modal
  also states whether the files go with the task (`delete_files`) and is
  labelled `DRY RUN` when `--dry-run` is active — both come from
  `delete::DeleteOptions`, which is session state and therefore a *parameter* of
  `build_confirmation`, not a field of the plan.
- `build_confirmation(&DeletePlan, DeleteOptions) -> ConfirmSummary` produces
  plain strings and counts and is where the wording and the arithmetic are
  tested; `render_confirm` only draws. Modal scroll is clamped in `App` against
  the line count and again at render against the height, the same split as
  `ui::table`'s derived scroll offset.
- `--fixture` mode forces `DeleteOptions::dry_run()`: there is no client
  offline, so a modal promising a real recursive delete would be lying.

### Terminal lifecycle (`ui`)

- `ui::TerminalGuard::new()` is the **only** place raw mode and the alternate
  screen are entered, and its `Drop` the only place they are left. It owns the
  `Terminal`, so a drawable terminal cannot outlive the restoration, and every
  exit path — clean quit, `?` out of the loop, unwinding panic — restores.
  Errors in `Drop` go to the log; there is nowhere else to put them.
- `ui::install_panic_hook()` **chains** to the previous hook rather than
  replacing it (the backtrace must still print) and is `Once`-guarded so a
  double install cannot nest. Install it *before* constructing the guard.
- Non-TTY stdout is a clean failure, not a corrupted terminal:
  `TerminalGuard::new()` returns the `enable_raw_mode` error and `main` prints
  an actionable message and exits non-zero.
- `ui::render(&mut Frame, &App)` is pure and takes `&App`. That is what makes
  the frame testable with `ratatui::backend::TestBackend`, which renders into
  an in-memory `Buffer` with **no TTY** — the right tool for layout regressions
  even though the terminal lifecycle itself stays "verified by running".
- The input source is a `spawn_blocking(event::read)` awaited one at a time,
  because crossterm's `event-stream` feature is unavailable through ratatui's
  re-export. Exactly one read is ever in flight, so nothing lingers on the
  blocking pool at shutdown.

### The event loop and the poller (`main.rs`, `event.rs`)

- The loop is `draw → select!(terminal event, AppEvent) → apply`. **The pending
  terminal read lives in a variable outside the loop** (`pending_read:
  Option<JoinHandle<_>>`) and is only cleared when it resolves. A blocking read
  cannot be cancelled, so re-creating it per iteration would spawn one orphaned
  stdin reader per poller tick and they would then take turns eating the user's
  keystrokes. The `select!` yields a `Next` enum rather than acting in its
  branch bodies, so the borrow ends with the expression.
- **Everything that touches the network runs off the loop** and reports through
  the single `mpsc` channel of `event::AppEvent`. There is no `Tick` variant:
  the poller drives data, and data drives redraws.
- **The poller is non-fatal.** A failed tick becomes `AppEvent::Error` and the
  interval keeps running; the next successful tick clears the banner. Never
  `return` out of the poller on a poll failure — a NAS that vanishes for a
  minute is ordinary. It ends only when the channel closes or `main` aborts it.
- **`r` is a request, not an action.** `App::request_refresh` sets a flag the
  loop drains with `take_refresh_request` and forwards to an
  `event::RefreshHandle` (an `Arc<Notify>`, so repeated presses coalesce into
  one poll). `App` holds no runtime handle and every key press stays a pure
  state transition.
- `App::apply_event` is the `AppEvent` counterpart of `App::handle_key` — same
  shape, same testability. Keep the reconciliation logic there, not in `main`.
- **`App::error` is not `App::status_message`.** The error banner is cleared
  automatically by the next successful refresh and rendered red with `⚠`; the
  status message survives underneath and returns when the banner clears.
- `App::apply_tasks` invariants: the cursor follows its **task ID** through a
  reorder; a cursor task that vanished holds its *row number* and clamps; stale
  IDs are pruned from the selection; and a `Tasks` event arriving in
  `Mode::Confirm` is **dropped entirely**, so the delete plan on screen cannot
  go stale while the user reads it.

### The task table (`ui::table`)

- The table is laid out **by hand**, not with ratatui's `Table` widget: every
  cell is truncated and padded through `format::truncate_ellipsis` /
  `display_width` so a CJK or emoji title cannot shear the columns to its
  right, and each row is emitted as one pre-composed `Line`.
- `COLUMNS` is the single definition of the column order, headers, fixed widths
  and alignment. **Name is the only flexible column** — it absorbs all the
  slack down to `MIN_NAME_WIDTH`; on a terminal narrower than `ideal_width()`
  the rightmost columns are clipped by the buffer, because responsive column
  *dropping* is deferred past v1.
- Column headers are spelled exactly like `view::SortKey::label()`, and the
  sort marker is placed by comparing the two. Do not introduce a second
  key→column mapping. `SortKey::Added` has no column and so shows no marker.
- **The scroll offset is derived, never stored** (`table::scroll_offset`): a
  pure function of cursor, row count and viewport height, so no second piece of
  state can disagree with a cursor that a refresh moved. `App` holds only the
  cursor; the event loop pushes the body height in via `App::set_page_size`
  after each draw so `PageUp`/`PageDown` move a real screenful.
- Cursor movement **clamps and never wraps** — a `j` held at the bottom of a
  long list wrapping to the top is how the wrong row gets deleted.

## Testing philosophy

Deliberately narrow. Pure logic where bugs are silent and expensive is tested:
`format`, `model`, `view`, `error`, `api::client` envelope parsing, `app`
selection/reconciliation, and above all **`delete`** — path resolution, guards
and op ordering, which is the highest-value test in the project.

Not tested (verified by running the binary): ratatui rendering, key wiring,
live HTTP against DSM. No mocking framework and no trait abstraction over the
HTTP client — one implementation does not warrant one. Offline verification
uses the hidden `--fixture <path>` flag.
