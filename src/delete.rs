//! Delete-path resolution and the safety guards around it — **the dangerous
//! part of this program**.
//!
//! Every other module can be wrong and cost the user a redraw. This one decides
//! which directory on a NAS gets handed to a *recursive* File Station delete,
//! and there is no undo. The governing rule, from the plan and repeated in
//! `CLAUDE.md`, is therefore:
//!
//! > **Refuse rather than guess.**
//!
//! A refusal costs the user one skipped row and a message. A guess costs them
//! data. So every uncertainty below resolves to an [`Error::UnsafePath`], the
//! task is left completely untouched, and the batch carries on with the items
//! that *were* unambiguous.
//!
//! ## Resolution order
//!
//! `additional.detail.destination` is normally share-relative with no leading
//! slash (`downloads`, `video/movies`), though some configurations surface an
//! absolute `/volumeN/share/…`. File Station wants a path rooted at the share.
//! **The on-disk name is the task's `title`.** Download Station names what it
//! writes after the task: a container directory for a multi-file torrent, and
//! for a single-file torrent the title *is* the filename. The BitTorrent spec
//! says the same — `info.name` is the directory name for a multi-file torrent
//! and the file name for a single-file one, and DSM reports `info.name` as the
//! title.
//!
//! The **file list never contains that container**, so it cannot name the
//! payload. What it does is say what *shape* to expect inside:
//!
//! | file list | expectation | why |
//! |---|---|---|
//! | one entry, no separator | [`ExpectedKind::File`] | a single-file torrent; the title is that filename |
//! | anything else non-empty | [`ExpectedKind::Dir`] | a multi-file torrent, which Download Station gave a container |
//! | empty, on HTTP/FTP/NZB/eMule | [`ExpectedKind::AnyFromTitle`] | nothing said which; accept either |
//! | empty, on **BitTorrent** | **REFUSE** | a torrent always has a list, so its absence means this client does not understand the record — and nothing corroborates the shape. `--no-delete-files` still removes the task. See [`crate::model::TaskType::file_list_is_mandatory`]. |
//!
//! ⚠️ **This used to derive the name from the file list's common top-level
//! component, and that was wrong in a way that could delete the wrong
//! directory** rather than merely failing. Measured against a real DSM 7 NAS
//! over a 41-task library:
//!
//! * `{destination}/{title}` existed, with the expected kind, for **40 of the
//!   40** tasks that had a destination and a file list.
//! * The old rule refused **15** of them outright, because their entries shared
//!   no top-level component — 37% of the library, undeletable by this tool.
//! * For **2** more it produced a path that was simply wrong: two Blu-ray
//!   torrents list `BDMV/…`, so it aimed at `/video/BDMV` when the payload is
//!   `/video/{title}/BDMV/…`. Nothing existed at `/video/BDMV`, so they failed
//!   safely — but any unrelated `/video/BDMV` is what would have been
//!   recursively deleted instead.
//! * It agreed with this rule on the other 38 only because a single-file
//!   torrent's title *is* its filename.
//!
//! There is consequently no "provenance" any more: every name comes from the
//! same place, so how to read an *absent* path is decided by the task's own
//! counters ([`payload_should_exist`]) rather than by which rule named it.
//!
//! Resolution also records what *kind* of object it expects to find
//! ([`ExpectedKind`]), because a path is only the right path if the thing at it
//! is the thing that was resolved — see [`crate::event::decide_file_phase`].
//!
//! The destination is then normalized ([`normalize_destination`]) and joined as
//! `/{destination}/{name}`.
//!
//! ## Guards
//!
//! [`validate_path`] rejects the syntactic hazards the plan enumerates (empty,
//! `/`, fewer than two components, a `..` component, an empty or `.` name, no
//! leading slash) plus two the plan does not, both of which turn a merely wrong
//! path into a *share-destroying* one if anything downstream normalizes it:
//!
//! * **Control characters.** A NUL is the interesting one: C string handling
//!   truncates at it, so `/downloads\0/Some.Torrent` would arrive as
//!   `/downloads` — the share root, deleted recursively.
//! * **Blank components.** If any layer trims whitespace, `/   /Some.Torrent`
//!   becomes `/Some.Torrent`, which is again a share root rather than a task
//!   directory. A component that is *entirely* whitespace has no legitimate
//!   use; incidental leading/trailing spaces inside a real name are left alone.
//!
//! The *semantic* guard (a `SYNO.FileStation.List` `getinfo` existence check
//! before the delete) belongs to the executor in [`crate::event::spawn_delete`]
//! — it needs the network. Everything here is pure.
//!
//! ## Snapshot semantics
//!
//! [`DeletePlan`] owns copies of everything it needs: id, title, size, status
//! and the resolved path, all taken at the instant the confirmation dialog
//! opens. It borrows nothing from the task list, so a background refresh
//! landing mid-dialog cannot change what the user is about to confirm.
//! (`App::apply_tasks` discards refreshes outright while `Mode::Confirm` is
//! active — belt *and* braces, because the failure mode here is deleting
//! something the user never read. The poller itself keeps ticking; it is the
//! *application* of a tick that is suppressed.)
//!
//! The snapshot's [`DeleteItem::status`], though, is only good for *display*.
//! It is as old as the dialog plus the item's place in the batch queue, so the
//! executor never decides whether to pause from it — see [`plan_delete_ops`].

use crate::config::{DEFAULT_DELETE_FILES, ResolvedConfig};
use crate::error::{Error, Result};
use crate::model::{Task, TaskFile, TaskStatus};

/// What a confirmed delete is allowed to do, resolved from the config and the
/// CLI (`delete_files`, `--no-delete-files`, `--dry-run`).
///
/// It rides alongside the [`DeletePlan`] rather than inside it: the plan is a
/// snapshot of *which* tasks are involved, and these two flags are a property of
/// the whole session. The confirmation dialog has to state both — a user who
/// configured `delete_files = false` and one who did not are being asked
/// materially different questions, and a dry run is not a question at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteOptions {
    /// Delete the payload on the volume as well as the DSM task.
    ///
    /// `false` issues no File Station call at all, so this program removes
    /// nothing from the volume — but it is **not** a promise that the files
    /// survive. The task delete sends `force_complete=false`, DSM's "do not
    /// keep the uncompleted download files", so DSM itself discards the partial
    /// data of a task it does not consider complete. A completed task's payload
    /// stays exactly where it is; an unfinished one's goes with the task. See
    /// [`payload_survives_task_delete`], which is what the confirmation dialog
    /// words each row from.
    pub delete_files: bool,
    /// Log the intended operations and issue **no** destructive call.
    pub dry_run: bool,
}

impl Default for DeleteOptions {
    /// Deleting the files as well as the task is the entire point of the tool,
    /// so that is the default — but a dry run never is: it has to be asked for.
    fn default() -> Self {
        Self {
            delete_files: DEFAULT_DELETE_FILES,
            dry_run: false,
        }
    }
}

impl DeleteOptions {
    /// The two delete-affecting settings of a merged configuration.
    pub fn from_config(config: &ResolvedConfig) -> Self {
        Self {
            delete_files: config.delete_files,
            dry_run: config.dry_run,
        }
    }

    /// A dry run: everything is described, nothing is removed.
    pub fn dry_run() -> Self {
        Self {
            dry_run: true,
            ..Self::default()
        }
    }
}

/// The shared top-level component of a torrent's file list, if there is
/// exactly one.
///
/// `additional.file[].filename` is a path *relative to the task's on-disk
/// root*, so the first component of every entry is the directory Download
/// Station created — or, for a single-file torrent, the file itself.
///
/// Returns `None` — meaning **refuse**, never "guess from the title" — when:
///
/// * the list is empty (the caller distinguishes this case first; see
///   [`resolve_delete_target`]),
/// * two entries disagree about their first component (comparison is exact:
///   the NAS filesystem is case-sensitive, and two roots differing only in case
///   are two directories), or
/// * any entry has no usable first component — an empty `filename`, or an
///   unexpected absolute one, whose leading `/` would otherwise make this
///   function report the *volume* as the shared root.
pub fn common_root(files: &[TaskFile]) -> Option<String> {
    let mut root: Option<&str> = None;
    for file in files {
        let first = first_component(&file.filename)?;
        match root {
            None => root = Some(first),
            Some(seen) if seen == first => {}
            Some(_) => return None,
        }
    }
    root.map(str::to_string)
}

/// The first `/`-separated component of a relative path, or `None` when there
/// is not a usable one.
fn first_component(filename: &str) -> Option<&str> {
    match filename.split('/').next() {
        Some("") | None => None,
        Some(first) => Some(first),
    }
}

/// Turn `additional.detail.destination` into a share-rooted, slash-free
/// fragment: `/volume1/downloads/` and `downloads` both become `downloads`.
///
/// Only an **absolute** destination has its mount point stripped. A relative
/// destination that merely *starts with* the text `volume1` is left alone: a
/// share may legally be named that, and mangling a relative path is how a
/// delete ends up one directory away from where it was aimed.
///
/// Every DSM mount point is recognized, not just `/volumeN`: internal volumes
/// (`/volume1`), USB and eSATA shares (`/volumeUSB1/usbshare1-2`,
/// `/volumeSATA1/…`) and the bare `/volume/…` some builds report. Leaving one
/// of those in place produced a File Station path that cannot exist
/// (`/volumeUSB1/usbshare1-2/x`) — and "it fails the existence check later" is
/// not the harmless outcome it sounds like, since an absent path is one of the
/// answers the executor is allowed to read as "already cleaned up". Recognition
/// is by **shape** ([`is_volume_component`]), not by a `volume` prefix: an
/// absolute first component that is not a mount point is passed through
/// untouched, because a share-rooted `/downloads` — or `/volumes/movies` — is a
/// legitimate destination.
pub fn normalize_destination(destination: &str) -> String {
    strip_volume_prefix(destination)
        .trim_matches('/')
        .to_string()
}

