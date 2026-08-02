# syno-clean

A terminal UI for reviewing and cleaning up **Synology Download Station** tasks — one
that deletes the downloaded **files** along with the DSM task.

DSM's web Download Station is slow to load and awkward for bulk work, and deleting a
task there leaves the payload sitting on the volume. Reclaiming the space means a
second trip through File Station to hunt down each directory by hand. `syno-clean`
lists every task in a sortable, filterable table, lets you multi-select with the
keyboard, and removes both halves in one confirmed step.

Nothing is installed on the NAS: it is a plain HTTP client for the documented DSM API.

```text
 syno-clean 0.1.0                                                                                         14 / 14 tasks
  Name▲          Status            Size  Progress      ↓ Speed      ↑ Speed  Ratio Seeds/Peers      ETA Destination
  Absolute.Dest… finished     256.0 MiB    100.0%            —            —   1.00           —        ∞ /volume1/downlo…
✓ archlinux-202… finished       1.1 GiB    100.0%            —            —   0.50           —        ∞ downloads
  Big.Buck.Bunn… seeding        1.8 GiB    100.0%            —    1.2 MiB/s   2.14         5/9        ∞ video/movies
✓ Broken.Releas… error        700.0 MiB      0.0%            —            —   0.00           —        ∞ downloads
  empty-placeho… waiting            0 B      0.0%            —            —   0.00           —        ∞ downloads
  Hosted.Archiv… hosting      500.0 MiB      0.0%            —            —   0.00           —        ∞ —
  LibreOffice.2… paused         3.0 GiB     30.0%            —            —   0.00           —        ∞ software
  Mixed.Root.Re… seeding        3.0 MiB    100.0%            —     64 KiB/s   0.33         2/1        ∞ video/tv
  Mystery.Task.… captcha_ne…    1.0 GiB      0.0%            —            —   0.00           —        ∞ —
  Sintel.2010.2… finishing     12.0 GiB    100.0%    1.0 MiB/s    256 KiB/s   0.17         6/2       1s downloads/incom…
  Some.Show.S01… checking      20.0 GiB     50.0%            —            —   0.00           —        ∞ video/tv
  syno-clean-0.… waiting        4.0 MiB      0.0%            —            —   0.00           —        ∞ downloads
  Ubuntu.24.04.… downloading    5.8 GiB     39.0%    8.5 MiB/s    512 KiB/s   0.05        12/4    7m 7s downloads
  千と千尋の神…  extracting     8.0 GiB    100.0%            —            —   0.05         3/0        ∞ video/movies
 2 selected · 1.8 GiB · sort Name▲ · d delete · p/u pause/resume · r refresh · q quit · ? help
```

> This is **not a photograph of a live session**. It is the program's own renderer
> drawing one 120x17 frame from the checked-in test fixture
> (`tests/fixtures/task_list.json`) into an in-memory buffer, with two rows selected —
> the same `ratatui` `TestBackend` path the layout tests use. Colour and the cursor
> highlight are lost in plain text; everything else is exactly what the terminal gets.
>
> ⚠️ **A real terminal screenshot (or an asciinema recording) against a live NAS is
> still outstanding and must be added before v0.1.0 is published.** The fixture behind
> this frame is itself provisional — see [Contributing](CONTRIBUTING.md).

## Features

- Every Download Station task in one table: name, status, size, progress, download and
  upload speed, ratio, seeds/peers, ETA and destination.
- Arrow/vim navigation, `Space` multi-select, `a` select-all-visible.
- `d` → a confirmation listing exactly which tasks and which resolved on-disk paths are
  about to go, with the total it will free — then deletes the files *and* the task.
- Sort by eight keys, filter by status, live substring search over titles.
- Live auto-refresh in the background; the UI never blocks on the network.
- Pause (`p`) and resume (`u`) the selection.
- Correct column alignment for CJK and emoji titles (display width, not character
  count).

## ⚠️ `d` deletes files, and it is irreversible

Pressing `d` and confirming removes the task's directory **recursively** from the NAS
volume through File Station, and then removes the Download Station task. There is no
undo in this program and no prompt beyond the one confirmation dialog.

Two things to know before the first real run:

