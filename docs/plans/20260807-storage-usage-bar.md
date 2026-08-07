# Storage usage bar above the task table

## Overview

Show how full the NAS is, without leaving the TUI. A one-line band between the
title bar and the task table renders one segment per volume:

```
 syno-clean 0.1.0 · nas.local:5001        12 / 41 tasks
volume1 [████████████████░░░░░]  78%  3.1 TiB free of 14.0 TiB
 Name                              Size   Status   Progress
 Some.Release.2160p                42.1G  seeding  100%
```

The problem it solves: this tool exists to reclaim space, and there is currently
no way to see whether reclaiming it worked or whether it is needed. The number
that answers both questions is one HTTP call away from an API the program
already talks to.

How it integrates: the band is drawn from `App` state like everything else, fed
by a new `AppEvent` the poller emits on a **slower** cadence than the task list,
and is **absent entirely** — zero rows, no layout shift — until a storage read
has succeeded. A NAS that refuses the call (or `--fixture`, which has no client
at all) simply never grows the band.

## Context (from discovery)

- files/components involved:
  - `src/api/file_station.rs` — new `list_share` call, pure response reader
  - `src/api/client.rs` — **read only**; the storage call uses the existing
    `endpoint` + `send` + `parse_envelope` escape hatch rather than `call`
  - `src/model.rs` — `de_u64`, currently private
  - `src/format.rs` — new `gauge()` helper; `bytes()`/`percent()` reused as-is
  - `src/event.rs` — new `AppEvent::Storage`, throttled fetch in `spawn_poller`
  - `src/app.rs` — new `App::storage` field, one `apply_event` arm, `Default`
  - `src/ui/mod.rs` — fourth layout band, pure `storage_line`, `CHROME_ROWS`
  - `src/main.rs` — the `set_page_size` call site, which must learn about the band
- related patterns found:
  - every API module owns a `SUPPORTED: VersionRange` and pure
    `build_*_params() -> Vec<(&'static str, String)>` builders
  - in `api::file_station` the **wire structs are `pub`** (`GetInfo`,
    `DeleteStatus`) precisely so the pure reader over them can be `pub` too
    (`classify_getinfo`, `classify_delete_status`). The private-`Raw*` pattern is
    `model.rs`'s, not this module's — following the wrong one here is a
    `private_interfaces` error under `-D warnings`.
  - `render` is a pure function of `&App`; `App` performs no I/O
  - `format::display_width` / `truncate_ellipsis`, never `str::len`
- dependencies identified: none new. `SYNO.FileStation.List` is already
  discovered at startup (discovery is `query=all`) and already pinned to
  `FS_LIST_SUPPORTED = (2, 2)`.

## Development Approach

- **testing approach**: **none — explicitly waived by the user for this
  feature.** The user will verify manually against a live NAS. No new unit
  tests, no new fixtures, no `TestBackend` assertions are to be written.
- The existing suite must still pass untouched, and the full gate still applies
  after every task:

  ```sh
  cargo fmt --all
  cargo build
  cargo clippy --all-targets -- -D warnings
  cargo test
  ```

  `-D warnings` is the one that will actually bite — `private_interfaces` in
  particular (see Task 2).
- complete each task fully before moving to the next; small, focused changes
- **update this plan file when scope changes during implementation**
- maintain backward compatibility — the band must be invisible on any NAS or
  account where the new call does not work

## Testing Strategy

Waived for this feature at the user's request (`No tests required. Just
implement the feature and I'll test it manually`). This is a deliberate,
recorded exception to the repo's normal rule, and it is defensible only because
of what the feature is: the storage band is **read-only display**. It issues no
destructive call, it touches neither `delete.rs` nor the op ordering, and a
wrong number on screen is a cosmetic bug rather than a data-loss one.

⚠️ **The waiver is about the diff, and behaviour is what actually matters.** A
read-only feature can still break the delete path through *shared client state*
— see the `permission_is_real` hazard in the Solution Overview, which is the one
thing in this plan that could make the waiver unsafe and is the reason the
storage call deliberately does not go through `SynoClient::call`. Nothing here
may be allowed to change that property.

The repo's existing tests are the regression net: `ui::tests` will fail loudly if
the new layout band shifts the frame when it should not, and `app::tests` if
`apply_event` grows a bad arm. Neither covers the client-state or cadence
hazards, so those are called out as explicit manual observations in Task 7.

