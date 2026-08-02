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
//! The on-disk *name* comes from, in order:
//!
//! 1. **The file list, when its entries share a single top-level component** —
//!    that component. This is authoritative: it is what BitTorrent actually
//!    wrote, and it is frequently *not* the display title (a release renamed by
//!    the indexer, a `.rar` set inside a differently-named folder).
//! 2. **The file list, when its entries share no single top-level component** —
//!    **REFUSE**. This is the critical rule. Never fall back to `title` here:
//!    the title is precisely the value the file list just contradicted, and a
//!    guessed directory name can easily match an unrelated folder that already
//!    exists next to it — which is then recursively deleted.
//! 3. **No file list at all** (HTTP/FTP/NZB tasks, which have no `file` block) —
//!    the `title`, which for those task types *is* the on-disk name.
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
//! before the delete) belongs to the executor in Task 15 — it needs the
//! network. Everything here is pure.
//!
//! ## Snapshot semantics
//!
//! [`DeletePlan`] owns copies of everything it needs: id, title, size, status
//! and the resolved path, all taken at the instant the confirmation dialog
//! opens. It borrows nothing from the task list, so a background refresh
//! landing mid-dialog cannot change what the user is about to confirm. (The
//! poller is suspended in `Mode::Confirm` as well — belt *and* braces, because
//! the failure mode here is deleting something the user never read.)

use crate::error::{Error, Result};
use crate::model::{Task, TaskFile, TaskStatus};

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
///   [`resolve_delete_path`]),
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
/// Only the **absolute** `/volumeN` form is stripped. A destination that
/// merely *starts with* the text `volume1` is left alone: a share may legally
/// be named that, and mangling a relative path is how a delete ends up one
/// directory away from where it was aimed. Anything unrecognized is likewise
/// passed through untouched — a path that does not exist fails the executor's
/// existence check and is skipped, which is the safe direction.
pub fn normalize_destination(destination: &str) -> String {
    strip_volume_prefix(destination)
        .trim_matches('/')
        .to_string()
}

/// Drop a leading `/volumeN` component (`N` being one or more digits).
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