- **Try `--dry-run` first.** It walks the same flow and shows the same list of tasks
  and resolved paths, but the dialog is labelled as a dry run — `DRY RUN · ` in the
  title, a yellow border instead of a red one, and "Dry run: nothing is deleted"
  instead of "This cannot be undone" — and it issues no destructive call at all, not
  even the existence check.
- **If the share has the DSM Recycle Bin enabled, deleted data may land in `#recycle`
  and free no space.** That is a DSM setting this program does not control. Check the
  share's settings if a delete does not reclaim what you expected.

The safety model is described in [Delete safety model](#delete-safety-model) below.

## Requirements

- **DSM 7.** DSM 6 is not supported.
- **Download Station** installed and enabled, and a DSM account with permission to use
  it. The program uses the documented v1 `SYNO.DownloadStation.Task` API for all four
  operations (list, delete, pause, resume).
- **File Station** installed, and the same account permitted to browse and delete in
  the download share — this is what actually removes the files
  (`SYNO.FileStation.List` and `SYNO.FileStation.Delete`). Without it the tasks are
  still listed, but a delete will fail at the file phase and deliberately leave the
  task in place rather than orphan the data.
- API versions are negotiated, not assumed: `SYNO.API.Info` is queried at startup and
  each call uses the newest version inside the range both the NAS and this client
  support. The one deliberate pin is `SYNO.DownloadStation.Task`, held at **v1**
  because the newer `DownloadStation2` shape is undocumented and encodes statuses
  differently.

## Install

### From source

Requires the Rust toolchain pinned in `rust-toolchain.toml` (rustup installs it
automatically):

```sh
git clone https://github.com/emacarov/syno-clean
cd syno-clean
cargo build --release
# the binary is at target/release/syno-clean
install -m 0755 target/release/syno-clean ~/.local/bin/
```

### From a release binary

Prebuilt archives are attached to each tagged release for
`x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu`:

```sh
VERSION=0.1.0
TARGET=aarch64-apple-darwin
curl -fsSLO "https://github.com/emacarov/syno-clean/releases/download/v${VERSION}/syno-clean-${VERSION}-${TARGET}.tar.gz"
tar xzf "syno-clean-${VERSION}-${TARGET}.tar.gz"
install -m 0755 "syno-clean-${VERSION}-${TARGET}/syno-clean" ~/.local/bin/
```

Each archive unpacks into a directory of its own name holding the binary plus
`README.md`, `LICENSE` and `CHANGELOG.md`.

A single `SHA256SUMS` file covering every archive is attached to the same release, and
its contents are repeated in the release notes.

## Quick start

```sh
# no config file needed — flags are enough
syno-clean --host nas.local --user eduard

# look before you leap
syno-clean --host nas.local --user eduard --dry-run
```

The password is **never** stored in the config file. It comes from
`SYNO_CLEAN_PASSWORD`, or is prompted for on the terminal before the alternate screen
is entered. If the account uses 2-step verification, DSM asks for a code and the
program prompts for it (or reads `SYNO_CLEAN_OTP`).

On the first run with nothing configured, `syno-clean` writes a commented config
template to `~/.config/syno-clean/config.toml`, explains what is missing, and exits
without entering the TUI. An existing config file is never overwritten.

## Configuration

**Precedence: CLI flags > `SYNO_CLEAN_*` environment variables > config file >
built-in defaults.** `host` and `username` must be resolved by *something*; everything
else has a default.

Paths use **XDG semantics on every platform, macOS included**:

| What | Path |
|---|---|
| Config | `$XDG_CONFIG_HOME/syno-clean/config.toml` (default `~/.config/syno-clean/config.toml`) |
| Log file | `$XDG_CACHE_HOME/syno-clean/syno-clean.log` (default `~/.cache/syno-clean/syno-clean.log`) |
| Session cache | `$XDG_CACHE_HOME/syno-clean/session.json` (default `~/.cache/syno-clean/session.json`), mode `0600` |

Logs never go to stdout — the TUI owns the terminal — so the log file is the place to
look when something misbehaves, and the right thing to attach to a bug report.

The session `sid` is cached, keyed by `{host}:{port}/{username}` so several NASes or
accounts never evict each other. **Quitting normally does not log out**, which is what
makes the next start fast; use `--logout` to invalidate the cached session
deliberately.

### Config file

Unknown keys are logged as a warning and ignored, never a hard error — an older binary
must tolerate a newer config file.

```toml
host         = "nas.local"
port         = 5001      # default: 5001 with https, 5000 without
https        = true      # default: true
insecure     = false     # accept a self-signed / invalid TLS certificate
username     = "eduard"
refresh_secs = 3         # must be > 0
delete_files = true      # false = remove the DSM task only, leave the files
```

### Environment variables

| Variable | Effect |
|---|---|
| `SYNO_CLEAN_HOST` | DSM hostname or IP |
| `SYNO_CLEAN_PORT` | DSM port |
| `SYNO_CLEAN_HTTPS` | `true`/`false` — use HTTPS |
| `SYNO_CLEAN_INSECURE` | `true`/`false` — accept an invalid certificate |
| `SYNO_CLEAN_USERNAME` | DSM account name |
| `SYNO_CLEAN_PASSWORD` | password (otherwise prompted for) |
| `SYNO_CLEAN_OTP` | 2-step verification code (otherwise prompted for when DSM asks) |
| `SYNO_CLEAN_REFRESH_SECS` | seconds between automatic refreshes |

There is deliberately **no** `SYNO_CLEAN_DELETE_FILES`: an environment variable that
silently disables the program's main function is a footgun. Use `--no-delete-files` or
the config key.

### Command-line flags

| Flag | Effect |
|---|---|
| `--config <PATH>` | Config file to read instead of the default path |
| `--host <HOST>` | DSM hostname or IP address |
| `--user <NAME>` | DSM account name |
| `--port <PORT>` | DSM port (default 5001 for HTTPS, 5000 for HTTP) |
| `--insecure` | Accept a self-signed or otherwise invalid TLS certificate |
| `--refresh-secs <SECS>` | Seconds between automatic task-list refreshes |
| `--no-delete-files` | Remove the DSM task only, leave a finished task's files |
| `--log-file <PATH>` | Write logs here instead of the default cache path |
| `--dry-run` | Report what would happen; issue no destructive call |
| `--logout` | Invalidate the cached session and exit |
| `--help`, `--version` | The usual |

Boolean flags are **one-way switches**: `--insecure` and `--dry-run` can only turn a
setting on and `--no-delete-files` only off, so an unset flag falls through to the
environment or the config file instead of overriding it with `false`.

## Keybindings

`?` shows the same list inside the program, and that overlay — not this table — is the
authoritative one.

### Navigation

| Key | Action |
|---|---|
| `↑` `k` | move up |
| `↓` `j` | move down |
| `PgUp` `PgDn` | move a screenful |
| `Home` `g` | first row |
| `End` `G` | last row |

### Selection

| Key | Action |
|---|---|
| `Space` | toggle this row |
| `a` | (de)select every **visible** row — a filtered-out task is never touched |
| `Esc` | clear the selection, hidden rows included |

### Actions

| Key | Action |
|---|---|
| `d` | delete — opens the confirmation |
| `p` | pause the selection, or the row under the cursor |
| `u` | resume the selection, or the row under the cursor |
| `r` | refresh now |

`d`, `p` and `u` act on the selection when there is one, and on the row under the
cursor when there is not.

### Sort, filter, search

| Key | Action |
|---|---|
| `s` | next sort column |
| `S` | reverse the sort direction |
| `f` | next status filter |
| `/` | search titles |

Sort keys: Name, Status, Size, Progress, ↓ Speed, ↑ Speed, Ratio, Added. That is eight
keys against the table's ten headed columns (the table has eleven in all; the first is
the headerless selection marker) — Seeds/Peers, ETA and Destination are **not**
sortable, and `Added` sorts by a value that has no column of its own. Post-v1 if it is
missed.