### Non-goals (keep the no-tests waiver honest)

- No change to `delete.rs`, `event::spawn_delete`, or anything in the delete
  ordering.
- No change to `SynoClient`'s retry semantics, the `permission_is_real` latch, or
  any other shared client state.
- No new *destructive* call. `list_share` is a read.
- No change to what the confirmation dialog says or totals.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- keep this plan in sync with the actual work

## Solution Overview

**Data source: `SYNO.FileStation.List`, `method=list_share`, with
`additional=["real_path","volume_status"]`.**

Chosen over `SYNO.Core.Storage.Volume` because it needs **no admin account** —
`SYNO.Core.Storage.*` is admin-gated on a normal DSM setup, and the account this
tool is pointed at is frequently a restricted download-only user, which would
get a permission error and no bar. It is also an API the program **already
discovers and already version-pins**, so it adds no new startup surface and no
second version negotiation.

`volume_status` reports `{freespace, totalspace, readonly}` **per share**, and
shares on the same volume all report the same numbers. `real_path` is what makes
them dedupable: `list_share` returns `path` as the *share* path (`/downloads`),
while `real_path` is the resolved one (`/volume1/downloads`), whose first
component is the mount point. Dedupe on that component and one volume shows once
however many shares live on it.

### ⚠️ The storage call must not go through `SynoClient::call`

This is the single most important constraint in the plan, and it is not obvious.

`call` → `call_text` (`src/api/client.rs:372-432`) treats DSM **105** as a
possibly-stale session: it clears the sid, re-logs-in, and if 105 survives the
fresh session it latches `permission_is_real` — **client-wide, not per-API**.
That latch then disables the 105 retry for *every* API, including
`SYNO.DownloadStation.Task`.

A restricted download-only account is exactly the kind this tool is pointed at,
and it is exactly the kind that answers 105 to `list_share`. So routing the
storage read through `call` would mean:

- the first storage poll throws away a working sid and forces a re-login, and
- once latched, a genuinely stale session answering 105 from Download Station no
  longer triggers the retry — reinstating the failure commit `5507247` exists to
  fix, where every poll fails until `session.json` is deleted by hand.

A cosmetic band must not be able to do that, and with no tests nothing would
catch it. **The storage read therefore uses the documented no-retry escape
hatch** — `client.endpoint(…)` + `client.send(…, client.sid())` +
`parse_envelope` — all of which are already `pub`. The cost is that a storage
read against a genuinely expired session just fails; the *task* poller repairs
the session a moment later and the storage read succeeds on its next turn. That
is the right trade for a display-only number.

### Cadence

**A separate, slower clock inside the existing poller.** Free space moves on the
scale of a completed download, not of a `refresh_secs` tick (default **3 s**);
asking every tick would multiply this program's request rate on the NAS for a
number that would not visibly change. The poller reads storage on its first tick
and thereafter at most once per `STORAGE_INTERVAL` (60 s).

The throttle stamps on **every attempt, success or failure**. Stamping only on
success would mean a NAS that refuses `list_share` gets a failed request and a
`warn!` line every 3 seconds for the whole session — a request storm and a
flooded log file, in the name of a feature whose entire justification is that it
is cheap. On top of that, a **permission-shaped refusal (105 / 403) disables the
storage read for the rest of the session**: that answer is not going to change,
and retrying it forever is pure noise.

### Failure is silent

A failed storage read logs at `warn` and emits nothing. It must **not** become
`AppEvent::Error`: that banner means "the NAS is unreachable", it is cleared by
the next successful *task* poll, and letting a cosmetic read raise it would both
lie and stamp on a real refresh error the user needs to see. The band simply
stays as it was, or absent.

Note the storage read runs **inline in the poller loop after `poll_once`**, and
the ticker uses `MissedTickBehavior::Delay`. A slow `list_share` can therefore
delay the *next* task tick by up to `REQUEST_TIMEOUT` (30 s), once a minute at
worst. This is accepted rather than solved: spawning a detached task per storage
read is more machinery than the problem deserves, and the failure mode is a
visibly stale table rather than anything silent. If a real NAS shows it, the
follow-up is in Post-Completion.

### Key design decisions