/// Drop a leading volume mount component from an absolute destination.
fn strip_volume_prefix(destination: &str) -> &str {
    let Some(rest) = destination.strip_prefix('/') else {
        return destination;
    };
    let (first, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if is_volume_component(first) {
        tail
    } else {
        destination
    }
}

/// True for every DSM mount point spelling — `volume`, `volume1`, `volume12`,
/// `volumeUSB1`, `volumeSATA2`; false for `video`, `vol1`, and for a *share*
/// whose name merely starts with the text: `volumes`, `volume-media`,
/// `volume_archive`, `volumeX`.
///
/// The **shape** is matched, not the prefix, because the first component of an
/// absolute destination is not always a mount point: this module's contract
/// explicitly allows a share-rooted `/downloads`, so a share-rooted
/// `/volumes/movies` reaches here too. Eating that first component would re-root
/// a *recursive* delete into a different share — `/movies/<name>`, which is
/// either an unrelated directory or (for a non-finished task) an absent path the
/// executor is allowed to read as "already cleaned up", orphaning the payload.
/// Every real mount is `volume`, `volume<N>`, `volumeUSB<N>` or `volumeSATA<N>`,
/// so nothing legitimate is lost by insisting on it.
fn is_volume_component(component: &str) -> bool {
    let Some(suffix) = component.strip_prefix("volume") else {
        return false;
    };
    // The bare `/volume/…` some builds report.
    if suffix.is_empty() {
        return true;
    }
    let index = suffix
        .strip_prefix("USB")
        .or_else(|| suffix.strip_prefix("SATA"))
        .unwrap_or(suffix);
    !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// What kind of object the resolution expects to find at the path.
///
/// The existence check answers "is there something here"; this is what makes
/// "…and is it the thing we resolved" answerable too. A `FileList` root derived
/// from a multi-file torrent names a **directory**; a single flat entry names
/// the downloaded **file** itself. If the path is a file where a directory was
/// expected — or a directory where a file was — the path is not this task's
/// payload, and `recursive=true` would then remove whatever *is* there.
///
/// **There are two ways not to know the kind, and they are not the same
/// answer.** Whether an indeterminate expectation is permissive is a property
/// of where the name came from, not of the indeterminacy: no metadata to
/// consult is a reason to accept what is there, metadata that says something
/// self-contradictory is a reason to refuse. The two therefore have separate
/// variants rather than a shared `Unknown` that a caller has to remember to
/// qualify elsewhere.
///
/// See [`crate::event::decide_file_phase`], which turns a mismatch — and an
/// indeterminate expectation over a path that exists — into a failed item
/// rather than a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedKind {
    /// The task wrote a directory: the file list has more than one entry, or a
    /// single entry with a path separator in it.
    Dir,
    /// The task wrote one file with no enclosing directory: exactly one flat
    /// file-list entry.
    File,
    /// **Nothing DSM sent describes the shape, because there was nothing to
    /// send.** The name came from the title (rule 3): an HTTP download writes a
    /// file, an NZB task's destination is usually a directory, and DSM's `list`
    /// response distinguishes neither.
    ///
    /// Deliberately permissive — either kind is accepted — because the
    /// alternative is refusing every task rule 3 exists for on a guess about
    /// DSM's unpack behaviour. The mismatch check is not the only guard on this
    /// route: a title-named path that is *absent* already fails hard
    /// (`event::decide_file_phase`), and both routes name `--no-delete-files`.
    /// The kind that was found is logged so a surprise is visible.
    AnyFromTitle,
    /// **Metadata that should have determined the kind did not.** A file list
    /// whose flat entries repeat one identical filename says something DSM
    /// should never say, and a refused item resolved nothing at all.
    ///
    /// The opposite of [`ExpectedKind::AnyFromTitle`], and deliberately so:
    /// here there *is* a file list, so "either kind will do" would be reading a
    /// malformed answer as permission. Nothing at the path can be checked
    /// against anything, and the call that would follow is a recursive delete,
    /// so it is refused — `--no-delete-files` still removes the task.
    Indeterminate,
}

impl ExpectedKind {
    /// Whether an object of the kind the NAS reported can be this task's
    /// payload.
    ///
    /// [`ExpectedKind::AnyFromTitle`] accepts both and
    /// [`ExpectedKind::Indeterminate`] accepts neither — see the variant docs.
    /// A caller therefore cannot authorize a delete off an indeterminate
    /// expectation by forgetting to ask where the name came from.
    pub fn accepts(self, is_dir: bool) -> bool {
        match self {
            ExpectedKind::Dir => is_dir,
            ExpectedKind::File => !is_dir,
            ExpectedKind::AnyFromTitle => true,
            ExpectedKind::Indeterminate => false,
        }
    }

    /// How the expectation reads in a refusal message.
    ///
    /// [`ExpectedKind::Indeterminate`] has no shape to name, and the refusal it
    /// produces says so in its own words rather than through this.
    pub fn label(self) -> &'static str {
        match self {
            ExpectedKind::Dir => "a directory",
            ExpectedKind::File => "a file",
            ExpectedKind::AnyFromTitle => "either a file or a directory",
            ExpectedKind::Indeterminate => "something its file list does not describe",
        }
    }
}

/// What a task's file list says the on-disk root must be.
///
/// Any entry with a separator means the root encloses something, so it is a
/// directory. Exactly one flat entry is the downloaded file itself. Several
/// entries that are all flat can only happen when they share one *identical*
/// filename — a shape DSM should never send — and that is
/// [`ExpectedKind::Indeterminate`]: the list is metadata, it was consulted, and
/// what it said does not describe a payload. Guessing either way would let a
/// malformed answer authorize the recursive delete.
fn expected_kind_of(files: &[TaskFile]) -> ExpectedKind {
    match files {
        // No list to consult. Only reachable for the task types that
        // legitimately have none (HTTP/FTP/NZB/eMule) — a torrent without one is
        // refused in `resolve_name` — and for those nothing says whether the
        // payload is a file or a directory, so accept either.
        [] => ExpectedKind::AnyFromTitle,
        // Exactly one entry, no separator: a single-file torrent, whose title
        // *is* that filename. The payload is the file itself, not a container.
        [only] if !only.filename.contains('/') => ExpectedKind::File,
        // Anything else is a multi-file torrent, and Download Station wrote it
        // into a container directory named after the task. That holds whether
        // the entries are loose (`e01.mkv`, `e02.mkv`) or nested (`BDMV/…`) —
        // the container is never itself in the list.
        _ => ExpectedKind::Dir,
    }
}

/// A resolved delete target: where the payload is, how that was worked out,
/// and what should be found there.
///
/// The three travel together because the executor needs all three to read the
/// existence check: the path to look up and [`ExpectedKind`] to know whether a
/// *present* answer is even the right object. Whether an *absent* one is benign
/// is [`payload_should_exist`]'s question, asked of the task's own counters
/// rather than of where the name came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub path: String,
    pub expected_kind: ExpectedKind,
}

/// The absolute File Station path holding a task's data, and the rule that
/// named it.
///
/// This is the only function permitted to answer "what does deleting this task
/// remove from the volume" — [`DeleteItem::for_task`] is its one production
/// caller — and it answers with an error far more readily than with a path. See
/// the module docs for the resolution order; the short version is that the name
/// is the task's title, the file list says what shape to expect there, and a
/// torrent that reports no file list at all is refused.
///
/// The returned path has already been through [`validate_path`]. It is
/// re-validated immediately before the File Station call anyway — the check is
/// free and the value crosses a task boundary in between.
pub fn resolve_delete_target(task: &Task) -> Result<ResolvedTarget> {
    let (name, expected_kind) = resolve_name(task)?;

    let destination = normalize_destination(&task.destination);
    if destination.is_empty() {
        // No destination means no share to root the path at, and `/{name}`
        // would name a share rather than a directory inside one.
        return Err(Error::unsafe_path(
            &task.title,
            "the task reports no destination, so its on-disk location is unknown",
        ));
    }

    let path = format!("/{destination}/{name}");
    validate_path(&path)?;
    Ok(ResolvedTarget {
        path,
        expected_kind,
    })
}

/// The on-disk name of a task's payload, and the shape to expect there.
///
/// **It is always the title.** Download Station names what it writes after the
/// task: for a multi-file torrent that is a container directory, and for a
/// single-file torrent the title *is* the filename. The BitTorrent spec agrees —
/// `info.name` is the directory for a multi-file torrent and the file name for a
/// single-file one, and DSM reports `info.name` as the title.
///
/// The **file list never contains that container**, so it cannot name the
/// payload; it only says what shape to expect. Verified against a real DSM 7 NAS
/// over a 41-task library: `{destination}/{title}` existed for **40 of the 40**
/// tasks that had both a destination and a file list.
///
/// ⚠️ This used to derive the name from the file list's common top-level
/// component, and that was **wrong in a way that could have deleted the wrong
/// directory** rather than merely failing. Two Blu-ray torrents in that library
/// list `BDMV/…`, so the old rule resolved them to `/video/BDMV` when the payload
/// is really `/video/{title}/BDMV/…`. Nothing existed at `/video/BDMV`, so they
/// failed safely — but any unrelated `/video/BDMV` is what would have been
/// recursively deleted instead. The old rule agreed with this one on 38 of 40
/// only because a single-file torrent's title *is* its filename.
fn resolve_name(task: &Task) -> Result<(String, ExpectedKind)> {
    // A torrent always has a file list, so one arriving without it is an
    // anomaly. The path would still be `{destination}/{title}`, but nothing
    // corroborates the shape and nothing shows DSM had the metadata at all, so
    // refuse rather than aim a recursive delete using a task record this client
    // evidently does not understand. `--no-delete-files` still removes the task.
    if task.files.is_empty() && task.task_type.file_list_is_mandatory() {
        return Err(Error::unsafe_path(
            &task.title,
            format!(
                "this {} task reports no files, though a torrent always has a file list, \
                 so what it wrote cannot be confirmed; refusing to aim a recursive delete \
                 at it (use --no-delete-files to remove the task without touching the \
                 volume)",
                task.task_type
            ),
        ));
    }

    validate_name(&task.title)?;
    Ok((task.title.clone(), expected_kind_of(&task.files)))
}

/// Guard the single path component a task's data lives under, before it is
/// joined to anything.
///
/// [`validate_path`] would catch most of this afterwards; checking here means
/// the *title* fallback cannot smuggle a separator into what is supposed to be
/// one component (`"Some/Release"` would silently delete one level deeper than
/// the task's actual directory), and the error names the offending value rather
/// than the assembled path.
fn validate_name(name: &str) -> Result<()> {
    let refuse = |reason: &str| Err(Error::unsafe_path(name, reason));

    if name.trim().is_empty() {
        return refuse("the task has no usable on-disk name");
    }
    if name.contains('/') {
        return refuse("the on-disk name contains a path separator");
    }
    if name == "." || name == ".." {
        return refuse("the on-disk name is a relative-path component");
    }
    if name.chars().any(char::is_control) {
        return refuse("the on-disk name contains a control character");
    }
    Ok(())
}