Status filters: **All**, **Downloading**, **Seeding**, **Finished**, **Paused**,
**Error**. `Downloading` means "in progress" and covers `downloading`, `waiting`,
`finishing`, `hash_checking`, `extracting` and `filehosting_waiting`, so those rows are
never invisible to someone who is filtering. A task with a status this client does not
recognize is shown only under `All` — it cannot be classified without guessing.

### Search box

| Key | Action |
|---|---|
| `Enter` | apply and close the box |
| `Esc` | cancel, restoring the previous query |
| `Backspace` | delete a character |

Matching is **live**, on every keystroke, so `Enter` commits the query rather than
applying it. Every other printable key is text — `q` types a `q`.

### Confirmation dialog

| Key | Action |
|---|---|
| `y` | delete |
| `n` `Esc` `q` | cancel |
| `Enter` | press the **focused** button |
| `Tab` `←` `→` `h` `l` | switch button |
| `↑` `↓` `k` `j` | scroll the list a line |
| `PgUp` `PgDn` | scroll the list a page |
| `Home` `End` | jump to the first / last line |

**Cancel has the focus when the dialog opens**, so a reflexive `Enter` cancels. `y` is
the deliberate one-key confirm. `q` closes the dialog rather than the program.

### General

| Key | Action |
|---|---|
| `?` | help overlay (any key closes it, and does nothing else) |
| `q` | quit |
| `Ctrl-C` | quit from anywhere |