| Decision | Why |
|---|---|
| `endpoint` + `send` + `parse_envelope`, **never** `client.call` | Keeps the storage read out of the 105 retry and the client-wide `permission_is_real` latch — see above. The one thing that could make the no-tests waiver unsafe. |
| Band is `Constraint::Length(0)` while `App::storage` is empty | No layout shift, and no empty gutter on a NAS that refuses the call or under `--fixture`. |
| Segment text built by a **pure** `ui::storage_line(&[VolumeUsage], width) -> Line` | Same split as `dialog::build_confirmation` / `render_confirm`: the arithmetic and the eliding are separable from the drawing, and `render` stays a pure function of `&App`. It returns a `Line` rather than a `String` because the filled run is coloured. |
| Wire structs are `pub` | Matches `GetInfo` / `DeleteStatus` in the same module, and a `pub` reader over a private type is a `private_interfaces` error under `-D warnings`. |
| Dedupe key from `real_path`'s first component; a share without one is **skipped** | It is the only field that distinguishes two volumes. Inventing a synthetic key from `{total}:{free}` would merge two genuinely distinct volumes and label them with a name DSM never sent — refusing to display beats displaying something made up. |
| Numbers through `model::de_u64` | DSM sends sizes as JSON numbers *or* strings depending on build. A plain `u64` field here is the same bug the task model already guards against. |
| Colour by fullness (green / yellow / red) | The whole point is noticing "almost full" without reading digits. |

## Technical Details

### Wire shape (`list_share`)

```json
{
  "success": true,
  "data": {
    "total": 5,
    "offset": 0,
    "shares": [
      {
        "isdir": true,
        "name": "downloads",
        "path": "/downloads",
        "additional": {
          "real_path": "/volume1/downloads",
          "volume_status": { "freespace": 3448068915200, "totalspace": 15384936448000, "readonly": false }
        }
      }
    ]
  }
}
```

Every field below `shares` is treated as optional (`#[serde(default)]`) — a
share with no `additional`, or with `real_path` but no `volume_status`, is
skipped rather than being allowed to blank the whole band. Same rule as
`model.rs`'s optional `additional` sub-blocks, for the same reason.

### New public types

```rust
// src/api/file_station.rs — all pub, matching GetInfo / DeleteStatus
pub struct ShareList      { pub shares: Vec<Share> }
pub struct Share          { pub name: String, pub additional: Option<ShareAdditional> }
pub struct ShareAdditional{ pub real_path: Option<String>, pub volume_status: Option<VolumeStatus> }
pub struct VolumeStatus   { pub freespace: u64, pub totalspace: u64 }   // both via model::de_u64

/// One volume's occupancy, deduped across the shares that live on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeUsage {
    /// Mount point as DSM spells it — `volume1`, `volumeUSB1`, … Display only.
    pub name: String,
    pub total: u64,
    pub free: u64,
}

impl VolumeUsage {
    pub fn used(&self) -> u64;      // total.saturating_sub(free)
    pub fn fraction(&self) -> f64;  // 0.0 when total == 0 — a guarded denominator
}

/// Pure: collapse a `list_share` payload into one entry per volume.
pub fn collect_volume_usage(list: &ShareList) -> Vec<VolumeUsage>;

/// The call. Uses the no-retry escape hatch, not `SynoClient::call`.
pub async fn volume_usage(client: &SynoClient) -> Result<Vec<VolumeUsage>>;
```

`collect_volume_usage` rules, in order:

1. skip any share with no `volume_status`, or whose `totalspace` is 0 — nothing
   to draw a bar of;
2. derive the volume key from `real_path`: the first path component, when the
   path is absolute and that component starts with `volume` — the same mount
   spelling rule `delete::normalize_destination` already relies on;
3. skip a share whose `real_path` is absent or does not look like a mount —
   there is nothing to attribute its numbers to;
4. first entry per key wins; the result is sorted by `name` so the band's order
   is stable across polls and the bar does not reshuffle under the user.

### `format::gauge`

```rust
/// A fixed-width `████░░░░` bar body (no brackets), `width` cells wide.
///
/// Both glyphs are single-cell, so the bar's rendered width is exactly `width`
/// — the same property `table::SELECTED_MARKER` is asserted to have, and for
/// the same reason: a two-cell glyph here would shear everything to its right.
pub fn gauge(fraction: f64, width: usize) -> String;
```