/// The syntactic guard. A path that does not pass this is never sent to File
/// Station, and the task it came from is left alone.
///
/// Rejected:
///
/// * empty, or not starting with `/` — File Station paths are absolute
/// * `/` itself, or fewer than two components: **one component is a share
///   root**, and recursively deleting `/downloads` is the worst outcome this
///   program has
/// * any empty or whitespace-only component (`//`, a trailing `/`, `/   /`)
/// * any `.` or `..` component, anywhere — not just at the end
/// * any control character, `\0` above all
pub fn validate_path(path: &str) -> Result<()> {
    let refuse = |reason: &str| Err(Error::unsafe_path(path, reason));

    if path.is_empty() {
        return refuse("the path is empty");
    }
    if !path.starts_with('/') {
        return refuse("the path is not absolute");
    }
    // Before anything splits or trims: a NUL truncates the path in any C-based
    // consumer, which turns "/share/task" into "/share".
    if path.chars().any(char::is_control) {
        return refuse("the path contains a control character");
    }
    if path == "/" {
        return refuse("the path is the filesystem root");
    }

    let components: Vec<&str> = path[1..].split('/').collect();

    if components
        .iter()
        .any(|component| component.trim().is_empty())
    {
        return refuse("the path has an empty or blank component");
    }
    if components.contains(&"..") {
        return refuse("the path contains a `..` component");
    }
    if components.contains(&".") {
        return refuse("the path contains a `.` component");
    }
    if components.len() < 2 {
        return refuse("the path is a share root, not a directory inside one");
    }

    Ok(())
}

/// What the executor should do with one snapshotted task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A resolved, guard-checked absolute File Station path.
    Path(String),
    /// Resolution refused. The task is left **entirely** untouched — no pause,
    /// no file delete, no task delete — and the reason is shown to the user.
    Refused(String),
}

/// One task, frozen at the moment the confirmation dialog opened.
///
/// Everything is owned: the snapshot must not change under the user while they
/// read it, and must not change under the executor while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteItem {
    pub id: String,
    pub title: String,
    /// Task size as DSM reported it — what the user is told they will reclaim.
    pub size: u64,
    /// Bytes DSM says have been written, at snapshot time. Carried beside
    /// [`Self::status`] because status alone is a poor proxy for "did this task
    /// write a payload" — see [`payload_should_exist`].
    pub downloaded: u64,
    /// Status **at snapshot time**.
    ///
    /// It is deliberately *not* what decides whether the task is paused before
    /// its files go: by the time the executor reaches this item the value is as
    /// old as the confirmation dialog plus the item's place in the batch queue,
    /// and a task that DSM's bandwidth schedule resumed in that window would be
    /// written into mid-delete. The live check lives in
    /// `event::pause_and_confirm`.
    ///
    /// It is not what decides whether an **absent** path is benign either,
    /// whenever a live read is available: the pause phase fetches the task's
    /// current state one instant earlier, and `event::decide_file_phase` uses
    /// *that* ([`PayloadState`]). Staleness bites in a direction that matters
    /// here — a task that finished while the dialog was up still reads as
    /// `Downloading` in this snapshot, and an absent payload would then be
    /// waved through as cleaned-up partial data. This value is the **fallback**
    /// for the one case with no live read: `delete_files = false`, where no
    /// path is looked at at all.
    pub status: TaskStatus,
    pub target: Target,
    /// Whether resolution produced a path at all — `false` for a refused item.
    ///
    /// The one thing the file phase still needs to know beyond the path itself:
    /// "nothing named this path" authorizes nothing, whatever the counters say.
    /// There used to be a `NameSource` here distinguishing a name read from the
    /// file list from one taken from the title, because the latter was a guess
    /// worth refusing over. That distinction is gone: the name is *always* the
    /// title, and the title is what Download Station actually writes.
    pub named: bool,
    /// What should be found at the path. [`ExpectedKind::Indeterminate`] — the
    /// variant that authorizes nothing — for a refused item, which resolved no
    /// path to find anything at.
    pub expected_kind: ExpectedKind,
}

impl DeleteItem {
    /// Resolve one task into a snapshot item. A refusal is recorded on the
    /// item rather than returned, so one bad torrent never aborts the batch.
    fn for_task(task: &Task) -> Self {
        let (target, named, expected_kind) = match resolve_delete_target(task) {
            Ok(resolved) => (Target::Path(resolved.path), true, resolved.expected_kind),
            // A refused item carries the expectation that accepts *nothing*.
            // It has no path, so no lookup can reach it — but the permissive
            // expectation belongs to the title fallback alone, and a refusal is
            // the furthest thing from "whatever is there will do".
            Err(Error::UnsafePath { reason, .. }) => {
                (Target::Refused(reason), false, ExpectedKind::Indeterminate)
            }
            // `resolve_delete_target` only produces `UnsafePath` today; anything
            // else is still a refusal, never a fallthrough to deletion.
            Err(other) => (
                Target::Refused(other.to_string()),
                false,
                ExpectedKind::Indeterminate,
            ),
        };
        DeleteItem {
            id: task.id.clone(),
            title: task.title.clone(),
            size: task.size,
            downloaded: task.downloaded,
            status: task.status.clone(),
            target,
            named,
            expected_kind,
        }
    }

    /// What this item's snapshot says about whether its payload was written.
    ///
    /// The **fallback** input to [`payload_should_exist`]: the executor prefers
    /// the live read from the pause phase, and only falls back to this when
    /// there was none. See [`Self::status`].
    pub fn payload_state(&self) -> PayloadState {
        PayloadState {
            status: self.status.clone(),
            downloaded: self.downloaded,
            size: self.size,
        }
    }

    /// The path to delete, or `None` when this item was refused.
    pub fn path(&self) -> Option<&str> {
        match &self.target {
            Target::Path(path) => Some(path),
            Target::Refused(_) => None,
        }
    }

    /// Why this item will be skipped, or `None` when it will be deleted.
    pub fn refusal(&self) -> Option<&str> {
        match &self.target {
            Target::Refused(reason) => Some(reason),
            Target::Path(_) => None,
        }
    }

    /// True when this item will be skipped.
    pub fn is_refused(&self) -> bool {
        self.refusal().is_some()
    }
}

/// An owned snapshot of everything a single `d` press will act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
    pub items: Vec<DeleteItem>,
}

impl DeletePlan {
    /// Freeze a set of tasks into a plan, resolving each one's path now.
    ///
    /// Takes an iterator of borrowed tasks (`app.selected_tasks()`, or a single
    /// cursor row) and keeps nothing borrowed afterwards.
    pub fn snapshot<'a>(tasks: impl IntoIterator<Item = &'a Task>) -> Self {
        DeletePlan {
            items: tasks.into_iter().map(DeleteItem::for_task).collect(),
        }
    }

    /// Every item, refused ones included.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when there is nothing to confirm — no dialog should open.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items that resolved to a path, in snapshot order.
    pub fn deletable(&self) -> impl Iterator<Item = &DeleteItem> {
        self.items.iter().filter(|item| !item.is_refused())
    }

    /// The items that were refused, in snapshot order.
    pub fn refused(&self) -> impl Iterator<Item = &DeleteItem> {
        self.items.iter().filter(|item| item.is_refused())
    }

    /// Bytes this plan will actually free — refused items are **excluded**,
    /// since nothing of theirs is removed.
    pub fn total_size(&self) -> u64 {
        self.deletable().map(|item| item.size).sum()
    }
}

// ---------------------------------------------------------------------------
// Op ordering
// ---------------------------------------------------------------------------

/// One phase of the three-phase delete.
///
/// The executor issues these **in order** and stops at the first failure; see
/// [`plan_delete_ops`] for why the order is what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Stop the task, so Download Station is neither holding file handles nor
    /// re-creating directories underneath the delete.
    Pause,
    /// Existence-check and then recursively remove this path.
    DeleteFiles(String),
    /// Remove the DSM task. **Last**, always.
    DeleteTask,
}

impl Op {
    /// How the phase reads in a log line or a dry-run report.
    pub fn describe(&self) -> String {
        match self {
            Op::Pause => "pause the task".to_string(),
            Op::DeleteFiles(path) => format!("delete {path}"),
            Op::DeleteTask => "delete the DSM task".to_string(),
        }
    }
}

/// A whole op list in one line — what `--dry-run` reports it *would* do.
pub fn describe_ops(ops: &[Op]) -> String {
    if ops.is_empty() {
        return "nothing".to_string();
    }
    ops.iter()
        .map(Op::describe)
        .collect::<Vec<_>>()
        .join(", then ")
}

/// Whether a task must be paused before its files can be removed.
///
/// The plan's table names six statuses that need a pause (downloading,
/// seeding, waiting, finishing, hash-checking, extracting) and three that do
/// not (paused, finished, error). It names ten statuses in total, so
/// `filehosting_waiting` — and any status this client does not recognize — is
/// unclassified. Both are treated as **active**: pausing something that is
/// already idle costs one round trip, while failing to pause something that is
/// live risks Download Station writing into the directory as it is being
/// deleted, or re-creating it afterwards.
///
/// ⚠️ Only ever called on a **live** status read at execution time
/// (`event::pause_and_confirm`), never on [`DeleteItem::status`].
pub fn requires_pause(status: &TaskStatus) -> bool {
    !matches!(
        status,
        TaskStatus::Paused | TaskStatus::Finished | TaskStatus::Error
    )
}

/// What a task says about whether its payload was ever written — status *and*
/// the transfer counters, from whichever read is freshest.
///
/// A separate type because the executor must assemble it from the freshest
/// evidence for each half rather than from one read: `event::PauseRead` folds
/// each half across every read of the task — starting from
/// [`DeleteItem::payload_state`], the confirmation snapshot, and ratcheting both
/// toward "the payload must exist". Passing the fields around loose is how the
/// stale one gets used by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadState {
    pub status: TaskStatus,
    /// Bytes written, as DSM's `transfer` block reports them.
    pub downloaded: u64,
    /// Total size, as DSM reports it.
    pub size: u64,
}

impl PayloadState {
    /// Everything one read of a task says, taken together.
    ///
    /// The whole state from a single moment, which is what the confirmation
    /// dialog asks [`payload_survives_task_delete`] about. The delete executor
    /// does **not** use this: it must mix two moments, and
    /// `event::payload_for_file_phase` is where that happens.
    pub fn of_task(task: &Task) -> Self {
        PayloadState {
            status: task.status.clone(),
            downloaded: task.downloaded,
            size: task.size,
        }
    }

    /// Whether DSM's counters say the whole payload was written.
    ///
    /// `downloaded >= size` rather than `==`: a BT task that re-downloads
    /// pieces reports more than its own size, which [`Task::progress`] already
    /// has to clamp. `size > 0` guards the other end — a task DSM reports as
    /// zero-sized (an unparseable `size`, or one it has not learned yet) has
    /// `0 >= 0` true for free, and that must not be read as evidence of
    /// anything.
    pub fn fully_downloaded(&self) -> bool {
        self.size > 0 && self.downloaded >= self.size
    }
}