## Delete safety model

Deriving "which directory holds this torrent" from the DSM API is the one place this
tool could destroy the wrong data, so it is built to **refuse rather than guess**.

### 1. Path resolution refuses ambiguity

1. The task's file list has a single common top-level component → that is the on-disk
   name, even when the display title differs from it.
2. The file list has **no** single common top-level component → **the task is skipped**
   and reported as such. It does *not* fall back to the title: a guessed path could
   match an unrelated folder and be recursively deleted.
3. The file list is absent or empty (HTTP/FTP/NZB downloads have none) → the title is
   used.
4. The destination is normalized (a leading volume mount stripped — `/volume1`,
   `/volume`, `/volumeUSB1/usbshare1-2`, `/volumeSATA1/…` — and surrounding slashes
   trimmed) and joined as `/{destination}/{name}`. A *relative* destination is never
   touched, and an absolute first component that is not a mount point (`/downloads`)
   is already share-rooted and left alone. A mount point is matched by **shape** —
   `volume`, `volume<N>`, `volumeUSB<N>`, `volumeSATA<N>` — so a share that merely
   starts with the word (`/volumes/movies`, `/volume-media/tv`) keeps its first
   component instead of having the delete re-rooted into a different share.
5. The normalized destination is **empty** → **the task is refused**. With no
   destination there is no share to root the path at, and `/{name}` would name a share
   rather than a directory inside one.

### 2. Syntactic guards

A resolved path is refused — leaving the task completely untouched — if it is empty,
`/`, has fewer than two components (never delete a share root), lacks a leading `/`,
contains a `.` or `..` component, has an empty or whitespace-only component, or
contains a control character. The on-disk name is separately required to be a single
component, so a title fallback cannot smuggle in a `/` and aim one level deeper than
the task's own directory.

### 3. Existence check before deleting

Before any recursive delete, `SYNO.FileStation.List` `getinfo` is called on the
resolved path:

- **not found**, for a task that never finished → the file phase is *skipped*
  (Download Station cleans up its own partial data) and only the DSM task is deleted,
  which is the harmless half;
- **not found**, for a task that **finished, is seeding or is extracting** → its
  payload demonstrably existed, so nothing being there says the resolved location is
  wrong far more often than it says the files are gone. The item fails and the task
  stays, because deleting it would leave that payload with nothing pointing at it.
  (A path this same run already deleted is exempt — the absence is explained.)
- **found** → proceed;
- **an error** (a permission problem, say) → that is not absence: the item fails and
  the task is left alone rather than being deleted while its files stay behind.

After the delete reports itself finished the path is looked up once more. Still there,
an error code, or a lookup that fails outright all keep the task: only an answer that
actually says "gone" completes the item.

### 4. Three phases, ordered for recoverability

| Task status | Ordering |
|---|---|
| Downloading, Seeding, Waiting, Finishing, Hash checking, Extracting, File-hosting waiting, unknown | pause → confirm the pause → delete the files → delete the task |
| Paused, Finished, Error | delete the files → delete the task |

The pause keeps Download Station from holding file handles or re-creating directories
underneath the delete, and it is **confirmed by re-reading the task**, because DSM
accepting a `pause` only means the request was queued.