Clamps `fraction` to `0.0..=1.0` and treats non-finite as 0, exactly as
`format::percent` already does — a `NaN` must not panic mid-frame. `width == 0`
returns an empty string rather than panicking.

### `ui::storage_line`

Per volume: `{name} [{gauge}] {pct}  {free} free of {total}`, segments joined by
three spaces, bar `STORAGE_GAUGE_WIDTH` (20) cells. When the line does not fit,
it degrades in exactly **three** steps — this is untested width arithmetic, so
the ladder is kept as short as it can be while still keeping the useful part:

1. full form;
2. drop the ` free of {total}` tail from every segment, keeping name + bar + percent;
3. `format::truncate_ellipsis` the whole line.

Colour applies to the filled run only: green below 75%, yellow 75–90%, red at or
above 90%. The line is emitted as a `Line` of spans, composed once.

## What Goes Where

- **Implementation Steps** (`[ ]`): the code, in this repo.
- **Post-Completion** (no checkboxes): the manual verification against a real
  NAS, which is the agreed substitute for tests here.

## Implementation Steps

### Task 1: Expose the permissive numeric deserializer to `api`

**Files:**
- Modify: `src/model.rs`

- [x] change `de_u64` from private to `pub(crate)`. **Leave `de_u32` and
      `de_i64_opt` alone** — widening a function with no consumer is a
      gratuitous diff
- [x] add a one-line doc note that this is the crate-wide answer to DSM sending
      a number as a string, and is deliberately `pub(crate)` rather than `pub`
- [x] full gate. It passes as-is: `de_u64` already has callers inside `model.rs`,
      so widening it produces no `dead_code` warning

### Task 2: `list_share` call and the pure volume reader

**Files:**
- Modify: `src/api/file_station.rs`

- [x] add `build_list_share_params()` returning
      `[("additional", "[\"real_path\",\"volume_status\"]")]`, built with
      `serde_json` rather than a hand-written literal, matching
      `encode_path_list`'s reason for existing. No new method const — every
      existing call site passes the method name as a literal
- [x] add the wire structs `ShareList` / `Share` / `ShareAdditional` /
      `VolumeStatus`, **all `pub`**, every field `#[serde(default)]`, both sizes
      through `model::de_u64`
- [x] add `pub struct VolumeUsage` with `used()` and `fraction()`, guarding the
      `total == 0` denominator
- [x] add pure `collect_volume_usage`, implementing the four rules under
      Technical Details (skip / key from `real_path` / skip unattributable / sort)
- [x] add `pub async fn volume_usage(client)` built from
      `client.endpoint(FS_LIST_API, FS_LIST_SUPPORTED)?`,
      `client.send(&endpoint, "list_share", &params, client.sid())` and
      `parse_envelope::<ShareList>` — **not** `client.call`, with a doc comment
      naming the `permission_is_real` latch as the reason so nobody "simplifies"
      it back later
- [x] full gate. Watch specifically for `private_interfaces`
- ➕ the mount-point test is a **local** private `mount_component` rather than
      `delete::is_volume_component`, which stays private: Task 7 asserts
      `git diff` shows no change to `src/delete.rs`, and coupling a display
      label to a guard whose job is paranoia invites loosening the guard. The
      duplication is documented at the function.

### Task 3: `format::gauge`

**Files:**
- Modify: `src/format.rs`

- [x] add `pub fn gauge(fraction: f64, width: usize) -> String` using `█`
      (U+2588) and `░` (U+2591)
- [x] clamp the fraction and map non-finite to 0, mirroring `percent`
- [x] return an empty string for `width == 0` rather than panicking
- [x] document that both glyphs are single-cell and that this is load-bearing
- [x] full gate
- ➕ the two glyphs are named consts (`format::GAUGE_FILLED` / `GAUGE_EMPTY`)
      beside `DASH` / `INFINITY` / `ELLIPSIS`, so the single-cell rule has one
      documented home rather than being a literal buried in the function

### Task 4: `AppEvent::Storage` and the throttled poller fetch

**Files:**
- Modify: `src/event.rs`

- [x] add `AppEvent::Storage(Vec<VolumeUsage>)` with a doc comment saying it is
      display-only and never fatal
