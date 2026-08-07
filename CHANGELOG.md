# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.1.0] - unreleased

### Fixed

- **A stale session is recovered instead of failing every request.** Download
  Station answers a dead `_sid` with DSM **105** ("insufficient user privilege"),
  not the documented 119, so the transparent re-login never fired and a cached
  session that had expired made every refresh fail until the cache was cleared by
  hand. 105 now earns one re-login — and if a fresh session is still refused, it
  really is a permission problem and the client stops retrying it, so a
  misconfigured account cannot turn the three-second poller into a login every
  three seconds.
- `--dump-tasks-json` writes a renewed session back to the cache, as the TUI
  already did.
- **`TaskFile` parsed a field DSM has never sent.** The model called it `selected`;
  the real key is `wanted`. It had been silently defaulting to `true` on every NAS
  this program has ever talked to. Nothing depended on the value, but the fixture
  had invented two more keys the same way (`priority` on the detail block,
  `status_extra` on the task), and a test now guards against that recurring.
- **The pause-confirmation bound is 60s, not 15s.** Pausing settles in ~0.1s
  normally, but one delete in a run of seven had its pause accepted and the task
  still seeding 15s later — most likely the tracker "stopped" announce that
  stopping a seeding torrent triggers. The wait now also logs once when it stops
  looking instant, because the failing run logged nothing at all for its 15s.

- **The on-disk path is now taken from the task's title, not from the file list's
  common top-level component.** Download Station names what it writes after the task
  — a container directory for a multi-file torrent, the filename itself for a
  single-file one — and the file list is relative to that container, so it can never
  name it. The old rule refused 15 of 41 tasks on a real DSM 7 NAS because their
  entries shared no top-level component, and for two Blu-ray torrents (file list
  `BDMV/…`) it resolved to `/video/BDMV` when the payload was `/video/{title}/BDMV/…`
  — a path that could have belonged to something else. The file list is now used for
  what it can attest: the *shape* to expect, checked against the volume before any
  delete.
- **File Station is pinned to API v2.** `path` is sent as a JSON array — the only
  encoding safe for a filename containing a comma — and v1 cannot parse one: it kills
  the backend CGI and DSM answers `502 Bad Gateway`, which made every delete fail at
  the existence check.
- The startup banner no longer occupies the footer for the whole session, which hid
  the key hints in every real run. The connection moved to the title bar.
- The password is asked for only when it is actually needed: a valid cached session
  now saves the typing, not just a round trip.


First release. Everything below is the initial implementation; there is no previous
version to have changed anything from.

<!-- Replace "unreleased" with the ISO date when v0.1.0 is tagged. The release
     workflow reads this section verbatim for the GitHub release notes, so keep the
     "## [0.1.0]" heading prefix intact. -->

### Added

#### The table

- Terminal UI listing every Synology Download Station task in eleven columns: name,
  status, size, progress, download and upload speed, ratio, seeds/peers, ETA and
  destination, with a selection marker. Column widths are measured in terminal cells
  via `unicode-width`, so CJK and emoji titles do not shear the layout.
- Keyboard navigation (`↑`/`↓`, `k`/`j`, `PgUp`/`PgDn`, `Home`/`End`, `g`/`G`) that
  clamps and never wraps, and multi-select with `Space`, `a` (visible rows only) and
  `Esc` (everything, hidden rows included).
- The cursor and the selection are keyed by task ID, so a background refresh that
  reorders or removes rows never silently reassigns what is armed.
- Sort by eight keys (`s` cycles, `S` reverses): name, status, size, progress, download
  speed, upload speed, ratio and date added. Ties keep their order in both directions.
- Status filters (`f`) where `Downloading` is the grouped "in progress" set — it also
  covers waiting, finishing, hash-checking, extracting and file-hosting-waiting, so no
  status is reachable only under `All`.
- Live case-insensitive substring search over titles (`/`): it matches on every
  keystroke, `Enter` commits and `Esc` restores the query you started with.
- Distinct empty states for "the NAS has no tasks" and "a filter or search is hiding
  everything", the latter naming how many rows are hidden and by what.
- A storage band above the table: one coloured bar per volume with the percentage used
  and the free-of-total figure, so "did reclaiming that space work?" is answerable
  without leaving the TUI. It reads `SYNO.FileStation.List` `list_share` on its own
  slower clock (once a minute, and on every `r`), needs no admin account, and takes up
  **no row at all** until a read has succeeded — a NAS or account that refuses the call
  renders exactly the frame it did before, never an error banner.
- `?` help overlay documenting every binding, generated from the same data a test
  checks against the actual keymap, and naming `--no-delete-files` — the one remedy
  that is a flag rather than a key.
- A results modal listing every item a batch skipped or failed **with its reason**, on
  screen rather than in the log: it opens by itself when a batch finishes with something
  to report and `v` reopens it. Reasons are wrapped, never truncated, in both modals,
  and the footer drops its context before it clips a message.

#### Deleting

- `d` opens a confirmation listing each task, its resolved on-disk path and the total
  that will be freed; confirming removes the files through File Station and the task
  through Download Station. Cancel holds the focus when the dialog opens, so a reflexive
  `Enter` cancels; `y` is the one-key confirm.