/// Whether a task in this state **must** have its payload on the volume.
///
/// This is what makes an *absent* resolved path readable. Download Station
/// removes its own partial data when an incomplete task goes away, so finding
/// nothing at the path of a downloading, waiting, paused or errored task is
/// ordinary — that is the case the plan's "Missing ⇒ still delete the task"
/// rule was written for. A task that **finished** is a different statement: its
/// payload demonstrably existed, so an absent path does not say "somebody
/// tidied up", it says *this program is looking in the wrong place* — a
/// mis-resolved destination, most likely — and deleting the DSM task on that
/// evidence orphans the very data the user wanted reclaimed.
///
/// `Extracting` counts as finished: DSM unpacks into the destination, so the
/// payload is there. `Finishing` does not — the data may still be in a temp
/// location being moved into place.
///
/// **Status is not the only evidence.** A task paused at 100%, or one that
/// errored *after* its download completed, is in none of those three states and
/// still has a full payload on disk — status says what the task is doing, not
/// what it has written. So a task DSM's counters call fully downloaded
/// ([`PayloadState::fully_downloaded`]) qualifies whatever its status is. The
/// threshold is the conservative one — the *whole* payload, not a fraction:
/// a partially downloaded task genuinely does have partial data that Download
/// Station cleans up after itself, and treating "most of it" as "must be there"
/// would fail items whose absence is ordinary. Anything this refuses is still
/// removable with `--no-delete-files`.
pub fn payload_should_exist(state: &PayloadState) -> bool {
    status_implies_payload(&state.status) || state.fully_downloaded()
}

/// The status arm of [`payload_should_exist`], on its own.
///
/// Named separately because `event::PauseRead` ratchets the status it hands the
/// file phase along exactly this ordering: any later read — including the very
/// first live one, taken after the confirmation snapshot seeded the fold —
/// replaces the status held only when its own is one of these. That is what
/// keeps a status *upgrade* (the task finished mid-pause) while rejecting every
/// *downgrade*, among them the one this program inflicts on itself (`Paused`,
/// which is not here). Asking [`payload_should_exist`] instead would let the
/// counters answer a question about the status.
pub fn status_implies_payload(status: &TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Finished | TaskStatus::Seeding | TaskStatus::Extracting
    )
}

/// Whether removing **only the DSM task** leaves this task's data on the volume.
///
/// The question `--no-delete-files` raises. That mode issues nothing but
/// [`Op::DeleteTask`], and `download_station::build_delete_params` sends
/// `force_complete=false` — DSM's "do *not* keep the uncompleted download
/// files". For a task DSM considers complete there is nothing uncompleted to
/// throw away and the payload stays exactly where it is; for one still
/// downloading, waiting, paused or errored, DSM discards the partial data along
/// with the task.
///
/// So the confirmation dialog must **not** promise those rows that their files
/// are left in place. Same rule as [`payload_should_exist`] — a task whose
/// payload must exist is a task DSM considers complete — named separately
/// because it answers a different question and the two must be free to diverge
/// if DSM's behaviour ever does.
pub fn payload_survives_task_delete(state: &PayloadState) -> bool {
    payload_should_exist(state)
}

/// The ordered phases for one snapshotted item — the plan's ordering table,
/// expressed as pure data so it can be tested without a NAS.
///
/// The order exists for **recoverability**. Files first, task last: if the file
/// delete fails, the task survives still pointing at its data, so nothing is
/// orphaned and the user can retry. Reversing them would leave a volume full of
/// directories nothing references.
///
/// Two cases produce no ops at all or a shortened list:
///
/// * **A refused item gets an empty list — but only while files are being
///   deleted.** `Target::Refused` means the on-disk location could not be
///   determined; removing the DSM task on top of a *recursive delete that could
///   not be aimed* would silently orphan exactly the data whose location is in
///   doubt. Under `delete_files = false` there is no recursive delete and the
///   path is never used at all, so the refusal has nothing to protect: those
///   tasks are removed like any other. They are precisely the tasks
///   `--no-delete-files` exists for — before this, a torrent whose file list has
///   several top-level roots could not be removed by this tool by any route.
/// * **`delete_files = false` drops the file phase — and the pause with it.**
///   The pause exists only to keep Download Station out of the way of the file
///   delete; with no file delete there is nothing to keep it out of, and a
///   pause that failed would then block a task-only removal for no reason.
///
/// ⚠️ **The pause phase is unconditional whenever files are being deleted**, and
/// is *not* filtered by [`requires_pause`] here. [`DeleteItem::status`] is a
/// snapshot taken when the dialog opened; a task that DSM's bandwidth schedule
/// resumed while the user was reading it would come out of this function with
/// no pause at all, and File Station would then recurse through a directory
/// Download Station is actively writing into. The executor issues the phase and
/// resolves it against a **live** status read (`event::pause_and_confirm`),
/// which costs one `getinfo` for a task that turns out to be idle and skips the
/// pause call itself.
///
/// `dry_run` deliberately does **not** shorten the list: a dry run has to be
/// able to report the operations it is declining to perform.
pub fn plan_delete_ops(item: &DeleteItem, options: DeleteOptions) -> Vec<Op> {
    if !options.delete_files {
        // No path is used, so a path that could not be resolved is no reason to
        // refuse: `--no-delete-files` removes the row and nothing else.
        return vec![Op::DeleteTask];
    }

    let Some(path) = item.path() else {
        return Vec::new();
    };

    vec![Op::Pause, Op::DeleteFiles(path.to_string()), Op::DeleteTask]
}

/// Whether this item will be acted on at all, given the session's options.
///
/// The confirmation dialog and the executor must agree about this — a dialog
/// that says `SKIPPED` for a row the executor then deletes (or the reverse) is
/// worse than either behaviour on its own — so both read it from
/// [`plan_delete_ops`], which is the only place the rule lives.
pub fn will_act(item: &DeleteItem, options: DeleteOptions) -> bool {
    !plan_delete_ops(item, options).is_empty()
}