- [x] add `pub const STORAGE_INTERVAL: Duration = Duration::from_secs(60);` with
      the rationale (free space changes on the scale of a finished download, the
      default `refresh_secs` is 3, and this call is not what the user is waiting
      for)
- [x] in `spawn_poller`, hold `let mut storage: StorageSchedule` (a tiny local
      struct or just `Option<tokio::time::Instant>` plus a `bool give_up`) across
      the loop and, **after** `poll_once`, call a new `poll_storage_once` when
      the read is due and has not been given up on
- [x] `poll_storage_once` returns whether the channel is still open, exactly like
      `poll_once`; on `Err` it logs `tracing::warn!` and **sends nothing** — no
      `AppEvent::Error`, per the Solution Overview
- [x] stamp the throttle on **every attempt**, success or failure, so a refusing
      NAS is asked once a minute rather than every 3 seconds
- [x] on a permission-shaped refusal — `Error::Dsm { code, .. }` where
      `code == error::OTP_REQUIRED_CODE` or the DSM permission code 105 — set the
      give-up flag and log once at `info` that the storage band is disabled for
      this session
- [x] full gate
- ➕ **ordering:** `App::apply_event`'s match is exhaustive, so the new variant
      does not compile without an arm. A one-line placeholder
      (`AppEvent::Storage(_) => {}`, commented as such) was added to `src/app.rs`
      in this task purely to keep the gate green; **Task 5 replaces it** with the
      real `self.storage = volumes`. A catch-all `_` arm was deliberately not
      used — it would stop the compiler naming future variants.
- ➕ `is_permission_refusal` is a small private predicate rather than an inline
      `matches!`, so the "105 here is not the ambiguous stale-session case"
      reasoning has one documented home. It uses `error::PERMISSION_DENIED_CODE`
      rather than a literal `105`.

### Task 5: `App::storage` and the `apply_event` arm

**Files:**
- Modify: `src/app.rs`

- [x] add `pub storage: Vec<VolumeUsage>` to `App`, with a doc comment: empty
      means "no storage read has succeeded yet", which is what the renderer keys
      the band's existence off
- [x] add it to the hand-written `impl Default for App`, which lists every field
- [x] add `AppEvent::Storage(volumes) => self.storage = volumes` to
      `apply_event` — applied unconditionally, **including in `Mode::Confirm`**,
      unlike `AppEvent::Tasks`. It is not part of the frozen delete plan and
      cannot make it stale. Note the honest cost: the confirmation modal is
      centred, not full-screen, so the first `Storage` event arriving while it is
      open shifts the table *around* it down one row. Cosmetic, and accepted
- [x] full gate
- ➕ the placeholder arm Task 4 left behind is gone; the doc comment on the real
      arm also records *why* `AppEvent::Tasks` is the one that must be dropped in
      `Mode::Confirm` (it feeds the frozen snapshot) so the asymmetry reads as a
      decision rather than an oversight. There is deliberately no
      "asked and failed" companion flag: a failed storage read is silent, so a
      flag would have nothing to say and would only offer the renderer a second,
      contradictable existence test.

### Task 6: The band in the frame, and the page size

**Files:**
- Modify: `src/ui/mod.rs`
- Modify: `src/main.rs`

- [ ] **first**, confirm `Constraint::Length(0)` yields exactly zero rows in
      ratatui 0.30's solver before building on it — the existing `ui::tests` are
      this feature's only regression net and they all run with an empty
      `App::storage`. Check `rendering_survives_a_terminal_too_small_for_the_layout`
      (`src/ui/mod.rs:1148`) in particular: it renders at 1x1, 1x3, 3x1 and 2x2,
      now with four constraints
- [ ] add `STORAGE_GAUGE_WIDTH` (20) const
- [ ] add pure `storage_line(volumes: &[VolumeUsage], width: usize) -> Line`
      implementing the three-step degradation, measuring with
      `format::display_width` only
- [ ] colour the filled run green / yellow / red at the 75% and 90% thresholds
- [ ] widen `render`'s `Layout::vertical` to four bands — title `Length(1)`,
      storage `Length(u16::from(!app.storage.is_empty()))`, body `Min(1)`, footer
      `Length(1)` — render the band only when it has height, and update the
      "Three bands" doc comment