**Any phase failing skips every later phase.** If the files cannot be deleted, the task
is not deleted either — it survives still pointing at its data, so nothing is ever
orphaned.

### 5. What you see is what goes

```text
  Absolute.Dest… fi┌ Delete 2 tasks ────────────────────────────────────────────────────────────────┐ ∞ /volume1/downlo…
✓ archlinux-202… fi│ Removes the Download Station task and its files on the NAS. This cannot be     │ ∞ downloads
  Big.Buck.Bunn… se│ undone.                                                                        │ ∞ video/movies
✓ Broken.Releas… er│                                                                                │ ∞ downloads
  empty-placeho… wa│ • archlinux-2026.07.01-x86_64.iso                                              │ ∞ downloads
  Hosted.Archiv… ho│     1.1 GiB  /downloads/archlinux-2026.07.01-x86_64.iso                        │ ∞ —
  LibreOffice.2… pa│ • Broken.Release.2019.720p                                                     │ ∞ software
  Mixed.Root.Re… se│     700.0 MiB  /downloads/Broken.Release.2019.720p                             │ ∞ video/tv
  Mystery.Task.… ca│                                                                                │ ∞ —
  Sintel.2010.2… fi│ 2 tasks · 1.8 GiB to free                                                      │1s downloads/incom…
  Some.Show.S01… ch│                        [ Cancel (Esc) ]   [ Delete (y) ]                       │ ∞ video/tv
  syno-clean-0.… wa└────────────────────────────────────────────────────────────────────────────────┘ ∞ downloads
```

Every resolved path is on screen before anything is sent. The confirmation dialog is an
owned **snapshot** taken the moment it opens: background
refreshes are suspended while it is up, and the path guards are re-run immediately
before each File Station call. Refused items are listed as `SKIPPED` with the reason
and excluded from the total, so a skipped task can never be mistaken for a cleaned-up
one.

### Escape hatches

- **`--dry-run`** — the whole flow runs, the dialog says `DRY RUN`, every item is
  reported as *skipped* and **no call is issued at all**, not even the read-only
  existence check. Pause and resume are suppressed too: a flag promising the NAS is
  untouched must mean it.
- **`--no-delete-files`** (or `delete_files = false`) — removes the Download Station
  task only and leaves a **finished** task's files on the volume. The dialog says so,
  and the totals line reads "left on disk" instead of "to free". ⚠️ A task that has
  *not* finished is different: DSM deletes its partial data along with the task
  (`force_complete=false`, which is deliberate — the alternative marks the task
  complete and keeps a half-downloaded file). The dialog says that too, per row and in
  the totals, so those bytes are never counted as staying. This is also the way to remove a task
  whose on-disk location cannot be worked out — no destination, or a file list with
  several top-level directories. Those rows are `SKIPPED` in the normal flow, because
  a recursive delete that cannot be aimed must not be followed by removing the only
  pointer to the data; with the files out of scope there is nothing left to be unsure
  about, so they are listed as ordinary deletable rows.

## Troubleshooting

- **"needs an interactive TTY"** — the TUI cannot run with stdout redirected. Nothing
  is written to the terminal and no terminal state is changed in that case.
- **Self-signed certificate** — `--insecure`, or `insecure = true`.
- **A connection or login failure exits non-zero with a diagnostic** naming the host and
  port tried, what DSM said in words rather than as a bare error code, and one thing to
  try. It never enters the TUI, because an empty table is exactly what a NAS with no
  downloads looks like.
- **A failure *during* a session is non-fatal**: a red banner appears in the footer and
  clears itself on the next successful refresh.
- The log file (see [Configuration](#configuration)) has the details for everything
  above. **It is written at `INFO` and above, and that level is fixed** — there is no
  `--verbose` flag and `RUST_LOG` is ignored, so debug-level lines never appear. If the
  log does not explain what happened, the only way to get more is a local build with
  the level raised in `config::init_logging`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). In short: `cargo fmt --all -- --check`, `cargo
clippy --all-targets -- -D warnings` and `cargo test --all` must all be clean — the
three commands CI runs — and the UI can be exercised offline against a captured
response with no NAS in reach.

## License

MIT — see [LICENSE](LICENSE).