- Delete-path resolution that **refuses rather than guesses**: a torrent whose file list
  has no single common top-level component — or which reports no file list at all, which
  for a torrent is an anomaly — is skipped and reported, never resolved from its title.
- Syntactic path guards rejecting a path that is empty, relative, the filesystem root, a
  share root, or that contains a `.`/`..` component, an empty or whitespace-only
  component, or a control character — each of which could otherwise turn into a
  recursive delete of a whole share.
- A File Station existence check before any recursive delete, distinguishing "already
  gone" (skip the files, still remove the task) from "an error" (touch nothing) — and
  checking *what* is there, not only that something is: a path whose kind disagrees with
  what the task resolved to — or whose file list describes no shape at all — is refused
  rather than removed recursively. A payload that should exist (a finished task, or one
  whose counters say it downloaded everything) and does not keeps its task. Every read
  of the task is folded into that judgement — the dialog's snapshot seeds it, the pause
  phase's reads follow — with status and counters each ratcheting toward "the payload
  must exist", so a task that completes while the pause settles is judged as complete
  and no later read, the `Paused` the pause itself causes included, can weaken what an
  earlier one already proved.
- Three-phase delete ordered for recoverability: pause an active task and **confirm by
  re-reading it** that it actually stopped, then delete the files, then delete the task.
  Any phase failing skips every later phase, so a task is never removed while its data
  survives.
- An owned snapshot behind the dialog: background refreshes are suspended while it is
  open and the path guards are re-run immediately before each File Station call, so what
  is on screen is exactly what goes.
- `--dry-run`, which runs the whole flow, reports every item as skipped and issues no
  call at all — not even the read-only existence check — and covers pause and resume as
  well as delete.
- `--no-delete-files` (`delete_files = false`), which removes the DSM task only and says
  so in the dialog, with the totals line reading "left on disk" instead of "to free".

#### Live operation

- Background poller on a configurable interval; the UI never blocks on the network.
  Poll failures raise a non-fatal banner that clears itself on the next successful
  refresh. `r` refreshes now.
- Pause (`p`) and resume (`u`) over the selection or the row under the cursor, in one
  round trip per batch.
- Per-task failures are detected inside DSM's `success: true` envelopes, so a delete,
  pause or resume that DSM rejected for a single task is never reported as a success.
- Delete, pause and resume all run off the event loop and report per item, so a batch of
  twenty does not freeze the terminal.

#### Connecting and configuring

- Configuration from CLI flags, `SYNO_CLEAN_*` environment variables and a TOML config
  file, in that order of precedence, with XDG paths on every platform including macOS.
  Unknown config keys warn and are ignored rather than failing.
- A first run with nothing configured writes a commented config template to
  `~/.config/syno-clean/config.toml`, says what is missing and exits without entering
  the TUI. An existing config file is never overwritten.
- Session `sid` caching at `~/.cache/syno-clean/session.json` (mode `0600`), keyed by
  host, port and username so several NASes or accounts never evict each other, with
  transparent re-login and one retry on DSM 106/107/119. `--logout` invalidates it; a
  normal quit deliberately does not.
- 2-step verification, via `SYNO_CLEAN_OTP` or a prompt when DSM asks. The password is
  never stored in the config file.
- HTTPS by default, with `--insecure` for a self-signed certificate.
- DSM API version negotiation through `SYNO.API.Info` — no hardcoded versions — and DSM
  error codes rendered as sentences, including the auth-specific 400 range.
- A connection or login failure exits non-zero with a diagnostic naming the host and
  port tried, what DSM said in words, and one thing to try, rather than showing an empty
  table.
- File logging to `~/.cache/syno-clean/syno-clean.log` (`--log-file` overrides), never
  to stdout, since the TUI owns the terminal.
- Running without an interactive TTY fails cleanly, writing nothing to stdout and
  changing no terminal state; a panic restores the terminal before printing.

#### Project

- MIT licence, README, contributing guide, and issue and pull-request templates.
- CI on Linux and macOS running `cargo fmt --check`, `cargo clippy -D warnings` and
  `cargo test`, against the toolchain pinned in `rust-toolchain.toml`.
- Tagged-release automation producing stripped binaries for `x86_64-apple-darwin`,
  `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`,
  with SHA-256 checksums and release notes taken from this file.

### Known limitations

- **DSM 7 only.** DSM 6 is not supported. All four operations use the documented v1
  `SYNO.DownloadStation.Task` API; migrating to `SYNO.DownloadStation2.Task` is a
  possible future change.
- **Both Download Station and File Station must be installed**, and the account needs
  permission for both. Without File Station the tasks still list, but a delete fails at
  the file phase and deliberately leaves the task in place rather than orphaning the
  data.
- **Eight sort keys against eleven columns**: seeds/peers, ETA and destination are not
  sortable, and "date added" sorts by a value that has no column of its own.
- **A terminal narrower than the table clips the rightmost columns.** Responsive column
  dropping is not implemented.
- **If the share has the DSM Recycle Bin enabled, deleted data may land in `#recycle`
  and free no space.** That is a DSM setting this program does not control.
- **`--logout` is not suppressed by `--dry-run`** — ending a session destroys no data.

[Unreleased]: https://github.com/Chekushkin/syno-clean/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Chekushkin/syno-clean/releases/tag/v0.1.0