- [ ] ⚠️ **thread the band's height into the page size.** `CHROME_ROWS = 3`
      (`src/ui/mod.rs:77`) and `table_page_size` (`:143`) hardcode "title + table
      header + footer", and `main.rs:364` feeds `terminal.page_size()` into
      `App::set_page_size` after every draw with no access to `App::storage`.
      Left alone, `PageDown`/`PageUp` over-jump by one row whenever the band is
      visible. Give `table_page_size` an `extra_chrome: u16` parameter (or an
      equivalent), pass `u16::from(!app.storage.is_empty())` from the call site,
      and update `CHROME_ROWS`' doc comment
- [ ] confirm the modals still draw over `frame.area()` and are unaffected
- [ ] full gate — the existing `ui::tests` must pass **unchanged**; if any fails,
      the band is taking a row it should not

### Task 7: Verify acceptance criteria

- [ ] band is absent with an empty `App::storage` (default, `--fixture`, and a
      NAS whose account cannot list shares)
- [ ] `git diff` shows **no change** to `src/api/client.rs`, `src/delete.rs`, or
      any op ordering, and `volume_usage` does not call `client.call`
- [ ] a failed storage read never raises the error banner and never clears a
      real one
- [ ] with the band visible, `PageDown` then `PageUp` returns the cursor to the
      row it started on
- [ ] no `str::len` or `chars().count()` used for any width in the new code
- [ ] no new dependency in `Cargo.toml`; no direct `crossterm`
- [ ] full gate one final time

### Task 8: [Final] Update documentation

- [ ] add a short subsection to `CLAUDE.md` under UI conventions covering: why
      `list_share` over `SYNO.Core.Storage.Volume` (no admin needed); **why the
      storage read bypasses `SynoClient::call`** (the client-wide
      `permission_is_real` latch, and the DSM-105 regression that reinstating it
      would cause); why the cadence is separate from `refresh_secs` and throttles
      failures too; why a storage failure is silent rather than an
      `AppEvent::Error`; and why the band has zero height when empty
- [ ] extend the `FS_LIST_SUPPORTED` doc comment: `list_share`'s `additional` is
      a JSON array on v2 and comma-separated on v1, so the new call *strengthens*
      the existing v2 pin rather than merely borrowing it — the comment is
      currently phrased entirely in terms of `build_fs_path_params`
- [ ] note in `CLAUDE.md`'s known-gaps section that the `list_share`
      `volume_status` shape is **unverified against a real NAS** until the manual
      pass below confirms it, and that this feature ships without tests by
      explicit request
- [ ] update `README.md` if it describes the screen layout (its terminal frame is
      a `TestBackend` rendering of the fixture, which has no storage, so the frame
      itself needs no change — check before editing)
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*No checkboxes — these need a real NAS and are the agreed substitute for tests.*

**Manual verification** (the whole test plan for this feature):

- Run against the live NAS and confirm the band appears within one poll and the
  numbers match DSM's own Storage Manager.
- Confirm the volume label is the real mount point (`volume1`, or `volumeUSB1`
  on an external volume) and that a NAS with several shares on one volume shows
  **one** segment, not one per share.
- **Read the log file after a few minutes** and confirm there is no re-login
  storm and no repeating storage `warn` — at most one storage line per minute,
  and none at all after a permission refusal beyond the single give-up line.
- Resize the terminal narrow and wide: the bar must shrink and the tail drop
  without the line ever wrapping or shearing the table below it.
- With the band visible, page up and down through a long list and confirm the
  cursor lands where it should.
- Delete a large task and confirm the free-space figure moves — within
  `STORAGE_INTERVAL`, not instantly.
- Run `--fixture` and confirm no band and no layout shift.
- If available, run with a **non-admin** download-only DSM account and confirm
  the band either works or is silently absent — never an error banner — and that
  deleting a task afterwards still works, i.e. the session retry is intact.
- Watch for the table stalling once a minute; that would be the inline storage
  read blocking the poller loop (see Cadence).

**Possible follow-ups, deliberately not in scope:**

- Spawn the storage read as a detached task if the inline read is observed to
  stall the poller.
- Poke the storage read from `event::OpContext` after a delete batch, so the
  reclaimed space shows immediately.
- Warn in the UI when a share has the DSM Recycle Bin enabled and a delete
  therefore reclaims nothing — the open question already recorded in
  `CLAUDE.md`'s known gaps, which this band makes newly visible (the user will
  now *see* that space did not come back).
