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
  each other. Normal quit does **not** log out; only `--logout` does.

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

Syntactic guards — a resolved path is refused if it is empty, `/`, has fewer
than two components, contains a `..` component, has an empty or `.` name
component, or lacks a leading `/`.

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

## Testing philosophy

Deliberately narrow. Pure logic where bugs are silent and expensive is tested:
`format`, `model`, `view`, `error`, `api::client` envelope parsing, `app`
selection/reconciliation, and above all **`delete`** — path resolution, guards
and op ordering, which is the highest-value test in the project.

Not tested (verified by running the binary): ratatui rendering, key wiring,
live HTTP against DSM. No mocking framework and no trait abstraction over the
HTTP client — one implementation does not warrant one. Offline verification
uses the hidden `--fixture <path>` flag.
