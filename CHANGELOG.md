# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.1.0] - unreleased

First release: everything below is the initial implementation.

### Added

- Terminal UI listing every Synology Download Station task with name, status, size,
  progress, download and upload speed, ratio, seeds/peers, ETA and destination. Column
  widths are measured in terminal cells, so CJK and emoji titles do not shear the
  layout.
- Keyboard navigation (`↑`/`↓`, `k`/`j`, `PgUp`/`PgDn`, `Home`/`End`, `g`/`G`) and
  multi-select with `Space`, `a` (visible rows only) and `Esc`. The cursor and the
  selection are keyed by task ID, so a background refresh never reassigns them.
- `d` deletes: a confirmation dialog lists each task, its resolved on-disk path and the
  total that will be freed, then the files are removed through File Station and the
  task through Download Station. Cancel holds the focus when the dialog opens.
- Delete-path resolution that **refuses rather than guesses** — a torrent whose file
  list has no single common root is skipped, never resolved from its title — plus
  syntactic path guards and a File Station existence check before any recursive delete.
- Three-phase delete ordered for recoverability: pause an active task and confirm it
  stopped, then delete the files, then delete the task. Any phase failing skips every
  later phase, so a task is never removed while its data survives.
- `--dry-run`, which runs the whole flow and issues no call at all, and
  `--no-delete-files` (`delete_files = false`), which removes the DSM task only.
- Pause (`p`) and resume (`u`) over the selection or the row under the cursor.
- Sort by any column (`s`, `S`), status filters (`f`) that group the in-progress
  statuses together, and live substring search over titles (`/`).
- Background poller with a configurable interval; poll failures show a non-fatal banner
  and clear themselves on the next successful refresh. `r` refreshes now.
- `?` help overlay documenting every binding.
- Configuration from CLI flags, `SYNO_CLEAN_*` environment variables and a TOML config
  file, in that order of precedence, with XDG paths on every platform. Unknown config
  keys warn and are ignored. A first run with nothing configured writes a commented
  template and exits without entering the TUI.
- Session `sid` caching at `~/.cache/syno-clean/session.json` (mode `0600`), keyed by
  host, port and username, with transparent re-login on DSM 106/107/119. `--logout`
  invalidates it; a normal quit does not.
- 2-step verification support, via `SYNO_CLEAN_OTP` or a prompt when DSM asks.
- DSM API version negotiation through `SYNO.API.Info` — no hardcoded versions — and
  DSM error codes rendered as sentences, including the auth-specific 400 range.
- File logging to `~/.cache/syno-clean/syno-clean.log` (`--log-file` overrides), never
  to stdout.

[Unreleased]: https://github.com/emacarov/syno-clean/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/emacarov/syno-clean/releases/tag/v0.1.0