/// The phases an executor **does not** issue because the one at `failed_at`
/// failed.
///
/// The other half of the ordering rule, and the reason the phases are ordered
/// at all: a failed phase cancels every later phase. A pause that fails cancels
/// both deletes; a file delete that fails cancels the task delete. The task
/// then survives still pointing at its data.
pub fn ops_cancelled_by(ops: &[Op], failed_at: usize) -> &[Op] {
    ops.get(failed_at + 1..).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedConfig;
    use crate::model::TaskType;
    use crate::testutil::{fixture_task as task, fixture_tasks};

    /// The path half of [`resolve_delete_target`], which is all most of the
    /// resolution tests below are asking about.
    fn resolve_delete_path(task: &Task) -> Result<String> {
        resolve_delete_target(task).map(|resolved| resolved.path)
    }

    /// A minimal synthetic task; the fields each test cares about are
    /// overwritten with struct-update syntax.
    ///
    /// **HTTP**, not BitTorrent: it carries no file list, and for a torrent that
    /// is itself a refusal (rule 4). The tests that hand it a `files` vector
    /// override the type where the distinction matters.
    use crate::testutil::bare_task as bare;

    fn file(filename: &str) -> TaskFile {
        TaskFile {
            filename: filename.to_string(),
            size: 1,
            priority: "normal".to_string(),
            wanted: true,
        }
    }

    /// Assert a refusal, and hand back the reason so the caller can check it
    /// says something useful.
    #[track_caller]
    fn refusal(result: Result<String>) -> String {
        match result {
            Ok(path) => panic!("expected a refusal, resolved to {path:?}"),
            Err(Error::UnsafePath { reason, .. }) => {
                assert!(!reason.is_empty(), "a refusal must explain itself");
                reason
            }
            Err(other) => panic!("expected Error::UnsafePath, got {other:?}"),
        }
    }

    #[track_caller]
    fn rejected(path: &str) -> String {
        match validate_path(path) {
            Ok(()) => panic!("{path:?} should have been rejected"),
            Err(Error::UnsafePath { reason, .. }) => reason,
            Err(other) => panic!("expected Error::UnsafePath, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // common_root
    // -----------------------------------------------------------------------

    #[test]
    fn a_multi_file_torrent_resolves_to_its_shared_directory() {
        let files = [
            file("Some.Release/a.mkv"),
            file("Some.Release/b.nfo"),
            file("Some.Release/subs/en.srt"),
        ];
        assert_eq!(common_root(&files).as_deref(), Some("Some.Release"));
    }

    #[test]
    fn a_single_file_torrent_resolves_to_the_file_itself() {
        let files = [file("archlinux-2026.07.01-x86_64.iso")];
        assert_eq!(
            common_root(&files).as_deref(),
            Some("archlinux-2026.07.01-x86_64.iso")
        );
    }

    #[test]
    fn entries_with_different_top_level_components_have_no_common_root() {
        let files = [file("Disc1/track01.flac"), file("Disc2/track01.flac")];
        assert_eq!(common_root(&files), None);
    }

    #[test]
    fn a_loose_file_beside_a_directory_has_no_common_root() {
        let files = [file("Some.Release/a.mkv"), file("readme.nfo")];
        assert_eq!(common_root(&files), None);
    }

    #[test]
    fn roots_differing_only_in_case_are_two_directories_not_one() {
        // The NAS filesystem is case-sensitive; folding here would pick one of
        // two real directories at random.
        let files = [file("Some.Release/a.mkv"), file("some.release/b.mkv")];
        assert_eq!(common_root(&files), None);
    }

    #[test]
    fn an_empty_file_list_has_no_common_root() {
        assert_eq!(common_root(&[]), None);
    }

    #[test]
    fn an_entry_with_an_empty_filename_makes_the_list_unusable() {
        let files = [file("Some.Release/a.mkv"), file("")];
        assert_eq!(common_root(&files), None);
    }

    #[test]
    fn an_absolute_filename_never_reports_the_volume_as_the_root() {
        // Splitting "/volume1/downloads/X/a.mkv" naively yields "" or, worse,
        // "volume1". Neither is a torrent directory.
        let files = [file("/volume1/downloads/Some.Release/a.mkv")];
        assert_eq!(common_root(&files), None);
    }

    #[test]
    fn a_deselected_file_still_counts_towards_the_common_root() {
        // Deliberate: `selected` describes what was downloaded, not what is on
        // disk, and a list that disagrees with itself must refuse either way.
        let mut skipped = file("Other.Release/extra.mkv");
        skipped.wanted = false;
        let files = [file("Some.Release/a.mkv"), skipped];
        assert_eq!(common_root(&files), None);
    }

    // -----------------------------------------------------------------------
    // normalize_destination
    // -----------------------------------------------------------------------

    #[test]
    fn a_share_relative_destination_is_left_alone() {
        assert_eq!(normalize_destination("downloads"), "downloads");
        assert_eq!(normalize_destination("video/movies"), "video/movies");
    }

    #[test]
    fn a_leading_volume_component_is_stripped() {
        assert_eq!(normalize_destination("/volume1/downloads"), "downloads");
        assert_eq!(
            normalize_destination("/volume12/video/movies"),
            "video/movies"
        );
        assert_eq!(normalize_destination("/volume2/downloads/"), "downloads");
    }

    #[test]
    fn surrounding_slashes_are_trimmed() {
        assert_eq!(normalize_destination("/downloads"), "downloads");
        assert_eq!(normalize_destination("downloads/"), "downloads");
        assert_eq!(normalize_destination("///downloads///"), "downloads");
    }

    #[test]
    fn a_volume_root_normalizes_to_nothing() {
        assert_eq!(normalize_destination("/volume1"), "");
        assert_eq!(normalize_destination("/volume1/"), "");
        assert_eq!(normalize_destination(""), "");
        assert_eq!(normalize_destination("/"), "");
    }

    #[test]
    fn only_an_absolute_destination_has_its_volume_stripped() {
        // A share may legally be named "volume1"; mangling a relative path is
        // how a delete lands one directory away from where it was aimed.
        assert_eq!(
            normalize_destination("volume1/downloads"),
            "volume1/downloads"
        );
        assert_eq!(normalize_destination("volumeUSB1/x"), "volumeUSB1/x");
    }

    #[test]
    fn every_dsm_mount_point_spelling_is_stripped() {
        // USB and eSATA shares are the ones this used to miss, and the result
        // was a File Station path that cannot exist — which is *not* harmless,
        // because an absent path is one of the answers the executor may read as
        // "already cleaned up".
        assert_eq!(
            normalize_destination("/volumeUSB1/usbshare1-2/download"),
            "usbshare1-2/download"
        );
        assert_eq!(
            normalize_destination("/volumeSATA1/satashare1-1"),
            "satashare1-1"
        );
        assert_eq!(normalize_destination("/volume/downloads"), "downloads");
        assert_eq!(normalize_destination("/volume12/downloads"), "downloads");
    }

    #[test]
    fn an_absolute_destination_that_is_not_a_mount_point_is_left_alone() {
        // A share-rooted "/downloads" is a legitimate destination, and
        // swallowing its first component would aim the delete at a share that
        // does not exist.
        assert_eq!(normalize_destination("/video/movies"), "video/movies");
        assert_eq!(normalize_destination("/downloads"), "downloads");
        assert_eq!(normalize_destination("/vol1/downloads"), "vol1/downloads");
    }

    #[test]
    fn a_share_named_like_a_volume_keeps_its_first_component() {
        // The dangerous case: the first component of an *absolute* destination
        // is not always a mount point (this module accepts a share-rooted
        // "/downloads"), so a share called "volumes" must not be eaten —
        // "/volumes/movies" -> "movies" would re-root a recursive delete into a
        // different share entirely.
        assert_eq!(normalize_destination("/volumes/movies"), "volumes/movies");
        assert_eq!(normalize_destination("/volume-media/tv"), "volume-media/tv");
        assert_eq!(
            normalize_destination("/volume_archive/2019"),
            "volume_archive/2019"
        );
        assert_eq!(
            normalize_destination("/volumeX/downloads"),
            "volumeX/downloads"
        );
        // Mount-point spellings that exist have an index; these do not.
        assert_eq!(normalize_destination("/volumeUSB/x"), "volumeUSB/x");
        assert_eq!(normalize_destination("/volumeSATA/x"), "volumeSATA/x");
        assert_eq!(normalize_destination("/volume1a/x"), "volume1a/x");
    }

    #[test]
    fn a_volume_shaped_share_resolves_to_a_path_inside_itself() {
        // End to end, because the failure this guards is a *resolved path* one
        // share to the left of where it was aimed.
        let task = Task {
            destination: "/volumes/movies".to_string(),
            files: vec![file("Some.Release/a.mkv")],
            ..bare()
        };
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/volumes/movies/Some.Release"
        );
    }

    #[test]
    fn a_usb_destination_resolves_to_a_share_rooted_path() {
        // End to end, because the join is where the missed prefix showed up:
        // "/volumeUSB1/usbshare1-2/download/X" is a path File Station has never
        // heard of.
        let task = Task {
            destination: "/volumeUSB1/usbshare1-2/download".to_string(),
            files: vec![file("X/a.mkv")],
            ..bare()
        };
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/usbshare1-2/download/Some.Release"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_delete_path — the happy paths, against the real fixture
    // -----------------------------------------------------------------------

    #[test]
    fn a_multi_file_task_resolves_to_destination_plus_directory() {
        assert_eq!(
            resolve_delete_path(&task("dbid_001")).unwrap(),
            "/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64"
        );
    }

    #[test]
    fn a_single_file_task_resolves_to_the_file() {
        assert_eq!(
            resolve_delete_path(&task("dbid_003")).unwrap(),
            "/downloads/archlinux-2026.07.01-x86_64.iso"
        );
    }

    #[test]
    fn a_nested_destination_is_preserved() {
        let task = task("dbid_002");
        assert_eq!(task.destination, "video/movies");
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/video/movies/Big.Buck.Bunny.2008.1080p.🐰.BluRay.x264"
        );
    }

    #[test]
    fn the_title_wins_even_when_the_file_list_root_disagrees_with_it() {
        // The inverse of what this test used to assert, and the reason for the
        // change: Download Station writes into a container named after the
        // *task*, and the file list is relative to that container — so a root
        // that differs from the title is a directory *inside* the payload, not
        // the payload. dbid_006's list is rooted at a name without the audio
        // suffix; on a real NAS the payload is still at the title.
        //
        // Getting this backwards is not a near miss. Two real Blu-ray torrents
        // list `BDMV/…`, and the old rule aimed them at `/video/BDMV`.
        let task = task("dbid_006");
        assert_eq!(task.title, "千と千尋の神隠し.2001.1080p.日本語音声");
        assert_eq!(
            common_root(&task.files).as_deref(),
            Some("千と千尋の神隠し.2001.1080p")
        );

        let path = resolve_delete_path(&task).unwrap();
        assert_eq!(path, "/video/movies/千と千尋の神隠し.2001.1080p.日本語音声");
    }

    #[test]
    fn an_absolute_volume_destination_resolves_share_relative() {
        let task = task("dbid_014");
        assert_eq!(task.destination, "/volume1/downloads");
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/downloads/Absolute.Destination.Sample"
        );
    }

    #[test]
    fn an_empty_file_list_falls_back_to_the_title_on_a_non_bt_task() {
        // dbid_012 has an explicit `"file": []` rather than no `file` key at
        // all — an empty list and an absent one are the same statement, and for
        // an HTTP task both are ordinary. (For a *torrent* they are not; see
        // `a_bt_task_with_no_file_list_is_refused_rather_than_named_from_its_title`.)
        let task = task("dbid_012");
        assert_eq!(task.task_type, TaskType::Http);
        assert!(task.files.is_empty());
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/downloads/empty-placeholder.bin"
        );
    }

    #[test]
    fn a_non_bt_task_with_no_file_block_falls_back_to_the_title() {
        let task = task("dbid_007");
        assert!(task.files.is_empty());
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/downloads/syno-clean-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn a_destination_wrapped_in_slashes_still_resolves() {
        let task = Task {
            destination: "/video/tv/".to_string(),
            ..bare()
        };
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/video/tv/Some.Release"
        );
    }

    #[test]
    fn a_deeply_nested_destination_resolves() {
        let task = Task {
            destination: "video/tv/archive/2026".to_string(),
            ..bare()
        };
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/video/tv/archive/2026/Some.Release"
        );
    }

    // -----------------------------------------------------------------------
    // resolve_delete_path — THE critical refusal
    // -----------------------------------------------------------------------

    #[test]
    fn a_file_list_with_no_common_root_resolves_to_the_container_dsm_made() {
        // dbid_013's files are "Disc1/…", "Disc2/…" and "readme.nfo" — several
        // things at the torrent's own root. Download Station puts that in a
        // directory named after the torrent, so the payload is the title path
        // and the file list is what proves a container exists.
        //
        // This rule refused until it met a real NAS, where 15 of 41 tasks had
        // this shape and every one of them resolved to an existing directory of
        // exactly this name.
        let task = task("dbid_013");
        assert_eq!(task.title, "Mixed.Root.Release");
        assert_eq!(task.destination, "video/tv");
        assert_eq!(task.files.len(), 3);

        let resolved = resolve_delete_target(&task).expect("rule 2 resolves");
        assert_eq!(resolved.path, "/video/tv/Mixed.Root.Release");
        // The file list says a container exists, so a *file* found there cannot
        // be this payload — unlike a task with no list at all, which accepts
        // either kind.
        assert_eq!(resolved.expected_kind, ExpectedKind::Dir);
    }

    #[test]
    fn rule_two_expects_a_directory_where_rule_three_accepts_anything() {
        // The distinction is the whole reason rule 2 is safe to resolve: both
        // are named from the title, but only one has a file list behind it
        // saying a container must be there.
        let with_list = Task {
            title: "Season.Pack".to_string(),
            destination: "downloads".to_string(),
            files: vec![file("e01.mkv"), file("e02.mkv")],
            ..bare()
        };
        let without_list = Task {
            title: "Season.Pack".to_string(),
            destination: "downloads".to_string(),
            task_type: TaskType::Http,
            files: Vec::new(),
            ..bare()
        };

        let a = resolve_delete_target(&with_list).expect("rule 2");
        let b = resolve_delete_target(&without_list).expect("rule 3");
        assert_eq!(a.path, b.path, "same name, different expectation");
        assert_eq!(a.expected_kind, ExpectedKind::Dir);
        assert_eq!(b.expected_kind, ExpectedKind::AnyFromTitle);
    }

    #[test]
    fn a_common_root_still_wins_over_the_title() {
        // Rule 1 is untouched and still takes precedence: when the list agrees
        // on a root, that root is authoritative even though the title differs.
        let task = Task {
            title: "Display.Title.Nobody.Wrote".to_string(),
            destination: "downloads".to_string(),
            files: vec![
                file("Actual.On.Disk.Name/a.bin"),
                file("Actual.On.Disk.Name/b.bin"),
            ],
            ..bare()
        };
        // The file list's own root is NOT the answer: Download Station writes
        // into a container named after the task, and the list is relative to
        // it. Aiming at the list's root is what sent two real Blu-ray torrents
        // (file list `BDMV/…`) at `/video/BDMV`.
        let resolved = resolve_delete_target(&task).expect("resolvable");
        assert_eq!(resolved.path, "/downloads/Display.Title.Nobody.Wrote");
        assert_eq!(resolved.expected_kind, ExpectedKind::Dir);
    }

    // -----------------------------------------------------------------------
    // resolve_delete_path — refusals from the destination and the name
    // -----------------------------------------------------------------------

    #[test]
    fn a_task_with_no_destination_is_refused() {
        // dbid_010 has no `additional` block at all, so DSM told us nothing
        // about where its data lives. `/Hosted.Archive.Part1of3` would name a
        // share, not a task directory.
        let task = task("dbid_010");
        assert_eq!(task.destination, "");
        let reason = refusal(resolve_delete_path(&task));
        assert!(reason.contains("destination"), "{reason}");
    }

    #[test]
    fn a_partial_additional_block_without_a_destination_is_refused() {
        // dbid_011 has a perfectly good single-root file list but no detail
        // block — a resolvable name is not enough on its own.
        let task = task("dbid_011");
        assert_eq!(
            common_root(&task.files).as_deref(),
            Some("Mystery.Task.With.Unrecognized.Status")
        );
        refusal(resolve_delete_path(&task));
    }

    #[test]
    fn a_volume_root_destination_is_refused() {
        let task = Task {
            destination: "/volume1".to_string(),
            ..bare()
        };
        refusal(resolve_delete_path(&task));
    }

    #[test]
    fn a_blank_destination_is_refused() {
        for destination in ["   ", "/   /", "\t"] {
            let task = Task {
                destination: destination.to_string(),
                ..bare()
            };
            refusal(resolve_delete_path(&task));
        }
    }

    #[test]
    fn an_empty_title_with_no_file_list_is_refused() {
        let task = Task {
            title: String::new(),
            ..bare()
        };
        let reason = refusal(resolve_delete_path(&task));
        assert!(reason.contains("no usable on-disk name"), "{reason}");
    }

    #[test]
    fn a_whitespace_only_title_is_refused() {
        let task = Task {
            title: "   ".to_string(),
            ..bare()
        };
        refusal(resolve_delete_path(&task));
    }

    #[test]
    fn a_title_containing_a_path_separator_is_refused() {
        // "/downloads/Some/Release" is a level deeper than the task's own
        // directory and could easily be someone else's.
        let task = Task {
            title: "Some/Release".to_string(),
            ..bare()
        };
        let reason = refusal(resolve_delete_path(&task));
        assert!(reason.contains("path separator"), "{reason}");
    }

    #[test]
    fn a_title_that_is_a_traversal_component_is_refused() {
        for title in ["..", ".", "../..", "../secret"] {
            let task = Task {
                title: title.to_string(),
                ..bare()
            };
            refusal(resolve_delete_path(&task));
        }
    }

    #[test]
    fn a_traversal_in_the_destination_is_refused() {
        for destination in ["downloads/..", "../downloads", "downloads/../../etc", "."] {
            let task = Task {
                destination: destination.to_string(),
                ..bare()
            };
            refusal(resolve_delete_path(&task));
        }
    }

    #[test]
    fn a_control_character_anywhere_is_refused() {
        let task = Task {
            title: "Some\0Release".to_string(),
            ..bare()
        };
        refusal(resolve_delete_path(&task));

        // The dangerous one: a NUL in the destination truncates the path in a
        // C consumer, turning "/downloads\0/x" into the share root.
        let task = Task {
            destination: "downloads\0".to_string(),
            ..bare()
        };
        refusal(resolve_delete_path(&task));

        let task = Task {
            destination: "down\nloads".to_string(),
            ..bare()
        };
        refusal(resolve_delete_path(&task));
    }

    #[test]
    fn a_hostile_title_is_guarded() {
        // The name is always the title now, so the title is what the guards
        // have to hold. A traversal or a separator in it would otherwise aim
        // the delete outside the destination entirely.
        for hostile in ["..", ".", "../escape", "a/b", "   ", ""] {
            let task = Task {
                title: hostile.to_string(),
                files: vec![file("a.mkv"), file("b.mkv")],
                ..bare()
            };
            refusal(resolve_delete_path(&task));
        }
    }

    #[test]
    fn every_fixture_task_either_resolves_safely_or_is_refused() {
        // No task may produce a path that fails the guards — resolution and
        // validation must not be able to disagree.
        for task in fixture_tasks() {
            if let Ok(path) = resolve_delete_path(&task) {
                validate_path(&path)
                    .unwrap_or_else(|err| panic!("{} resolved to an invalid path: {err}", task.id));
                assert!(path.starts_with('/'), "{}: {path}", task.id);
            }
        }
    }

    // -----------------------------------------------------------------------
    // validate_path
    // -----------------------------------------------------------------------

    #[test]
    fn a_normal_task_path_passes() {
        for path in [
            "/downloads/Some.Release",
            "/video/movies/千と千尋の神隠し.2001.1080p",
            "/downloads/incoming/Sintel.2010.2160p.HDR",
            "/video/tv/archive/2026/Some.Show.S01",
            "/downloads/file with spaces.iso",
            "/downloads/Release [2026] (1080p)",
        ] {
            validate_path(path).unwrap_or_else(|err| panic!("{path:?} should pass: {err}"));
        }
    }

    #[test]
    fn an_empty_path_is_rejected() {
        assert!(rejected("").contains("empty"));
    }

    #[test]
    fn a_relative_path_is_rejected() {
        assert!(rejected("downloads/Some.Release").contains("absolute"));
        assert!(rejected("Some.Release").contains("absolute"));
    }

    #[test]
    fn the_filesystem_root_is_rejected() {
        assert!(rejected("/").contains("root"));
    }

    #[test]
    fn a_share_root_is_rejected() {
        // One component: "/downloads" is an entire share.
        assert!(rejected("/downloads").contains("share root"));
        // The plan's "" destination case: joining with no destination yields a
        // single-component path, which must never be deleted.
        assert!(rejected("/Some.Release").contains("share root"));
    }

    #[test]
    fn a_trailing_slash_is_rejected() {
        // "/downloads/Some.Release/" has two non-empty components but would
        // also read as a share root once the trailing separator collapses.
        assert!(rejected("/downloads/Some.Release/").contains("component"));
        assert!(rejected("/downloads/").contains("component"));
    }

    #[test]
    fn a_doubled_slash_is_rejected() {
        assert!(rejected("//Some.Release").contains("component"));
        assert!(rejected("/downloads//Some.Release").contains("component"));
    }

    #[test]
    fn a_blank_component_is_rejected() {
        // Whitespace-only components are the trimming hazard: "/   /Release"
        // collapses to "/Release", a share root, if anything downstream trims.
        assert!(rejected("/   /Some.Release").contains("blank"));
        assert!(rejected("/downloads/   ").contains("blank"));
        assert!(rejected("/downloads/ /Some.Release").contains("blank"));
    }

    #[test]
    fn a_parent_traversal_component_is_rejected() {
        for path in [
            "/downloads/..",
            "/downloads/../etc",
            "/../downloads/Some.Release",
            "/downloads/Some.Release/../../..",
            "/..",
        ] {
            assert!(rejected(path).contains(".."), "{path}");
        }
    }

    #[test]
    fn a_current_directory_component_is_rejected() {
        for path in ["/downloads/.", "/./downloads/Some.Release", "/."] {
            rejected(path);
        }
    }

    #[test]
    fn a_name_that_merely_starts_with_dots_is_still_allowed() {
        // ".." is a traversal; "..hidden" is just a file.
        validate_path("/downloads/..hidden").expect("..hidden is a legal name");
        validate_path("/downloads/.hidden").expect(".hidden is a legal name");
    }

    #[test]
    fn a_control_character_is_rejected() {
        for path in [
            "/downloads/Some\0Release",
            "/downloads\0/Some.Release",
            "/downloads/Some.Release\n",
            "/downloads/Some\rRelease",
            "/downloads/Some\u{7f}Release",
        ] {
            assert!(rejected(path).contains("control"), "{path:?}");
        }
    }

    // -----------------------------------------------------------------------
    // DeletePlan
    // -----------------------------------------------------------------------

    #[test]
    fn a_plan_records_a_path_for_every_resolvable_task() {
        let tasks = [task("dbid_001"), task("dbid_003")];
        let plan = DeletePlan::snapshot(tasks.iter());
        assert_eq!(plan.len(), 2);
        assert!(!plan.is_empty());
        assert_eq!(plan.refused().count(), 0);
        assert_eq!(
            plan.items[0].path(),
            Some("/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64")
        );
        assert_eq!(plan.items[0].id, "dbid_001");
        assert_eq!(plan.items[0].status, TaskStatus::Downloading);
    }

    #[test]
    fn an_unresolvable_task_is_a_per_item_skip_not_an_aborted_batch() {
        let tasks = [task("dbid_001"), task("dbid_010"), task("dbid_003")];
        let plan = DeletePlan::snapshot(tasks.iter());

        assert_eq!(plan.len(), 3);
        assert_eq!(plan.deletable().count(), 2);
        assert_eq!(plan.refused().count(), 1);

        let skipped = plan.refused().next().unwrap();
        assert_eq!(skipped.id, "dbid_010");
        assert!(skipped.path().is_none(), "a refused item has no path");
        assert!(skipped.refusal().is_some_and(|r| !r.is_empty()));
        // Order is snapshot order, so the dialog lists rows as the user sees
        // them.
        assert_eq!(plan.items[1].id, "dbid_010");
    }

    #[test]
    fn the_total_excludes_refused_items() {
        let resolvable = task("dbid_001");
        let refused = task("dbid_010");
        assert!(refused.size > 0, "the refused task must have a size");

        let plan = DeletePlan::snapshot([&resolvable, &refused]);
        assert_eq!(plan.total_size(), resolvable.size);
    }

    #[test]
    fn an_empty_plan_has_nothing_to_confirm() {
        let plan = DeletePlan::snapshot(std::iter::empty::<&Task>());
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
        assert_eq!(plan.total_size(), 0);
        assert_eq!(plan, DeletePlan::default());
    }

    #[test]
    fn a_plan_is_an_owned_snapshot_that_a_refresh_cannot_change() {
        // The reason `DeletePlan` copies rather than borrows: the poller may
        // hand `App` an entirely new task list while the dialog is open, and
        // what the user confirms must be what they read.
        let mut tasks = vec![task("dbid_001")];
        let plan = DeletePlan::snapshot(tasks.iter());
        let before = plan.clone();

        tasks[0].destination = "totally/elsewhere".to_string();
        tasks[0].title = "Renamed".to_string();
        tasks[0].size = 0;
        tasks[0].files = vec![file("Different.Root/x.bin")];
        tasks.clear();

        assert_eq!(plan, before);
        assert_eq!(
            plan.items[0].path(),
            Some("/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64")
        );
        assert_eq!(plan.items[0].title, "Ubuntu.24.04.3.LTS.Desktop.amd64");
        assert!(plan.total_size() > 0);
    }

    #[test]
    fn a_plan_over_the_whole_fixture_resolves_what_it_can_and_refuses_the_rest() {
        let tasks = fixture_tasks();
        let plan = DeletePlan::snapshot(tasks.iter());
        assert_eq!(plan.len(), tasks.len());

        let refused: Vec<&str> = plan.refused().map(|item| item.id.as_str()).collect();
        // dbid_008 is a torrent with an empty file list; dbid_010 and dbid_011
        // have no destination; dbid_013 has no common root. Everything else is
        // unambiguous.
        assert_eq!(refused, ["dbid_008", "dbid_010", "dbid_011"]);

        for item in plan.deletable() {
            let path = item.path().expect("deletable items have a path");
            validate_path(path).unwrap_or_else(|err| panic!("{}: {err}", item.id));
        }
    }

    // -----------------------------------------------------------------------
    // plan_delete_ops — the ordering table
    // -----------------------------------------------------------------------

    /// A resolvable item in a given status.
    fn item_with(status: TaskStatus) -> DeleteItem {
        DeleteItem::for_task(&Task { status, ..bare() })
    }

    /// The statuses the plan's table sends through a pause first.
    const ACTIVE: [TaskStatus; 6] = [
        TaskStatus::Downloading,
        TaskStatus::Seeding,
        TaskStatus::Waiting,
        TaskStatus::Finishing,
        TaskStatus::HashChecking,
        TaskStatus::Extracting,
    ];

    /// The statuses the plan's table deletes straight away.
    const INACTIVE: [TaskStatus; 3] = [TaskStatus::Paused, TaskStatus::Finished, TaskStatus::Error];

    #[test]
    fn every_task_is_paused_before_anything_is_deleted_whatever_the_snapshot_said() {
        // Including the statuses the plan's table calls inactive. The snapshot
        // status is as old as the confirmation dialog plus this item's place in
        // the batch queue, so it cannot be trusted to say a task is idle *now*;
        // the executor resolves the phase against a live read and skips the
        // pause call when the task really is idle.
        for status in ACTIVE.iter().chain(INACTIVE.iter()) {
            let item = item_with(status.clone());
            assert_eq!(
                plan_delete_ops(&item, DeleteOptions::default()),
                vec![
                    Op::Pause,
                    Op::DeleteFiles("/downloads/Some.Release".to_string()),
                    Op::DeleteTask,
                ],
                "{status:?}"
            );
        }
    }

    #[test]
    fn the_two_statuses_the_plans_table_does_not_name_are_treated_as_active() {
        // `filehosting_waiting` is not in either column of the plan's table,
        // and neither is an unrecognized status. Pausing an idle task costs a
        // round trip; not pausing a live one risks Download Station writing
        // into the directory mid-delete.
        for status in [
            TaskStatus::FilehostingWaiting,
            TaskStatus::Unknown("captcha_needed".to_string()),
        ] {
            assert!(requires_pause(&status), "{status:?}");
            assert_eq!(
                plan_delete_ops(&item_with(status.clone()), DeleteOptions::default())[0],
                Op::Pause,
                "{status:?}"
            );
        }
    }

    #[test]
    fn requires_pause_partitions_every_known_status() {
        for status in ACTIVE {
            assert!(requires_pause(&status), "{status:?}");
        }
        for status in INACTIVE {
            assert!(!requires_pause(&status), "{status:?}");
        }
    }

    // -----------------------------------------------------------------------
    // payload_should_exist — status is not the only evidence
    // -----------------------------------------------------------------------

    /// A state with the counters of a task that has downloaded nothing.
    fn empty_state(status: TaskStatus) -> PayloadState {
        PayloadState {
            status,
            downloaded: 0,
            size: 1024,
        }
    }

    #[test]
    fn a_completed_status_means_the_payload_must_be_there() {
        for status in [
            TaskStatus::Finished,
            TaskStatus::Seeding,
            TaskStatus::Extracting,
        ] {
            assert!(
                payload_should_exist(&empty_state(status.clone())),
                "{status}"
            );
        }
    }

    #[test]
    fn an_incomplete_task_may_legitimately_have_nothing_on_disk() {
        for status in [
            TaskStatus::Downloading,
            TaskStatus::Waiting,
            TaskStatus::Paused,
            TaskStatus::Error,
            TaskStatus::Finishing,
            TaskStatus::HashChecking,
            TaskStatus::FilehostingWaiting,
        ] {
            assert!(
                !payload_should_exist(&empty_state(status.clone())),
                "{status}"
            );
        }
    }

    #[test]
    fn a_fully_downloaded_task_must_have_a_payload_whatever_its_status_says() {
        // The gap status alone leaves: a task paused at 100%, and one that
        // errored *after* its download finished, both have the whole payload on
        // disk. Neither status is in the completed set, so reading status alone
        // called an absent path benign and removed the task.
        for status in [
            TaskStatus::Paused,
            TaskStatus::Error,
            TaskStatus::Finishing,
            TaskStatus::HashChecking,
            TaskStatus::Unknown("captcha_needed".to_string()),
        ] {
            let state = PayloadState {
                status: status.clone(),
                downloaded: 4096,
                size: 4096,
            };
            assert!(payload_should_exist(&state), "{status}");
        }
    }

    #[test]
    fn a_bt_task_that_re_downloaded_pieces_still_counts_as_complete() {
        // `downloaded > size` is ordinary for BitTorrent; `==` alone would miss
        // it and read an absent payload as ordinary partial data.
        let state = PayloadState {
            status: TaskStatus::Paused,
            downloaded: 5000,
            size: 4096,
        };
        assert!(state.fully_downloaded());
        assert!(payload_should_exist(&state));
    }

    #[test]
    fn a_zero_sized_task_is_not_evidence_of_anything() {
        // `0 >= 0` is true for free, and a task whose size DSM never reported
        // (or reported unparseably, which the model reads as 0) must not be
        // turned into one whose payload "must" exist — that would fail items
        // whose absence is entirely ordinary, with no way back except
        // `--no-delete-files`.
        let state = PayloadState {
            status: TaskStatus::Downloading,
            downloaded: 0,
            size: 0,
        };
        assert!(!state.fully_downloaded());
        assert!(!payload_should_exist(&state));
    }

    #[test]
    fn most_of_a_payload_is_not_the_whole_payload() {
        // The threshold is deliberately the conservative one. A task at 99% has
        // partial data Download Station cleans up after itself, which is
        // exactly the case an absent path is allowed to mean.
        let state = PayloadState {
            status: TaskStatus::Paused,
            downloaded: 4095,
            size: 4096,
        };
        assert!(!payload_should_exist(&state));
    }

    #[test]
    fn a_snapshot_item_reports_the_state_it_was_taken_with() {
        let item = DeleteItem::for_task(&Task {
            status: TaskStatus::Paused,
            size: 2048,
            downloaded: 2048,
            ..bare()
        });
        assert_eq!(
            item.payload_state(),
            PayloadState {
                status: TaskStatus::Paused,
                downloaded: 2048,
                size: 2048,
            }
        );
        // And the dialog's question is answered from the same evidence: a task
        // paused at 100% keeps its files when only the DSM task is removed.
        assert!(payload_survives_task_delete(&item.payload_state()));
    }

    #[test]
    fn the_files_always_go_before_the_task() {
        // The recoverable ordering: a task that outlives its files is a bug the
        // user can retry, a volume full of unreferenced directories is not.
        for status in ACTIVE.iter().chain(INACTIVE.iter()) {
            let ops = plan_delete_ops(&item_with(status.clone()), DeleteOptions::default());
            let files = ops
                .iter()
                .position(|op| matches!(op, Op::DeleteFiles(_)))
                .expect("a file phase");
            let task = ops
                .iter()
                .position(|op| *op == Op::DeleteTask)
                .expect("a task phase");
            assert!(files < task, "{status:?}: {ops:?}");
            assert_eq!(task, ops.len() - 1, "the task delete must be last");
        }
    }

    #[test]
    fn a_refused_item_is_touched_by_nothing_at_all_while_files_are_being_deleted() {
        // Not even the DSM task: the row was shown to the user as SKIPPED, and
        // deleting the task would orphan precisely the data whose location is
        // in doubt.
        let refused = DeleteItem::for_task(&task("dbid_010"));
        assert!(refused.is_refused());
        assert_eq!(plan_delete_ops(&refused, DeleteOptions::default()), vec![]);
        assert_eq!(plan_delete_ops(&refused, DeleteOptions::dry_run()), vec![]);
        assert!(!will_act(&refused, DeleteOptions::default()));
    }

    #[test]
    fn no_delete_files_can_remove_a_refused_item_because_no_path_is_used() {
        // The tasks that need `--no-delete-files` most are exactly the ones the
        // resolver refuses — a torrent with several top-level roots, or one with
        // no destination at all. Refusing those here too left them unremovable
        // by this tool by any route, while the README promised the flag
        // "removes the Download Station task only".
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        for id in ["dbid_010", "dbid_011"] {
            let refused = DeleteItem::for_task(&task(id));
            assert!(refused.is_refused(), "{id} is meant to be refused");
            assert_eq!(
                plan_delete_ops(&refused, options),
                vec![Op::DeleteTask],
                "{id}"
            );
            assert!(will_act(&refused, options), "{id}");
        }
    }

    #[test]
    fn no_delete_files_drops_the_file_phase_and_the_pause_with_it() {
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        for status in ACTIVE.iter().chain(INACTIVE.iter()) {
            assert_eq!(
                plan_delete_ops(&item_with(status.clone()), options),
                vec![Op::DeleteTask],
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_dry_run_still_plans_every_op_so_it_can_report_them() {
        assert_eq!(
            plan_delete_ops(
                &item_with(TaskStatus::Downloading),
                DeleteOptions::dry_run()
            ),
            plan_delete_ops(
                &item_with(TaskStatus::Downloading),
                DeleteOptions::default()
            )
        );
    }

    #[test]
    fn the_file_phase_carries_the_path_the_snapshot_resolved() {
        let item = DeleteItem::for_task(&task("dbid_001"));
        let ops = plan_delete_ops(&item, DeleteOptions::default());
        assert!(ops.contains(&Op::DeleteFiles(
            "/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64".to_string()
        )));
    }

    // -----------------------------------------------------------------------
    // ops_cancelled_by — "a failed phase cancels every later phase"
    // -----------------------------------------------------------------------

    #[test]
    fn a_pause_failure_leaves_both_deletes_unissued() {
        let ops = plan_delete_ops(&item_with(TaskStatus::Seeding), DeleteOptions::default());
        assert_eq!(ops[0], Op::Pause);
        assert_eq!(
            ops_cancelled_by(&ops, 0),
            &[
                Op::DeleteFiles("/downloads/Some.Release".to_string()),
                Op::DeleteTask,
            ]
        );
    }

    #[test]
    fn a_file_delete_failure_leaves_the_task_delete_unissued() {
        // The task must survive still pointing at its data — otherwise a failed
        // file delete silently orphans the directory.
        let ops = plan_delete_ops(&item_with(TaskStatus::Finished), DeleteOptions::default());
        let files = ops
            .iter()
            .position(|op| matches!(op, Op::DeleteFiles(_)))
            .expect("a file phase");
        assert_eq!(ops_cancelled_by(&ops, files), &[Op::DeleteTask]);
    }

    #[test]
    fn the_last_phase_failing_cancels_nothing() {
        let ops = plan_delete_ops(&item_with(TaskStatus::Finished), DeleteOptions::default());
        assert!(ops_cancelled_by(&ops, ops.len() - 1).is_empty());
        // Out of range is empty too, not a panic.
        assert!(ops_cancelled_by(&ops, 99).is_empty());
        assert!(ops_cancelled_by(&[], 0).is_empty());
    }

    // -----------------------------------------------------------------------
    // describe_ops
    // -----------------------------------------------------------------------

    #[test]
    fn an_op_list_reads_as_a_sentence_for_the_dry_run_report() {
        let ops = plan_delete_ops(
            &item_with(TaskStatus::Downloading),
            DeleteOptions::dry_run(),
        );
        assert_eq!(
            describe_ops(&ops),
            "pause the task, then delete /downloads/Some.Release, then delete the DSM task"
        );
        assert_eq!(describe_ops(&[]), "nothing");
    }

    // -----------------------------------------------------------------------
    // DeleteOptions::from_config — the only translation of the CLI flags
    // -----------------------------------------------------------------------

    /// A resolved configuration with the two delete-affecting settings set.
    fn config_with(delete_files: bool, dry_run: bool) -> ResolvedConfig {
        ResolvedConfig {
            delete_files,
            dry_run,
            ..crate::testutil::offline_config()
        }
    }

    #[test]
    fn from_config_carries_both_flags_through_unchanged() {
        // An inverted `dry_run` here would make `--dry-run` perform the real
        // recursive delete, and nothing else in the program would notice.
        for delete_files in [true, false] {
            for dry_run in [true, false] {
                let options = DeleteOptions::from_config(&config_with(delete_files, dry_run));
                assert_eq!(
                    options,
                    DeleteOptions {
                        delete_files,
                        dry_run
                    },
                    "delete_files={delete_files} dry_run={dry_run}"
                );
            }
        }
    }

    #[test]
    fn from_config_and_plan_delete_ops_agree_about_what_a_dry_run_is() {
        // The end-to-end property the flag exists for: `--dry-run` plans every
        // op (so it can report them) and `--no-delete-files` plans no file
        // phase at all.
        let dry = DeleteOptions::from_config(&config_with(true, true));
        assert!(dry.dry_run && dry.delete_files);
        let item = item_with(TaskStatus::Downloading);
        assert!(
            plan_delete_ops(&item, dry)
                .iter()
                .any(|op| matches!(op, Op::DeleteFiles(_)))
        );

        let task_only = DeleteOptions::from_config(&config_with(false, false));
        assert_eq!(plan_delete_ops(&item, task_only), vec![Op::DeleteTask]);
    }

    #[test]
    fn the_default_options_are_what_an_unflagged_run_resolves_to() {
        assert_eq!(
            DeleteOptions::from_config(&config_with(DEFAULT_DELETE_FILES, false)),
            DeleteOptions::default()
        );
        assert!(!DeleteOptions::default().dry_run, "a dry run is opt-in");
        assert!(DeleteOptions::dry_run().dry_run);
    }

    // -----------------------------------------------------------------------
    // `named` — whether anything resolved a path at all
    // -----------------------------------------------------------------------

    #[test]
    fn a_resolved_item_is_marked_as_named() {
        let resolved = resolve_delete_target(&task("dbid_001")).expect("resolvable");
        assert_eq!(resolved.path, "/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64");
        assert!(DeleteItem::for_task(&task("dbid_001")).named);
    }

    #[test]
    fn a_refused_item_is_not_named() {
        // The one input that authorizes nothing whatever the counters say:
        // `event::decide_file_phase` refuses an absent path outright when
        // nothing named it.
        assert!(!DeleteItem::for_task(&task("dbid_010")).named);
    }

    // -----------------------------------------------------------------------
    // Rule 4 — a torrent with no file list is refused, never title-guessed
    // -----------------------------------------------------------------------

    #[test]
    fn a_bt_task_with_no_file_list_is_refused_rather_than_named_from_its_title() {
        // dbid_008 is a `bt` task whose `additional` block carries detail and
        // transfer but no `file`. Rule 3 was written for the types that have no
        // file list to give; for a torrent an absent list is anomalous, and the
        // title is exactly the value rule 2 refuses to trust. Guessing here
        // aims a *recursive* delete at "/downloads/incoming/Sintel.2010.2160p.HDR"
        // on nothing but a display string.
        let bt = task("dbid_008");
        assert_eq!(bt.task_type, TaskType::BitTorrent);
        assert!(bt.files.is_empty());

        let reason = refusal(resolve_delete_path(&bt));
        assert!(reason.contains("no files"), "{reason}");
        assert!(
            reason.contains("--no-delete-files"),
            "a refusal must name the way forward: {reason}"
        );

        let item = DeleteItem::for_task(&bt);
        assert!(item.is_refused());
        assert!(!item.named);
        // …and the escape hatch really is one: with no file delete there is no
        // path to be unsure about, so the row can still be removed.
        assert_eq!(
            plan_delete_ops(
                &item,
                DeleteOptions {
                    delete_files: false,
                    dry_run: false
                }
            ),
            vec![Op::DeleteTask]
        );
    }

    #[test]
    fn a_missing_file_list_is_still_a_title_fallback_for_the_types_that_have_none() {
        // The other half of rule 4: HTTP, FTP, NZB and eMule tasks legitimately
        // carry no `file` block, and refusing them would strand every non-BT
        // download this tool is asked to clean up.
        for task_type in [
            TaskType::Http,
            TaskType::Https,
            TaskType::Ftp,
            TaskType::Ftps,
            TaskType::Nzb,
            TaskType::Emule,
            // A type this client has never heard of is *not* assumed to be a
            // torrent: refusing over an unrecognized string would strand tasks
            // on the next DSM build to invent one.
            TaskType::Unknown("magnet_of_the_future".to_string()),
            TaskType::default(),
        ] {
            let task = Task {
                task_type: task_type.clone(),
                files: Vec::new(),
                ..bare()
            };
            assert_eq!(
                resolve_delete_path(&task).ok().as_deref(),
                Some("/downloads/Some.Release"),
                "{task_type}"
            );
        }
    }

    #[test]
    fn a_bt_task_that_does_have_a_file_list_is_unaffected() {
        // Rule 4 is about the *absence* of the list, not about the type.
        let task = Task {
            task_type: TaskType::BitTorrent,
            files: vec![file("Some.Release/a.mkv")],
            ..bare()
        };
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/downloads/Some.Release"
        );
    }

    // -----------------------------------------------------------------------
    // ExpectedKind — what should be at the resolved path
    // -----------------------------------------------------------------------

    #[test]
    fn a_multi_file_torrent_expects_a_directory() {
        // dbid_001's entries all live under one root, so the root is a folder —
        // and a *file* of that name is not this task's payload.
        let resolved = resolve_delete_target(&task("dbid_001")).expect("resolvable");
        assert_eq!(resolved.expected_kind, ExpectedKind::Dir);
        assert_eq!(
            DeleteItem::for_task(&task("dbid_001")).expected_kind,
            ExpectedKind::Dir
        );
    }

    #[test]
    fn a_single_file_torrent_expects_a_file() {
        // dbid_003 is one flat entry: the resolved name *is* the download, and
        // a directory of that name is something else entirely.
        let resolved = resolve_delete_target(&task("dbid_003")).expect("resolvable");
        assert_eq!(resolved.expected_kind, ExpectedKind::File);
    }

    #[test]
    fn a_single_entry_with_a_separator_still_expects_a_directory() {
        let task = Task {
            task_type: TaskType::BitTorrent,
            files: vec![file("Some.Release/only.mkv")],
            ..bare()
        };
        let resolved = resolve_delete_target(&task).expect("resolvable");
        assert_eq!(resolved.expected_kind, ExpectedKind::Dir);
    }

    #[test]
    fn a_title_named_path_expects_nothing_in_particular() {
        // Rule 3 has no file list to describe the shape: an HTTP download is a
        // file, an NZB task's destination is usually a directory, and DSM says
        // neither. Documented as undetermined rather than guessed at.
        let resolved = resolve_delete_target(&task("dbid_007")).expect("resolvable");
        assert_eq!(resolved.expected_kind, ExpectedKind::AnyFromTitle);
        assert!(ExpectedKind::AnyFromTitle.accepts(true));
        assert!(ExpectedKind::AnyFromTitle.accepts(false));
    }

    #[test]
    fn each_expectation_accepts_only_its_own_kind() {
        assert!(ExpectedKind::Dir.accepts(true));
        assert!(!ExpectedKind::Dir.accepts(false));
        assert!(ExpectedKind::File.accepts(false));
        assert!(!ExpectedKind::File.accepts(true));
        // A refused item never reaches a lookup, but it must not carry an
        // expectation that would authorize one either.
        assert_eq!(
            DeleteItem::for_task(&task("dbid_010")).expected_kind,
            ExpectedKind::Indeterminate
        );
    }

    #[test]
    fn an_indeterminate_expectation_accepts_neither_kind() {
        // The half of "not knowable" that is **not** permissive. Both
        // indeterminate variants used to be one `Unknown` that accepted
        // anything, which let a malformed file list authorize the recursive
        // delete the file list was supposed to constrain.
        assert!(!ExpectedKind::Indeterminate.accepts(true));
        assert!(!ExpectedKind::Indeterminate.accepts(false));
    }

    #[test]
    fn a_malformed_file_list_still_expects_the_container() {
        // Several entries carrying one identical filename is a shape DSM should
        // never send. It used to make the *expectation* indeterminate, because
        // the list was what named the path and a self-contradicting list could
        // not be trusted to describe it.
        //
        // The list no longer names anything: the payload is the title, and a
        // multi-entry list means Download Station made a container whatever the
        // entries say. So the malformed part cannot mislead the path, and the
        // expectation is an ordinary directory. `Indeterminate` now belongs to
        // refused items alone.
        let task = Task {
            task_type: TaskType::BitTorrent,
            files: vec![file("Some.Release"), file("Some.Release")],
            ..bare()
        };
        let resolved = resolve_delete_target(&task).expect("resolvable");
        assert_eq!(resolved.path, "/downloads/Some.Release");
        assert_eq!(resolved.expected_kind, ExpectedKind::Dir);
    }
}