/// True for `volume1`, `volume12`; false for `volume`, `volumeUSB1`, `video`.
fn is_volume_component(component: &str) -> bool {
    component
        .strip_prefix("volume")
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// The absolute File Station path holding a task's data.
///
/// This is the only function permitted to answer "what does deleting this task
/// remove from the volume", and it answers with an error far more readily than
/// with a path. See the module docs for the resolution order; the short version
/// is that a file list which disagrees with itself is refused outright rather
/// than resolved from the title.
///
/// The returned path has already been through [`validate_path`]. It is
/// re-validated immediately before the File Station call anyway — the check is
/// free and the value crosses a task boundary in between.
pub fn resolve_delete_path(task: &Task) -> Result<String> {
    let name = resolve_name(task)?;

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
    Ok(path)
}

/// The on-disk name of a task's payload — rules 1 to 3 of the resolution order.
fn resolve_name(task: &Task) -> Result<String> {
    let name = if task.files.is_empty() {
        // Rule 3: no file list to be authoritative, so the title is all there
        // is. Non-BT tasks are named after the file they fetch.
        task.title.clone()
    } else {
        // Rules 1 and 2.
        common_root(&task.files).ok_or_else(|| {
            Error::unsafe_path(
                &task.title,
                format!(
                    "the task's {} files share no single top-level directory (found {}), \
                     so the on-disk name cannot be determined; refusing to guess it from \
                     the title",
                    task.files.len(),
                    describe_roots(&task.files)
                ),
            )
        })?
    };

    validate_name(&name)?;
    Ok(name)
}

/// The distinct first components of a file list, for a refusal message that
/// tells the user *why* their torrent was skipped.
fn describe_roots(files: &[TaskFile]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for file in files {
        let component = first_component(&file.filename).unwrap_or("");
        if !seen.contains(&component) {
            seen.push(component);
        }
    }
    let shown: Vec<String> = seen
        .iter()
        .take(4)
        .map(|root| format!("{root:?}"))
        .collect();
    if seen.len() > shown.len() {
        format!("{}, …", shown.join(", "))
    } else {
        shown.join(", ")
    }
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
    /// Status **at snapshot time**, which is what picks the delete ordering
    /// (pause first for an active task).
    pub status: TaskStatus,
    pub target: Target,
}

impl DeleteItem {
    /// Resolve one task into a snapshot item. A refusal is recorded on the
    /// item rather than returned, so one bad torrent never aborts the batch.
    fn for_task(task: &Task) -> Self {
        let target = match resolve_delete_path(task) {
            Ok(path) => Target::Path(path),
            Err(Error::UnsafePath { reason, .. }) => Target::Refused(reason),
            // `resolve_delete_path` only produces `UnsafePath` today; anything
            // else is still a refusal, never a fallthrough to deletion.
            Err(other) => Target::Refused(other.to_string()),
        };
        DeleteItem {
            id: task.id.clone(),
            title: task.title.clone(),
            size: task.size,
            status: task.status.clone(),
            target,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;
    use crate::model::TaskList;

    const FIXTURE: &str = include_str!("../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        parse_envelope::<TaskList>(FIXTURE, "SYNO.DownloadStation.Task")
            .expect("the fixture must parse")
            .tasks
    }

    /// One fixture task by id.
    fn task(id: &str) -> Task {
        fixture_tasks()
            .into_iter()
            .find(|task| task.id == id)
            .unwrap_or_else(|| panic!("fixture has no task {id}"))
    }

    /// A minimal synthetic task; the fields each test cares about are
    /// overwritten with struct-update syntax.
    fn bare() -> Task {
        Task {
            id: "synthetic".to_string(),
            title: "Some.Release".to_string(),
            status: TaskStatus::Finished,
            size: 1024,
            downloaded: 1024,
            uploaded: 0,
            download_speed: 0,
            upload_speed: 0,
            destination: "downloads".to_string(),
            files: Vec::new(),
            seeders: 0,
            leechers: 0,
            create_time: None,
        }
    }

    fn file(filename: &str) -> TaskFile {
        TaskFile {
            filename: filename.to_string(),
            size: 1,
            priority: "normal".to_string(),
            selected: true,
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
        skipped.selected = false;
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
    fn only_the_absolute_volume_form_is_stripped() {
        // A share may legally be named "volume1"; mangling a relative path is
        // how a delete lands one directory away from where it was aimed.
        assert_eq!(
            normalize_destination("volume1/downloads"),
            "volume1/downloads"
        );
        // Not /volumeN: neither is touched.
        assert_eq!(
            normalize_destination("/volumeUSB1/usbshare1"),
            "volumeUSB1/usbshare1"
        );
        assert_eq!(
            normalize_destination("/volume/downloads"),
            "volume/downloads"
        );
        assert_eq!(normalize_destination("/video/movies"), "video/movies");
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
        // dbid_002 also proves the file list wins over a differing title: the
        // title carries an emoji the on-disk directory does not.
        let task = task("dbid_002");
        assert_eq!(task.title, "Big.Buck.Bunny.2008.1080p.🐰.BluRay.x264");
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/video/movies/Big.Buck.Bunny.2008.1080p.BluRay.x264"
        );
    }

    #[test]
    fn the_file_list_beats_a_title_that_disagrees_with_it() {
        // The whole point of rule 1: the display title has a suffix the actual
        // directory does not, and deleting by title would miss (or hit the
        // wrong thing).
        let task = task("dbid_006");
        assert_eq!(task.title, "千と千尋の神隠し.2001.1080p.日本語音声");
        let path = resolve_delete_path(&task).unwrap();
        assert_eq!(path, "/video/movies/千と千尋の神隠し.2001.1080p");
        assert!(
            !path.contains("日本語音声"),
            "resolved from the title: {path}"
        );
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
    fn an_empty_file_list_falls_back_to_the_title() {
        // dbid_008 has `"file": []` with a nested destination.
        let task = task("dbid_008");
        assert!(task.files.is_empty());
        assert_eq!(
            resolve_delete_path(&task).unwrap(),
            "/downloads/incoming/Sintel.2010.2160p.HDR"
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
    fn a_file_list_with_no_common_root_is_refused_and_never_guessed_from_the_title() {
        // The single most important test in this project. dbid_013's files are
        // "Disc1/…", "Disc2/…" and "readme.nfo": there is no directory that
        // holds exactly this task's data, and /video/tv/Mixed.Root.Release may
        // well be an unrelated folder that already exists.
        let task = task("dbid_013");
        assert_eq!(task.title, "Mixed.Root.Release");
        assert_eq!(task.destination, "video/tv");
        assert_eq!(task.files.len(), 3);

        let reason = refusal(resolve_delete_path(&task));
        assert!(
            reason.contains("no single top-level"),
            "unhelpful reason: {reason}"
        );
        // It must not have quietly resolved via the title by any other route.
        assert!(
            !reason.contains("/video/tv/Mixed.Root.Release"),
            "the title path leaked into the refusal: {reason}"
        );
    }

    #[test]
    fn the_refusal_names_the_conflicting_roots() {
        let reason = refusal(resolve_delete_path(&task("dbid_013")));
        for root in ["Disc1", "Disc2", "readme.nfo"] {
            assert!(reason.contains(root), "{root} missing from: {reason}");
        }
    }

    #[test]
    fn a_no_common_root_task_is_refused_even_when_the_title_would_be_valid() {
        // Same shape as dbid_013 but with a title that passes every guard, to
        // prove the refusal is about the file list and not about the title.
        let task = Task {
            title: "Perfectly.Fine.Name".to_string(),
            destination: "downloads".to_string(),
            files: vec![file("A/one.bin"), file("B/two.bin")],
            ..bare()
        };
        refusal(resolve_delete_path(&task));
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
    fn a_root_from_the_file_list_is_guarded_too() {
        // Not just the title fallback: a hostile or corrupt file list gets the
        // same treatment.
        let task = Task {
            files: vec![file("../a.mkv"), file("../b.mkv")],
            ..bare()
        };
        refusal(resolve_delete_path(&task));

        let task = Task {
            files: vec![file("./a.mkv")],
            ..bare()
        };
        refusal(resolve_delete_path(&task));
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
        let tasks = [task("dbid_001"), task("dbid_013"), task("dbid_003")];
        let plan = DeletePlan::snapshot(tasks.iter());

        assert_eq!(plan.len(), 3);
        assert_eq!(plan.deletable().count(), 2);
        assert_eq!(plan.refused().count(), 1);

        let skipped = plan.refused().next().unwrap();
        assert_eq!(skipped.id, "dbid_013");
        assert!(skipped.path().is_none(), "a refused item has no path");
        assert!(skipped.refusal().is_some_and(|r| !r.is_empty()));
        // Order is snapshot order, so the dialog lists rows as the user sees
        // them.
        assert_eq!(plan.items[1].id, "dbid_013");
    }

    #[test]
    fn the_total_excludes_refused_items() {
        let resolvable = task("dbid_001");
        let refused = task("dbid_013");
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
        // dbid_010 and dbid_011 have no destination; dbid_013 has no common
        // root. Everything else is unambiguous.
        assert_eq!(refused, ["dbid_010", "dbid_011", "dbid_013"]);

        for item in plan.deletable() {
            let path = item.path().expect("deletable items have a path");
            validate_path(path).unwrap_or_else(|err| panic!("{}: {err}", item.id));
        }
    }
}
