# CLAUDE.md — syno-clean conventions

Working notes for anyone (human or agent) touching this repo: the conventions the
code actually follows and, more importantly, **why**, so a later change does not
undo a decision without knowing it was one.

This describes the shipped v0.1.0 code. Where a rule exists because getting it
wrong destroys data or silently lies to the user, it says so — those are the ones
not to "simplify". The historical record of how each rule arrived (task by task,
with the alternatives that were rejected) is in
`docs/plans/20260802-syno-clean-tui.md`.

Contents:

1. [What this is](#what-this-is)
2. [Toolchain and the validation gate](#toolchain-and-the-validation-gate)
3. [Dependency rules](#dependency-rules)
4. [Module layout](#module-layout)
5. [Configuration precedence](#configuration-precedence)
6. [Error handling](#error-handling)
7. [DSM API conventions](#dsm-api-conventions)
8. [The dangerous part: delete ordering and path safety](#the-dangerous-part-delete-ordering-and-path-safety)
9. [UI and state conventions](#ui-and-state-conventions)
10. [Formatting](#formatting)
11. [Offline and debugging flags](#offline-and-debugging-flags)
12. [Testing philosophy](#testing-philosophy)
13. [Known gaps and outstanding debt](#known-gaps-and-outstanding-debt)

## What this is

A Rust terminal UI over the Synology DSM HTTP API for reviewing Download
Station tasks and deleting **both** the DSM task and the files it left on the
volume. Nothing is installed on the NAS.

## Toolchain and the validation gate

- Pinned in `rust-toolchain.toml` to an **explicit version** (currently
  `1.97.1`), not `stable`, so CI is reproducible. Components: `rustfmt`,
  `clippy`. CI reads the pin from that file rather than repeating it, so the
  version has exactly one home.
- Edition is set explicitly in `Cargo.toml` (`2024`).

```sh
cargo fmt --all
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
```

All four must be clean before any change is considered done. Warnings are
errors, in CI and locally.

## Dependency rules

- **Never add `crossterm` as a direct dependency.** It is consumed through
  `ratatui::crossterm` so there is exactly one crossterm in the tree and no
  version-skew type errors. ratatui 0.30 pulls crossterm 0.29 via
  `ratatui-crossterm` with default features (`events`, `bracketed-paste`).
  Crossterm's **`event-stream` feature is not enabled** by that path, so
  `crossterm::event::EventStream` does not exist here and the async input source
  is a `spawn_blocking(event::read)` reader instead — see
  [the event loop](#the-event-loop-and-the-poller-mainrs-eventrs).
- `reqwest` is `default-features = false` with `rustls` (reqwest 0.13 renamed
  the old `rustls-tls` feature to `rustls`), plus `json`, `query`, `form`. No
  OpenSSL, no system TLS. The `rustls` feature resolves to the **`aws-lc-rs`**
  provider, which compiles C through `cmake` — that is why the aarch64 Linux
  cross build in `release.yml` installs a C toolchain and not just a linker.
- `tracing` writes to a **file**, never stdout — the TUI owns the terminal.
- `tempfile` is a dev-dependency, used so no test touches a real XDG directory.

## Module layout

```
src/
  main.rs                  thin binary: startup, event loop, op dispatch
  lib.rs                   library root, declares every module below
  cli.rs                   clap definitions
  config.rs                TOML config, env overrides, validation, sid cache, paths
  error.rs                 Error enum, DSM code mapping, startup diagnostics
  format.rs                human-readable bytes/speed/eta/percent, width-correct truncation
  model.rs                 Task, TaskFile, TaskStatus, JSON -> Task
  view.rs                  SortKey/SortDir/StatusFilter/search -> visible indices
  delete.rs                delete-path resolution, safety guards, op ordering
  app.rs                   App state, key handling, selection, event application
  event.rs                 AppEvent, poller task, op tasks (delete / pause / resume)
  ui/
    mod.rs                 frame layout and dispatch, terminal guard, panic hook
    table.rs               task table widget
    dialog.rs              confirmation modal, help overlay
  api/
    mod.rs
    client.rs              reqwest client, API discovery, envelope, sid handling, retry
    auth.rs                login / logout
    download_station.rs    list / delete / pause / resume
    file_station.rs        path info lookup, delete files
tests/fixtures/task_list.json
```

**Why both `lib.rs` and `main.rs`:** in a bin-only crate every `pub` item that
`main` cannot reach yet is a `dead_code` warning — which `-D warnings` turns into
a hard failure. Splitting the library out removes that friction without switching
the lint off, and lets `tests/` reach the code. Add new modules to `lib.rs`; keep
`main.rs` a thin shell that calls into `syno_clean::`.

`main.rs` owns exactly four things: startup (config, first run, discovery,
login), the `select!` event loop, dispatching what the loop drains onto
`event::` op tasks, and the process exit code. Everything else belongs in the
library, where it is testable.

## Configuration precedence

**CLI flags > `SYNO_CLEAN_*` env vars > config file > defaults.**

- XDG semantics on *all* platforms (via `etcetera`'s XDG strategy), so the
  documented paths are the real ones on macOS too: config at
  `~/.config/syno-clean/config.toml`, cache and logs at `~/.cache/syno-clean/`
  (`config::LOG_FILE` is `syno-clean.log`).
- Unknown config keys are **warned about and ignored**, never a hard error — an
  older binary must tolerate a newer config file. Do not use
  `deny_unknown_fields`.
- `host` and `username` are validated as present in `config::merge`, so every
  later module may assume them.
- Config layers are `Option`-per-field (`config::Config`) with a container-level
  `#[serde(default)]`, so "absent" stays distinguishable from "set to the
  default"; the concrete defaults are the `config::DEFAULT_*` consts, applied in
  `merge`. The default port follows the scheme (5001 https / 5000 http).
- Boolean CLI flags are **one-way switches** — an unset `--insecure` never
  overrides a config `insecure = true`. `--insecure` and `--dry-run` can only
  turn a setting on, `--no-delete-files` only off.
- `refresh_secs = 0` is **rejected**, not clamped: a zero-second poll would
  hammer the NAS and is far more likely a typo than an intent.
- An unparseable env value (`SYNO_CLEAN_PORT=abc`) is an error naming the
  variable, not a silent fall-through to the file value.
- **A blank env var is unset, at every var.** `Config::from_env` filters on
  `trim().is_empty()` exactly as `parse_env` does, so an exported-but-empty
  `SYNO_CLEAN_HOST` falls through to the config file. A blank from the *CLI*
  does not fall through — `config::first_set` trims it to nothing and leaves the
  value unresolved, because `--host "  "` is a mistake to report rather than a
  reason to connect somewhere the user did not name.
- There is deliberately **no `SYNO_CLEAN_DELETE_FILES`**: an environment variable
  that silently disables the program's main function is a footgun. `merge` does
  not consult an env layer for it either — a branch that can never be taken is
  a claim that the var exists.
- The password is never written to the config file. It comes from
  `SYNO_CLEAN_PASSWORD` or an interactive `rpassword` prompt, taken **before**
  the alternate screen is entered. 2FA likewise via `SYNO_CLEAN_OTP` or a prompt
  when DSM answers 403.
- Session `sid` cache lives at `~/.cache/syno-clean/session.json`, mode `0600`
  inside a `0700` cache directory (the log file next to it is `0600` too),
  keyed by `{host}:{port}/{username}` so multiple NASes/accounts never evict
  each other. Normal quit does **not** log out — that would invalidate the cache
  and defeat it; only `--logout` does. A corrupt cache is discarded with a
  warning (`SessionCache::load` returns `Self`, not `Result<Self>`) — it is an
  optimization and must never block startup.

`main` initializes logging *before* loading the config, so config warnings reach
the log file, and holds the `tracing_appender::WorkerGuard` for the whole of
`main` — dropping it early silently discards buffered lines.

**The log level is hardcoded.** `config::init_logging` sets
`with_max_level(tracing::Level::INFO)` and never consults `RUST_LOG`; there is no
`--verbose` flag either. Every `tracing::debug!` and `trace!` in the crate is
therefore dead at runtime — write them for a future level switch, but never rely
on one for a diagnostic a user is expected to send in. Anything a bug report
needs must be `info!` or higher. `--log-file` changes only *where* the file goes.

### First run

- **A missing config *file* is never an error.** Only values still unresolved
  after the merge are. `config::missing_required` asks that question with the
  very same resolution `merge` enforces (`resolved_host` / `resolved_username`),
  never a second copy of the rule, and a test asserts the two can never
  disagree. `main::first_run` then writes `config::CONFIG_TEMPLATE` and exits
  non-zero without entering the TUI.
- The template has **every key commented out** — a starter config shipping a live
  `host = "nas.local"` would aim the next invocation at a host the user never
  named — and `write_config_template` never overwrites an existing file.

### Two injection seams (keep them — the tests depend on them)

- **Environment**: nothing outside `config::system_env` calls `std::env::var`.
  Config reads take `EnvLookup<'_> = &dyn Fn(&str) -> Option<String>`, so
  precedence tests are pure and the suite stays parallel-safe. Never write a
  test that sets a process env var.
- **Filesystem**: paths come from a `config::Paths` value —
  `Paths::discover()` in `main`, `Paths::with_base(tempdir)` in tests. **No test
  may read or write the real `~/.config/syno-clean` or `~/.cache/syno-clean`.**

## Error handling

- One crate-wide `Error` enum in `error.rs` (`thiserror`) plus a `Result<T>`
  alias. Variants: `Http`, `Dsm { code, api }`, `Config`, `Io`, `Parse`, `Auth`,
  `UnsafePath { path, reason }`, `ApiUnavailable { api, reason }`.
- **The variant list is closed in practice.** Three classes of failure
  deliberately reuse an existing variant rather than growing it: protocol
  violations (success with no data, failure with no code, a body that is not an
  envelope) are `Error::Parse` built with `serde::de::Error::custom`; failures
  with no DSM code behind them (`path_err_num > 0`, an id the per-task result
  array never mentioned, a bounded wait expiring) are `Error::Io`; unresolved
  required config is `Error::Config`.
  Adding a variant for these is the change to argue for, not to make quietly.
  The `Error::Io` reuse has **one spelling each**: `Error::operation_failed(msg)`
  and `Error::timed_out(msg)` in `error.rs`. Three sites reached for it
  independently before, and hand-rolling a fourth is how the kinds drift.
- `anyhow` is for the top of `main` only; library code returns
  `error::Result<T>`.
- DSM reports failures as a bare integer, so `dsm_message(code, api) -> String`
  owns the translation. It returns `String`, not `&'static str`, because the
  fallback has to embed the unrecognized number. The 100-119 codes are common to
  every API; the 400-range is **API-specific**, and two tables are implemented —
  `SYNO.API.Auth` and, selected by the `SYNO.FileStation` prefix,
  `SYNO.FileStation.*`. The same number means different things in each (403 is
  "permission denied" on File Station and "2-step verification required" on
  Auth), and a 400 from Download Station — which has no table — must *not*
  render as "incorrect password", so every other `(code, api)` pair falls
  through to the common codes and then to a message naming the raw number.
- `error::is_session_error(code)` is the single definition of "re-login and
  retry once" (106 / 107 / 119). `OTP_REQUIRED_CODE` (403) drives the 2FA prompt.
- Missing APIs are reported by DSM *package* ("File Station is not installed on
  this NAS"), not by raw API name, via `Error::api_missing`.
- **A startup connection or login failure exits non-zero with
  `error::connection_diagnostic`** — host and port tried, the DSM code in words,
  and one hint — and never enters the TUI. An empty table is exactly what a NAS
  with no downloads looks like, so a failed login must never be able to render as
  one. Failures *during* a session stay non-fatal (see the poller).

## DSM API conventions

- **DSM 7 only**, using the documented **v1** `SYNO.DownloadStation.Task` API for
  all four operations (list / delete / pause / resume) — no mixed-API seam. DSM 7
  also ships `SYNO.DownloadStation2.Task` (what the web UI uses), but its `list`
  is undocumented and it returns numeric statuses and a different `additional`
  shape; migrating is a post-v1 question. A DSM 6 NAS gets a clear error.
- **No hardcoded API versions.** `SYNO.API.Info` is queried once at startup
  from the fixed `/webapi/query.cgi` (it is *not* served from `entry.cgi`);
  every later call picks the highest version inside the discovered
  `minVersion..maxVersion` range that this client understands.
  **Exception, deliberate:** `DS_TASK_SUPPORTED` is pinned to `(1, 1)` even
  though DSM 7 advertises 3. v2/v3 change the status encoding and the
  `additional` shape `model.rs` is built around, so following the NAS upward
  would silently break parsing. A test pins the pin.
- On DSM error **106 / 107 / 119** the client re-logs-in once and retries
  exactly once — and on **105** as well, which is the ambiguous one.
  `error::may_be_stale_session` is that wider set; `error::is_session_error` is
  still the unambiguous three, because 105 must keep *rendering* as a permission
  error. A real DSM 7 answers a dead sid from `SYNO.DownloadStation.Task` with
  105, never 119, so without this an expired cached session failed every request
  until `session.json` was deleted by hand. The ambiguity is settled by trying:
  if a fresh session is still refused, `SynoClient::permission_is_real` latches
  and 105 stops triggering a retry — otherwise an account that genuinely lacks
  Download Station permission would log in once per poll, every `refresh_secs`,
  for as long as the program is open.
- List-valued parameters are encoded differently per API: Download Station v1
  takes **comma-separated** strings, File Station takes **JSON arrays**. All
  encoding lives in pure `build_*_params() -> Vec<(&str, String)>` functions so
  it is unit-testable and changeable in one place. A cross-API test pins the
  difference, using a path containing a comma — the reason one builder cannot
  serve both.

### Using the client (`api::client`)

- Never build a URL or pick a version by hand. Call
  `client.call::<T>(api, method, SUPPORTED, &params)` — it resolves the
  endpoint from the discovery map, attaches `_sid`, and owns the re-login
  retry. `SynoClient::send` is the no-retry escape hatch, and
  `SynoClient::post_form` is the **POST** one: both exist for `auth::login`,
  which must not recurse into the retry that called it. Login is the only POST
  in the program, and it is a POST for a reason — a DSM query string is written
  in full to the NAS's nginx access log, so `passwd=` there would persist the
  account password to disk on every login. Only `api`/`version`/`method` ride in
  the query.
- Each API module declares its own `SUPPORTED: VersionRange` const (inclusive
  `(min, max)`); `pick_version_in` takes the top of the overlap with what the
  NAS advertises and errors naming both ranges when there is none.
- Two envelope readers: `parse_envelope` (payload required) and
  `check_envelope` (success only, payload ignored). Logout, pause and resume
  answer with a bare `{"success": true}`, and the retry path has to classify a
  response before committing to a payload type — hence two, not one.
  `Envelope::into_result` is the third, lower-level form when an absent payload
  needs to stay an `Option`.
- **A per-task result array is read for the id that was asked about**, never
  scanned. `download_station::check_task_result(id, results)` treats an id DSM
  reported nothing for as a *failure*; `check_task_results` (plural) is the
  "any non-zero code" collapse behind it and is vacuously `Ok` on an empty
  slice, which is only ever correct once the caller knows the slice covers its
  task. For a delete, getting this wrong means the files are already gone and
  the surviving task is reported as removed.
- Credentials are redacted in a hand-written `Debug`. Keep it that way:
  `SynoClient` derives `Debug` and holds them, so one `{:?}` would otherwise put
  a password in the log file.
- **The `sid` is a bearer credential and is kept out of the log by two separate
  rules.** `SynoClient` derives `Debug` and prints its `sid` field, so never
  `{:?}` a `SynoClient`; and every non-login request carries `_sid=` in its query
  string, so `error::Error`'s `From<reqwest::Error>` strips the query out of the
  message reqwest builds (`" for url (…)"`) at the single boundary where a
  transport error enters the crate. Do not render a `reqwest::Error` any other
  way, and do not log a raw request URL with its query attached. The log file and
  the session cache are both `0600`, and the cache directory `0700`, because a
  formatting mistake here is a credential on disk.

### Two DSM shapes that are easy to get wrong

- **`delete` / `pause` / `resume` report failure per task, not in the
  envelope**: `{"success": true, "data": [{"id": …, "error": 544}]}`. Always run
  the result array through `download_station::check_task_results`; reading only
  `success` reports a failed delete as a success, and the delete ordering depends
  on knowing whether a pause actually happened.
- **`getinfo` distinguishes four answers, and so does `PathInfo`**: `Missing`
  (skip the file phase, still delete the task), `Found`, `Error(code)` and
  `Unknown` (nothing attributable to the path — see below) —
  a permission problem is *not* absence, and collapsing the two would delete the
  task and strand the files. `Missing` arrives as a per-entry code 408, as a bare
  entry with no `isdir`, or at the envelope level depending on the DSM build;
  `classify_getinfo` is pure and covers all three.

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
  lists the ten documented variants, and `Ord` follows declaration order (that
  is the status sort).
- `TaskFile` is an **object** (`filename`, `size`, `priority`, `selected`), not a
  string — the file list is what the delete-path resolver reads.
- `progress()` / `ratio()` / `eta()` all guard their denominators — a zero-size
  task is ordinary, not an error.
- `list_tasks` always requests `limit = -1`. The poller reconciles the whole list
  each tick; paging would make the cursor/selection reconciliation lie.

### The task-list fixture

`tests/fixtures/task_list.json` is a full `list` envelope covering every known
status plus an unknown one (`captcha_needed`), missing/partial `additional`
blocks, an empty file list, a non-BT download, a zero-size task, a CJK title, an
emoji title, a file list with **no common root** (`delete.rs` rule 2, the
title-named container)
and a `/volume1/...` destination. Numbers appear in both the JSON-number and
string forms on purpose. It drives the `model.rs` parser tests, the layout tests
and the offline `--fixture` mode.

**Its shape came from a real DSM 7 capture; its content is synthetic.** Every key
name, nesting level and value type is the real one — the titles and filenames are
invented so the file can be public, and the real library it was captured from
stays private. `_comment` at the top says so, and lists the three deliberate
departures (string-encoded numbers, omitted `additional` sub-blocks, and the
status/type variety the captured library happened not to contain).

⚠️ **Do not re-capture straight over it.** `syno-clean --dump-tasks-json` emits
real torrent titles, and this repository is public. Take the *shape* from a
capture and keep the content synthetic;
`model.rs::the_fixture_still_carries_only_keys_a_real_nas_sends` is the guard
against invented keys creeping back.

That guard exists because the fixture had three of them before the first real
capture: `selected` on a file entry (the real key is **`wanted`** — `TaskFile`
deserialized a name DSM has never sent, silently defaulting it on every NAS),
`priority` on the detail block, and `status_extra` on the task. A real entry also
carries `index` and `size_downloaded`, which are deliberately not modelled
because nothing reads them.

## The dangerous part: delete ordering and path safety

Deriving "which directory holds this torrent" is the one place this tool can
destroy the wrong data. Everything here is built to **refuse rather than guess**,
and the bulk of the test suite lives in `delete.rs` for that reason.

### Path resolution (`delete::resolve_delete_target`)

**The on-disk name is the task's `title`, always.** Download Station names what
it writes after the task — a container directory for a multi-file torrent, and
for a single-file torrent the title *is* the filename. The BitTorrent spec
agrees: `info.name` is the directory for a multi-file torrent and the file name
for a single-file one, and DSM reports `info.name` as the title.

The file list **never contains that container**, so it cannot name the payload.
It says what *shape* to expect:

| file list | expectation |
|---|---|
| one entry, no separator | `ExpectedKind::File` — single-file torrent |
| anything else non-empty | `ExpectedKind::Dir` — Download Station made a container |
| empty, on HTTP/FTP/NZB/eMule | `ExpectedKind::AnyFromTitle` — accept either |
| empty, on **BitTorrent** | **REFUSE** — a torrent always has a list, so its absence means the record is not understood. `TaskType::file_list_is_mandatory` draws the line, and only `bt` is on the strict side: an unrecognized type is *not* assumed to be a torrent, since that would strand tasks over a string. `--no-delete-files` still removes it. |

⚠️ **This used to take the file list's common top-level component as the name,
and that was wrong in a way that could delete the wrong directory** — not merely
fail. Measured on a real DSM 7 NAS, 41-task library:

- `{destination}/{title}` existed with the expected kind for **40 of 40** tasks
  that had a destination and a file list.
- The old rule **refused 15** of them (entries shared no top-level component) —
  37% of the library, undeletable.
- For **2** more it produced a wrong path: two Blu-ray torrents list `BDMV/…`,
  so it aimed at `/video/BDMV` when the payload is `/video/{title}/BDMV/…`.
  Nothing was at `/video/BDMV` so they failed safely — but an unrelated
  `/video/BDMV` is what would have been recursively deleted.
- It agreed with the current rule on the other 38 only because a single-file
  torrent's title *is* its filename.

There is therefore **no name provenance any more**. `NameSource` is gone; how to
read an *absent* path is decided by the task's counters (`payload_should_exist`),
not by which rule named it. `DeleteItem::named` survives only to mark a refused
item, whose absent path authorizes nothing whatever the counters say.

4. Normalize `destination`: strip a leading `/volumeN`, trim surrounding
   slashes. Join as `/{destination}/{name}`.

Resolution also records an `ExpectedKind` — `Dir` for a multi-entry file list (or
one entry with a separator), `File` for a single flat entry, `AnyFromTitle` for
the title fallback, where DSM says nothing about the shape, and `Indeterminate`
for a file list that *does* say something and it does not describe a payload
(flat entries repeating one identical filename), or for a refused item. The
semantic guard refuses a path whose `isdir` disagrees. **The two "not knowable"
variants are opposite answers, and that is the point:** no metadata to consult
is a reason to accept what is there, metadata that contradicts itself is a
reason to refuse. Never collapse them back into one permissive `Unknown` — that
let a malformed file list authorize the recursive delete the file list exists to
constrain.

Supporting rules, each of which exists because the alternative is a guess:

- `common_root` compares components **exactly** — the NAS filesystem is
  case-sensitive, so `Some.Release/` and `some.release/` are two directories.
- An entry with an **empty or absolute `filename` makes the whole list
  unresolvable**, rather than being skipped: splitting
  `/volume1/downloads/X/a.mkv` naively would report `volume1` as the shared root.
- **A deselected file still counts towards the common root.** `selected`
  describes what was downloaded, not what is on disk, and filtering to selected
  entries would *resolve* some of the ambiguous cases rule 2 exists to refuse.
- **Only an absolute destination has its mount point stripped**, and *every*
  DSM mount spelling counts: `/volume1`, `/volume`, `/volumeUSB1/usbshare1-2`,
  `/volumeSATA1/…` — anything whose first component starts with `volume`, since
  on DSM the first component of an absolute path is always a mount point. A
  relative `volume1/downloads` passes through untouched (a share may legally be
  named `volume1`), and so does an absolute first component that is *not* a
  mount (`/downloads` is share-rooted and already correct). Leaving `/volumeUSB1`
  in place used to build a path File Station has never heard of, and "it fails
  the existence check later" is **not** harmless: absence is one of the answers
  the executor may read as "already cleaned up".
- **An empty normalized destination is refused with its own reason**, because
  `/{name}` names a *share*, and "the task reports no destination" is the message
  the user can act on.

### Syntactic guards (`delete::validate_path`, `delete::validate_name`)

A resolved path is refused — leaving the task **entirely** untouched — if it is
empty, not absolute, `/`, has fewer than two components, contains a `..` or `.`
component anywhere, or has an empty component. Two further guards were not in the
original plan and exist because each turns a merely wrong path into a
*share-destroying* one if anything downstream normalizes it:

- **no control characters** — a NUL truncates the path in any C-based consumer,
  so `/downloads\0/Some.Torrent` arrives as `/downloads`, the share root;
- **no blank (whitespace-only) components** — if any layer trims,
  `/   /Some.Torrent` collapses to `/Some.Torrent`, again a share root.
  Incidental leading/trailing spaces *inside* a real name are left alone: those
  are legitimate on the NAS, and refusing them would skip real torrents.

There is deliberately **no glob guard**. File Station's `path` is a literal path
(searching is a separate API), while scene release names contain brackets
constantly — the guard would refuse most real torrents to defend against a
behaviour DSM does not have.

The on-disk name is guarded **separately, before it is joined**
(`validate_name`): it must be a single non-blank component that is not `.`/`..`
and holds no control characters. `validate_path` would catch most of it
afterwards, but a `title` fallback of `Some/Release` passes every path guard
while aiming one level deeper than the task's own directory.

### Semantic guard

`SYNO.FileStation.List` `getinfo` runs against the resolved path before any
recursive delete, and **existence alone does not authorize it**:

- **found, kind matches `ExpectedKind`** ⇒ delete (`AnyFromTitle` matches both:
  rule 3 had nothing to go on, and the kind found is logged);
- **found, kind disagrees** ⇒ *fail* the item. The path is not this task's
  payload, and the delete is recursive;
- **found, `ExpectedKind::Indeterminate`** ⇒ *fail* the item. The file list was
  consulted and describes no payload, so there is nothing to check the object
  against and a malformed answer must not be what authorizes a recursive delete;
- **not found** ⇒ report *skipped* and still delete the DSM task — but only when
  nothing says the payload must be there. It must be there for a finished,
  seeding or extracting task, for one whose counters say it downloaded
  everything (`payload_should_exist`, which status alone answers wrongly for a
  task paused at 100% or errored after completing), and for any path named from
  the *title*. Those *fail* instead, so the row survives to point at the data;
- **error** is not absence — see `PathInfo` above.

The state those questions are asked of is what the pause phase folded
(`pause_and_confirm` hands back a `PauseRead`), never `DeleteItem` on its own:
that snapshot is frozen when the dialog opens and can be minutes stale mid-batch.

**A `PauseRead`'s two halves both ratchet toward "the payload must exist", by
different evidence.** Every read of the task — **the dialog's snapshot, which
seeds the fold**, the one before the pause, and each one confirming it — is
folded into both halves under the same monotonic rule: a later read may move the
answer toward *must exist*, never away from it. The *status* advances when a read
reports one `delete::status_implies_payload` accepts, so a task that reaches
`Finished`/`Seeding`/`Extracting` while the pause settles is judged as finished
rather than from its stale pre-pause status; and because `Paused` is not such a
status, no read can walk a `Seeding` back into a state whose absent payload the
check waves through — not this program's own pause, and not a task DSM stopped
between the dialog and its turn in the queue. The *counters* (`downloaded`/`size`)
advance to the freshest values seen, except that a read which said "complete" is
never walked back: pausing does not un-download anything, and a task that reaches
100% while the pause takes effect is exactly the case where stale-low counters
let a missing path be judged benign.

**Seeding the fold with the snapshot is the whole design, not an optimization.**
The snapshot is simply the earliest read, so it belongs *inside* the ratchet
rather than as a fallback chosen against it — a seeding torrent with unselected
files (`downloaded < size`, so only the status proves anything) that DSM stopped
before its turn came round is the case that composing the halves "live if
present, else snapshot" got wrong. `payload_for_file_phase` therefore chooses
nothing; it unwraps the folded value, and falls back to the raw snapshot only
when no pause phase ran at all. Do not reintroduce per-half preference, and do
not "simplify" either half into "whatever the last read said".

### Snapshot semantics

The `DeletePlan` is an **owned** snapshot taken when the confirmation dialog
opens. Task-list refreshes are suspended while `Mode::Confirm` is active, and
**`validate_path` is re-run immediately before every File Station call** on top
of having run at snapshot time — the value crossed a task boundary in between and
the next call has no undo. What the user read on screen is exactly what gets
deleted.

### Three phases, ordered for recoverability

| Task status | Ordering |
|---|---|
| Downloading, Seeding, Waiting, Finishing, HashChecking, Extracting, FilehostingWaiting, `Unknown(_)` | pause → confirm paused → delete files → delete task |
| Paused, Finished, Error | delete files → delete task |

- Any phase failing **skips all later phases**. The task then survives still
  pointing at its data — nothing is orphaned.
- The DS API removes the task but never the payload; files go via
  `SYNO.FileStation.Delete` `start` + `status` polling (a recursive delete of a
  big torrent directory can outlive the HTTP timeout). `finished: true` is not on
  its own success — `classify_delete_status` fails the phase when
  `path_err_num > 0`.
- For **incomplete** tasks, "path not found" during the file phase counts as
  success — Download Station cleans up its own partial data. For a task that
  **finished** (`delete::payload_should_exist`: Finished, Seeding, Extracting)
  it does not: the payload demonstrably existed, so an absent path means the
  resolved *location* is wrong far more often than it means the files are gone,
  and deleting the DSM row on that evidence orphans them. That item fails and
  keeps its task.
- **"Confirm paused" is a real re-read** (`download_station::task_info`, polled
  until the task reports itself paused, bounded at 15 s). DSM accepting a `pause`
  says the request was queued, not that the task stopped writing. The answer is
  matched to **the id that was asked about** (`event::task_with_id`, never
  `.first()`), and an answer carrying no entry for it means **pause**, not
  "idle": `TaskList::tasks` is `#[serde(default)]`, so an unreadable payload
  arrives as no entry at all, and fail-open there recurses through a directory
  Download Station may still be writing into.

### How the ordering is expressed (`delete.rs` + `event.rs`)

- The **rules are pure**: `delete::plan_delete_ops(&DeleteItem, DeleteOptions)`
  is the table above as data, and `delete::ops_cancelled_by(ops, failed_at)` is
  the "a failed phase cancels every later phase" half. Both are unit-tested with
  no network. `event::spawn_delete` is the I/O and the accounting around them —
  **keep new ordering logic in `delete.rs`, not in the executor.**
  `DeleteOptions` is a *parameter* rather than a field of `DeletePlan` because
  `delete_files` / `dry_run` are session state with a different lifetime from the
  snapshot. `DeleteItem::status` is carried for *display* only — see below.
- `delete::requires_pause` treats **everything except Paused / Finished / Error
  as active**, which is where `filehosting_waiting` and any `Unknown(_)` land:
  pausing an idle task costs a round trip, not pausing a live one risks DS
  writing into the directory mid-delete.
- ⚠️ **`requires_pause` is only ever applied to a live status.**
  `plan_delete_ops` emits `Op::Pause` unconditionally whenever files are being
  deleted, and `event::pause_and_confirm` re-reads the task with `getinfo`
  before deciding. `DeleteItem::status` is a snapshot as old as the confirmation
  dialog plus the item's place in the batch queue: filtering on it meant a task
  DSM's bandwidth schedule resumed mid-dialog was never paused, and File Station
  then recursed through a directory DS was writing into. The live read also
  skips the pause *call* for a genuinely idle task, so DSM's "already paused"
  per-task error cannot fail an otherwise good delete.
- **An absent path is only benign when something explains the absence**
  (`event::decide_file_phase`). Three inputs decide it, **and the order they are
  asked in is load-bearing**:
  1. `event::DeletedPaths` — the set of paths *this process* already deleted
     successfully. This is what keeps the strictness below from becoming a trap:
     an item whose files went but whose post-delete re-check could not be made
     fails and keeps its task, and the obvious retry would otherwise hit the
     refusal for ever. Record the fact where it is known (the delete returning
     success) rather than reading a failed lookup as a success.
  2. `delete::payload_should_exist` — the ratcheted status *and* counters. If the
     task never wrote a payload, there is nothing on the volume to orphan and the
     absence is benign.
  3. only then `DeleteItem::name_source` (`FileList` or `Title`). A name guessed
     from the display title is at least as likely to have missed as to have been
     tidied up — but **only if there was a payload to guess at**. Asking
     provenance first made every non-BitTorrent task (HTTP/FTP/NZB/eMule all
     resolve their name from the title) that had downloaded nothing permanently
     undeletable: `Missing` + `Title` hard-failed, the task delete was cancelled,
     and every retry did the same. Keep the hard failure for a title-named path
     that is missing when the payload *should* be there; do not reinstate it for
     one that never existed.

  A path with no provenance at all (`name_source: None`, a refused item, which
  resolves no path and so cannot reach the file phase) is still refused outright
  ahead of all of this: nothing named it, so nothing authorizes acting on it.
- **The existence check has four answers, not three.** `PathInfo::Unknown` is a
  `getinfo` response carrying no entry attributable to the requested path — an
  empty `files` array (which is what a shape this client cannot parse produces,
  since the field is `#[serde(default)]`), or several entries none of which
  match. It must never collapse into `Missing`: that would report "the files
  were already gone" for every item of a batch and delete every task while
  reclaiming nothing. For the same reason `classify_getinfo` only borrows a
  non-matching entry when it is the *only* one.
- **The recursive delete is confirmed by a second `path_info`**, not by
  `path_err_num` alone. That field is `#[serde(default)]` and no real NAS
  response has been captured to check the spelling, so a rename would make a
  delete that removed nothing look finished and clean.
- ⚠️ **One `getinfo` answer means opposite things before and after the delete**,
  and `event::decide_file_phase` / `event::decide_confirm_phase` are deliberately
  *not* one function. That answer is **`Unknown`, and only `Unknown`**. Before:
  a hard failure — nothing has been touched yet and an unreadable response must
  not authorize a delete. After: acceptable — `confirm_deleted` is only ever
  reached through a `Found` on the same call for the same path, so the shape
  demonstrably parses on this NAS and an entry that has stopped being
  attributable is a path that has stopped being there. Demanding a positive
  `Missing` there made every item of every run half-complete (files gone, task
  kept, footer reporting FAILED) on any DSM build that answers an absent path
  with `{"files": []}`.
- ⚠️ **The relaxation stops there.** After the delete, `Found` fails, an
  `Error(code)` fails (a readable "I could not look" — the shape a directory
  holding one undeletable entry produces — which says nothing about the path
  being gone), and a `path_info` call that **errors outright** fails too. Only
  `Missing` and `Unknown` confirm. A failed re-check used to count as
  confirmation; that deleted the DSM task on the strength of an answer that never
  came. The retry-deadlock this was protecting against is handled by
  `DeletedPaths` instead.
- **`delete_files = false` drops the file phase *and* the pause.** The pause
  exists only to keep DS out of the way of the file delete; with no file delete a
  failed pause would block a task-only removal for nothing.
- **A refused item (`Target::Refused`) gets an empty op list *while files are
  being deleted*** — not even the DSM task goes. The dialog showed the row as
  SKIPPED, and removing the task would orphan precisely the data whose location
  is in doubt. **Under `--no-delete-files` it is an ordinary deletable row**: no
  path is used, so the refusal protects nothing, and those tasks (no destination,
  or a torrent with no file list at all) are exactly the ones that flag
  exists for — refusing them there left them unremovable by this tool by any
  route. `delete::will_act` is the single rule; `ui::dialog` reads it too, so the
  dialog and the executor cannot disagree about which rows are skipped.
- **`--dry-run` issues no call at all** — not even the read-only `getinfo`
  existence check — and covers **pause and resume as well as delete**. Every
  phase logs what it would do and the item is reported as a *skip*, never a
  success, so the footer can never read "3 succeeded" for a run that changed
  nothing. A flag that promises the NAS is untouched has to mean it.

## UI and state conventions

- `App` holds all state; **rendering is a pure function of `&App`**. That is what
  makes every frame assertable with `ratatui::backend::TestBackend`, which draws
  into an in-memory buffer with no TTY.
- Sorting/filtering produce a `Vec<usize>` of indices into the task list rather
  than cloning or reordering the source data.
- **Cursor and selection are keyed by task ID, not row index**, so a refresh
  that reorders or removes rows never silently reassigns what is selected.
  `App::cursor` is a position in the *visible* list; `App::apply_tasks` and
  `App::change_view` do the reconciliation.
- **`App` holds no runtime handle and performs no I/O.** Every key press is a
  pure state transition. Anything needing the network is *parked* for the event
  loop to drain — `take_refresh_request`, `take_confirmed_delete`,
  `take_requested_op` — which is the single reason the whole keymap and the whole
  delete flow are testable without a tokio runtime or a NAS.
- `App::handle_key` ignores anything that is not `KeyEventKind::Press` —
  Windows and the kitty protocol report releases too, and acting on both halves
  runs every binding twice. `Ctrl-C` is handled before the mode dispatch so it
  works from inside a modal.
- **`App::error` is not `App::status_message`.** The error banner is cleared
  automatically by the next successful refresh and rendered red with `⚠`; the
  status message survives underneath and returns when the banner clears.
- **The footer is one line and cannot hold a reason.** Every per-item outcome
  that lands in it is overwritten by the next one, and the `OpDone` summary that
  replaces them all carries counts only. So `AppEvent::OpProgress` carries an
  `event::ItemReport` (title + `ItemOutcome`) as *data*, not a preformatted line:
  `App` writes the footer from it **and** keeps the non-successes, folding them
  into an `app::OpReport` when the batch finishes. That is what the results modal
  (`Mode::Results`, `v`) lists. A message that still will not fit is elided by
  `ui::fit_footer`, which drops the sort and then the selection before it
  truncates the message, and marks the truncation with an ellipsis.
- **A modal is never replaced under a user who is reading one.** The results
  modal auto-opens on a batch with problems, but only from `Mode::Normal`; the
  report is kept either way and `v` reaches it later. `Mode::Results` does *not*
  close on any key the way `Mode::Help` does — it scrolls, so `j` and `k` have to
  mean something — and it blocks no refresh, because it describes what already
  happened rather than a snapshot the list could invalidate.
- **Refusal reasons are wrapped, never truncated** (`format::wrap`, used by both
  modals). The remedy a refusal names (`--no-delete-files`) is at the *end* of the
  sentence and the modal is capped at 82 cells however wide the terminal is, so
  truncating put it out of reach at every terminal size. The confirmation and the
  results modal also both carry `dialog::SKIP_REMEDY` as a standing line that
  cannot scroll away, and the `?` overlay names the flag in its footer: it is the
  one escape hatch that is not a key, and there is deliberately **no in-app
  toggle** for it — a key that silently re-aims a delete at "task only" is worse
  than a restart.

### Selection

- **`a` (`toggle_select_all_visible`) touches only the visible rows**, in both
  directions — a filtered-out task is never armed for deletion by a key press
  the user aimed at what was on screen, and never quietly *un*armed either. On a
  partially selected visible set it selects the rest rather than clearing: a key
  that sometimes discards a half-built selection is the one that loses work.
- `Esc` (`clear_selection`) is the opposite and clears everything, hidden rows
  included; it is the "I do not know what is armed" key.
- **Space does not advance the cursor.** With `d` acting on the selection, a
  cursor that drifts is how the wrong row ends up under a later un-selected `d`.
- The selection footer counts and sums **`App::selected_tasks()`** — the selected
  IDs that still name a real task — not `selected.len()`. Between a task
  vanishing on the NAS and the next refresh pruning the set, the raw length would
  over-report while the size sum did not.
- **One definition of "the current target"**: `App::target_tasks` — the selection
  when there is one, the row under the cursor otherwise, nothing at all when the
  table is empty. `d`, `p` and `u` all go through it, and a selected task a filter
  is hiding is still included (the selection is what is armed, not what is on
  screen).

### Sort, filter and search (`view.rs`, `App::change_view`)

- **Every view change goes through `App::change_view`** (`s`, `S`, `f`, and
  every keystroke in the search box). It follows the cursor's task by **ID**
  through the re-sort or re-filter, falls back to holding the row number when
  the change hides that task, and then clamps — the same rules `apply_tasks`
  uses, for the same reason: a cursor that lands on a different torrent is how
  the wrong thing gets deleted.
- **A view change never touches the selection.** A filter is a question about
  what to look at, not an instruction to disarm rows that scrolled off screen.
- **Descending reverses the `Ordering`, never the result `Vec`.** `sort_by` is
  stable, so reversing the comparison preserves the incoming order of ties in
  *both* directions; reversing the vector would shuffle ties on every `S`.
- `f64` keys use **`total_cmp`**, never `partial_cmp().unwrap()` — a `NaN` must
  not panic mid-frame. Name comparison and search fold case over **iterators**,
  not with `to_lowercase()` per comparison, because the comparator runs
  O(n log n) times per re-sort.
- **`StatusFilter::Downloading` means "in progress"**, covering `downloading`,
  `waiting`, `finishing`, `hash_checking`, `extracting` and
  `filehosting_waiting`. Five filters exist against ten statuses; exact matching
  would leave five statuses reachable only under `All`, silently hiding rows from
  someone who believes they are filtering. The other four filters are exact.
- **An `Unknown(_)` task is visible only under `All`** — it cannot be classified
  without guessing, and filing it under `Error` would mislabel a healthy task.
- **Search matches live, on every keystroke**, so `Enter` *commits* rather than
  applies. The query being edited is `view.search` itself; `App::search_backup`
  holds what it was when `/` was pressed and is the only way `Esc` can undo an
  abandoned edit. `/` deliberately keeps the committed query so a search can be
  refined rather than retyped.
- **In `Mode::Search` every printable key is text**, never a binding — a box that
  cannot type `q` cannot search. Only `Enter`, `Esc`, `Backspace` and the global
  `Ctrl-C` are commands; `Ctrl`/`Alt` chords are dropped rather than typed, and
  `Shift` is not (it is how a capital arrives). Backspacing past the start is
  inert, not an exit.
- **`Esc` is mode-specific**: cancel-and-restore in `Mode::Search`, clear the
  selection in `Mode::Normal`. Keep both halves correct when adding modes.

### The task table (`ui::table`)

- The table is laid out **by hand**, not with ratatui's `Table` widget: every
  cell is truncated and padded through `format::truncate_ellipsis` /
  `display_width` so a CJK or emoji title cannot shear the columns to its right,
  and each row is emitted as one pre-composed `Line`. A widget that measures
  differently cannot be made to agree.
- `COLUMNS` is the single definition of the column order, headers, fixed widths
  and alignment. **Name is the only flexible column** — it absorbs all the slack
  down to `MIN_NAME_WIDTH`; on a terminal narrower than `ideal_width()` the
  rightmost columns are clipped by the buffer, because responsive column
  *dropping* is deferred past v1.
- Column headers are spelled exactly like `view::SortKey::label()`, and the sort
  marker is placed by comparing the two. Do not introduce a second key→column
  mapping. `SortKey::Added` has no column and so shows no marker.
- The selection marker (`SELECTED_MARKER`) is asserted to be **exactly one cell
  wide**; a two-cell glyph there would shear every column to its right on
  selected rows only — the hardest layout bug in this table to spot by eye.
  Selection is a *colour*, the cursor a *reversal*, so all four combinations read
  differently.
- Long statuses are shortened to fit the column (`hash_checking` → `checking`,
  `filehosting_waiting` → `hosting`); an **unknown status is rendered verbatim**
  and coloured magenta, never renamed.
- **Scrolling is edge-triggered, and the offset is stored but re-clamped on
  every use** (`App::scroll`, `App::scroll_offset`, `table::scroll_offset`). The
  window moves only when the cursor would leave it. Deriving the offset from the
  cursor alone cannot express that — for any `cursor >= height` there is exactly
  one answer, `cursor - height + 1`, which welds the cursor to the bottom row, so
  no row below it is ever visible and one `Up` slides the whole table. The old
  rationale (a derived value cannot fall out of step with a refresh-moved cursor)
  is kept by clamping the stored offset against the *current* cursor, row count
  and viewport height at every read: a stale offset self-corrects. The event loop
  pushes the body height in via `App::set_page_size` after each draw, and
  `table::render` re-clamps against the real height of the frame it is drawing.
- Cursor movement **clamps and never wraps** — a `j` held at the bottom of a long
  list wrapping to the top is how the wrong row gets deleted.

### The delete confirmation (`ui::dialog`, `App::begin_delete`)

- **`d` never deletes.** It snapshots (`DeletePlan::snapshot`) the target and
  opens `Mode::Confirm`. An empty plan opens no dialog at all.
- **Cancel is the default focus** (`ConfirmFocus::default() == Cancel`), so
  `Enter` on an untouched dialog *cancels*. `y` is the deliberate one-key
  confirm; `n` / `Esc` / `q` cancel; `q` closes the dialog rather than the
  program (`Ctrl-C` still quits). Every unrecognized key does nothing — never
  "defaults to confirming".
- **Refused items are rendered, never dropped**: `Target::Refused` shows as
  `SKIPPED` with its reason, in snapshot order (so a dialog row maps to the row
  the user selected), carries **no size** — a number beside a skip reads as bytes
  about to be freed — and is excluded from the total.
- **The totals line changes wording with `delete_files`**: "to free" when the
  files go, "left on disk" when only the task does. Reporting "to free" for a
  task-only delete would be the single most misleading number the program could
  print. `--dry-run` is stated in the border title *and* the effect line, with a
  yellow border instead of red.
- `build_confirmation(&DeletePlan, DeleteOptions) -> ConfirmSummary` produces
  plain strings and counts and is where the wording and the arithmetic are
  tested; `render_confirm` only draws. Modal scroll is clamped in `App` against
  the line count and again at render against the height — the same split as
  `ui::table`'s derived scroll offset.

### The help overlay (`dialog::HELP_SECTIONS`)

- It is the keymap's public face, and it is **data, not formatted text**: a test
  tokenizes every `keys` field and asserts each key in a **hand-written literal
  list** appears there. That list is *not* derived from `App`'s match arms — it
  mirrors `handle_normal_key`, `handle_search_key` and `handle_confirm_key` by
  hand. A new binding therefore lands in three places: the handler,
  `HELP_SECTIONS`, and the list in
  `dialog::tests::the_overlay_documents_every_key_the_app_binds`. Forget the
  overlay and the test names the key; forget the test's list and nothing does.
- The *implementation* is the source of truth for what it says (notably: `Enter`
  presses the focused button in the confirmation, and commits an already-live
  query in the search box). The README transcribes the overlay and says so, so
  there is no second copy to drift.
- The overlay binds nothing itself: **any key closes it and does nothing else**,
  so dismissing with `d` cannot also open a delete confirmation. It lays out in
  two columns and drops its inter-section blank lines rather than clipping — the
  whole card has to fit **80x24**, and two tests pin that.

### Empty states

Chosen by `App::tasks` being empty, **not** by whether the view narrows
anything: with zero tasks and a filter set both are true, and blaming the filter
sends the user pressing `f` at a NAS that has nothing to show. `View` therefore
has no `is_narrowed` predicate — the only caller it could have had is the one
place it must not be used. The narrowed state names how many rows are hidden and
by what (`ui::narrowing_summary`), so the fix is on screen rather than guessed
at.

### Terminal lifecycle (`ui`)

- `ui::TerminalGuard::new()` is the **only** place raw mode and the alternate
  screen are entered, and its `Drop` the only place they are left. It owns the
  `Terminal`, so a drawable terminal cannot outlive the restoration, and every
  exit path — clean quit, error out of the loop, unwinding panic — restores.
  Errors in `Drop` go to the log; there is nowhere else to put them.
  `new()` also unwinds its own partial setup, so a half-failed startup never
  hands back a raw-mode terminal with no program left to read keys.
- `ui::install_panic_hook()` **chains** to the previous hook rather than
  replacing it (the backtrace must still print) and is `Once`-guarded so a
  double install cannot nest. Install it *before* constructing the guard.
- Non-TTY stdout is a clean failure, not a corrupted terminal:
  `TerminalGuard::new()` returns the `enable_raw_mode` error and `main` prints
  an actionable message and exits non-zero, having written nothing to stdout.
- The caret in the search box is a **glyph** (`ui::SEARCH_CARET`), not the
  terminal cursor, so `render` stays pure and the cursor stays hidden for the
  whole session instead of being shown and hidden per mode.

### The event loop and the poller (`main.rs`, `event.rs`)

- The loop is `draw → select!(terminal event, AppEvent) → apply`.
- The input source is a `spawn_blocking(event::read)` reader, because crossterm's
  `event-stream` feature is unavailable through ratatui's re-export. **The
  pending read lives in a variable outside the loop** (`pending_read:
  Option<JoinHandle<_>>`) and is only cleared when it resolves: a blocking read
  cannot be cancelled, so re-creating it per iteration would spawn one orphaned
  stdin reader per poller tick and they would then take turns eating the user's
  keystrokes. Exactly one read is ever in flight. The `select!` yields a `Next`
  enum rather than acting in its branch bodies, so the mutable borrow ends with
  the expression.
- **Everything that touches the network runs off the loop** and reports through
  the single `mpsc` channel of `event::AppEvent`. There is no `Tick` variant:
  the poller drives data, and data drives redraws.
- **The poller is non-fatal.** A failed tick becomes `AppEvent::Error` and the
  interval keeps running; the next successful tick clears the banner. Never
  `return` out of the poller on a poll failure — a NAS that vanishes for a
  minute is ordinary. It ends only when the channel closes or `main` aborts it,
  which `main` does after the loop so an in-flight 30 s HTTP timeout cannot delay
  exit.
- **Quitting stops the poller *before* waiting for in-flight op batches, and
  keeps draining the channel while it waits** (`main::shutdown`). The poller and
  the op tasks share one `mpsc` of 64 slots; a poller still ticking into a
  channel the loop no longer drains fills it, and the delete's next
  `OpProgress` send then blocks forever — a hang with the terminal already
  restored, escapable only by the `Ctrl-C` that abandons the batch between "the
  files are gone" and "the task is gone". The wait is bounded
  (`main::IN_FLIGHT_GRACE`) for the same reason.
- ⚠️ **Every exit from the loop reaches that wait, `?` included.** The loop body
  lives in `main::event_loop` and `run_tui` captures its result: a failed
  terminal write (SSH dropped, window closed, resize race) used to return past
  the wait entirely, tearing the runtime down on top of whatever item the batch
  was on. Do not `?` out of `run_tui` before `shutdown`.
- **`IN_FLIGHT_GRACE` bounds *silence*, not the batch.** Every drained
  `OpProgress` restarts the clock and is echoed on stderr, so a twenty-item
  delete gets twenty grace periods rather than one and the user watches it
  drain. On expiry the process names the last item it saw and **exits non-zero**;
  a batch cut mid-item is not a successful run.
- **One op batch at a time, refused in `App` rather than in the loop.** The loop
  pushes `App::set_op_in_flight` before each draw and `d` / `p` / `u` say no on
  the spot. Refusing where the plan is *drained* meant
  `take_confirmed_delete` had already consumed it, and the footer line saying so
  was overwritten by the running batch's next progress event — the user saw a
  delete they confirmed report as finished having never run.
- **`r` is a request, not an action.** The loop drains it and pokes an
  `event::RefreshHandle` (an `Arc<Notify>`, so repeated presses coalesce into one
  poll); the poller `reset()`s its interval after a manual tick.
- `App::apply_event` is the `AppEvent` counterpart of `App::handle_key` — same
  shape, same testability. Keep reconciliation there, not in `main`.
- `App::apply_tasks` invariants: the cursor follows its **task ID** through a
  reorder; a cursor task that vanished holds its *row number* and clamps; stale
  IDs are pruned from the selection; and a `Tasks` event arriving in
  `Mode::Confirm` is **dropped entirely** — before touching anything, banner
  included — so the delete plan on screen cannot go stale while it is being read.

### Op tasks (`event::OpContext`)

Anything long-running gets an `OpContext { client, tx, refresh }`, runs off the
loop, reports per item with `AppEvent::OpProgress` and once at the end with
`OpDone`, and then pokes `refresh` — **one refresh per batch, not per item**.
`App` renders the report through `app::op_summary`, which names only the non-zero
categories and prefixes `⚠` when anything failed. **The report goes in
`status_message`, never the error banner**: the batch's own refresh would clear
the banner a moment after it appeared.

- Delete runs **per item**, because its *ordering* is per item.
- Pause and resume are **one round trip for the whole batch**:
  `event::spawn_task_op` sends the comma-separated id list once and derives
  per-item outcomes from the per-task result array (`task_op_outcome`, which runs
  each entry through `check_task_results`). An id DSM reported nothing for counts
  as a **failure** — the refresh that follows shows the truth, and a false
  "paused" is the answer the user cannot correct.
- Neither pause nor resume is confirmed by a modal: each is undone by the other
  key, and a modal in front of a reversible operation only trains the user to
  dismiss modals.
- `spawn_task_op` takes `TaskOp`, a two-variant enum (`Pause` / `Resume`) with
  **no `Delete` variant**, so a delete asked for here is unrepresentable rather
  than a runtime check. A delete carries an ordering and belongs to
  `spawn_delete`; an unreachable `match` arm here would have answered with an
  empty result array and reported every item of the batch as "DSM reported no
  result for this task".

## Formatting

- Sizes are **binary** (1 KiB = 1024 B) because that is what DSM reports.
  `B`/`KiB` print as whole numbers, `MiB` and up get one decimal. The unit is
  picked *after* rounding, so nothing ever renders as `1024 KiB`.
- **Zero and unknown are different sentinels.** `speed(0)` is `DASH` (`—`) — the
  task is idle, not unknown; an ETA that cannot be computed is `INFINITY` (`∞`).
  Do not collapse them into `0`: rendering both as `0` tells the user a paused
  task is about to finish.
- `duration` renders **at most two units** (`45s`, `1m 5s`, `2h 14m`, `1d 1h`).
  Seconds of precision on a four-hour download is false detail and costs column
  width. `Some(0)` is a *known* `0s` and deliberately differs from `None`.
- `percent` takes a **fraction** (`0.0..=1.0`), matching `Task::progress()`, not
  an already-multiplied percentage. Out-of-range and non-finite inputs clamp
  rather than surfacing a `NaN` in the table.
- **Never size or pad a column with `str::len` or `chars().count()`.** Use
  `format::display_width` and `format::truncate_ellipsis`, which measure
  terminal cells via `unicode-width`; the fixture's CJK and emoji titles are
  there to keep that honest. `truncate_ellipsis` never exceeds the requested
  width and may stop one cell short rather than clip a double-width character
  in half. Truncation is per `char`, not per grapheme cluster — correct
  segmentation would mean another dependency for a title that was being elided
  anyway.

## Offline and debugging flags

`--dump-api-info` and `--dump-tasks-json` print a raw DSM response verbatim and
exit. They are `hide = true` — debugging aids, not advertised interface.
`--dump-api-info` deliberately does **not** log in, since discovery needs no
session and that is exactly the case where a login is what is broken.

`--fixture <path>` runs the whole TUI over a captured `list` response with no
network call and — deliberately — **no configuration at all**: it short-circuits
`main` before the config merge, so it works on a machine with no config file, no
host and no password. The file is a full DSM envelope, read through the same
`parse_envelope::<TaskList>` the live path uses (`app::parse_fixture`), never a
second, laxer parser: a fixture only the fixture loader can read would prove
nothing about what the NAS sends. It **forces `DeleteOptions::dry_run()`** —
there is no client offline, so a modal promising a real recursive delete would be
lying.

`--fixture` is hidden like the dump flags but is not one of them: `Cli::is_dump()`
stays false, because it enters the TUI rather than printing and exiting.

## Testing philosophy

Deliberately narrow, and that is a decision rather than an omission: TUI
rendering and HTTP plumbing are obvious when broken, while a wrong sort
comparator, a misparsed byte count or a mis-resolved delete path fails quietly
and destroys data.

**Tested:** `format`, `model`, `view`, `error`, `api::client` envelope parsing,
`app` selection/reconciliation and the whole key state machine, and above all
**`delete`** — path resolution, guards and op ordering, the highest-value tests in
the project. Rendering is also covered far beyond the original intent, because
`TestBackend` makes it free.

**Not tested (verified by running the binary):** the terminal lifecycle (raw
mode, alternate screen, panic hook), live HTTP against DSM.

Rules that keep the suite honest:

- **No test touches the network, a real timer, the real `$HOME`, or a process env
  var.** Request construction is extracted into pure `build_*_params` functions
  and ordering into pure `plan_delete_ops` / `ops_cancelled_by` precisely so the
  interesting half needs no I/O. The whole suite runs in well under a second.
- **No mocking framework and no trait abstraction over the HTTP client.** One
  implementation does not warrant one. Where an executor has to be exercised end
  to end, it is pointed at a host that does not resolve — which is also what makes
  "`--dry-run` issued no call" a *positive* assertion rather than an inspection.
- The panic hook is deliberately **not** unit-tested: it is process-global, and a
  test installing one would swallow the output of any test panicking
  concurrently.
- `ui::tests::frame_lines` keeps the space ratatui parks in the continuation cell
  of a double-width glyph (that is what makes its one-symbol-per-cell width check
  correct), so a CJK title read back out of it has a space after every character.
  Use `frame_text_narrow` when asserting that *text* reached the screen — a first
  draft that did not looked exactly like a real bug.

## Known gaps and outstanding debt

Real, deliberate, and none of them a regression:

- **The fixture's wire shape is now real** (see above), which retired the largest
  piece of debt this project had. Still unconfirmed against a NAS: the
  `SYNO.FileStation.Delete` `status` payload in flight — `finished` and
  `path_err_num` were only ever probed with a bogus taskid — which is why
  `confirm_deleted` re-checks the path rather than trusting those fields.
- **A live NAS has now exercised the read paths, discovery, login, list, pause,
  resume and delete**, and the interactive TUI has been driven in a pty. What
  remains unexercised is listed under Post-Completion in the plan.
- ⚠️ **The README's terminal frame is a `TestBackend` rendering of the fixture,
  not a screenshot.** A real capture is still owed before publishing.
- **The repository is `https://github.com/Chekushkin/syno-clean`** — settled, no
  longer a placeholder. It appears in `Cargo.toml`, `README.md`,
  `CONTRIBUTING.md` and `CHANGELOG.md`; keep those four in step.
  `publish = false` still guards against an accidental crates.io release.
- **Eight sort keys against eleven columns.** Seeds/Peers, ETA and Destination
  are not sortable, and `SortKey::Added` sorts by a value with no column. Post-v1
  if it is missed.
- **Responsive column *dropping* is deferred past v1** — a narrow terminal clips
  the rightmost columns rather than rearranging.
- **A share with the DSM Recycle Bin enabled may reclaim no space.** Documented in
  the README; whether to surface a warning in the UI is unresolved and needs a
  real NAS to answer.
- **`--logout` is not suppressed by `--dry-run`.** Deliberate — ending a session
  destroys no data and the flag is explicit.
- **`SYNO.DownloadStation2.Task`** is a possible future migration, not a v1
  concern.
