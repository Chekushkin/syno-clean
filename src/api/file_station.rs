//! `SYNO.FileStation.List` and `SYNO.FileStation.Delete` — the half of a delete
//! that Download Station will not do for you.
//!
//! The DS API removes a *task*; the payload it wrote stays on the volume. This
//! module is what actually reclaims the space, and it is therefore the only
//! place in the program that issues a recursive delete. Two rules from the plan
//! live here:
//!
//! * **List-valued parameters are JSON arrays.** Download Station v1 encodes the
//!   same idea as a comma-separated string (see
//!   [`crate::api::download_station::build_ds_id_params`]). Getting the two
//!   confused yields a silently wrong request rather than an error, so both
//!   encodings are pure `build_*_params` functions with tests pinning the
//!   difference.
//! * **Deletion is `start` + `status` polling, not the blocking `delete`
//!   method.** Recursively removing a large torrent directory can comfortably
//!   outlive [`crate::api::client::REQUEST_TIMEOUT`]; the polling form bounds
//!   the *overall* wait ([`DELETE_TIMEOUT`]) instead of each round trip.
//!
//! The existence check ([`path_info`]) is the plan's **semantic guard**: before
//! anything is deleted, the resolved path is looked up, and what came back
//! decides whether the recursive delete is issued at all. This module only
//! *classifies* the answer ([`PathInfo`]); what each answer means for a
//! particular task is `event::decide_file_phase`'s question, and the answers are
//! not symmetric:
//!
//! * an **absent** path is benign only for a task whose payload need not be
//!   there — an incomplete download, whose partial data Download Station cleans
//!   up after itself. For a task that finished (or whose counters say it
//!   downloaded everything) an absent path fails the item, because a payload
//!   that demonstrably existed and is not at the resolved path means the
//!   resolution is wrong, and removing the DSM task would orphan it;
//! * a **present** path is only accepted when [`FileEntry::isdir`] agrees with
//!   the kind the task resolved to, since `recursive=true` on the wrong kind of
//!   object removes something that is not this task's payload;
//! * an **error** or an unattributable answer is never read as absence.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::api::client::{SynoClient, VersionRange};
use crate::error::{Error, Result};

/// Path lookup — used for the pre-delete existence check.
pub const FS_LIST_API: &str = "SYNO.FileStation.List";
/// Version range this client implements for `SYNO.FileStation.List`.
///
/// **Pinned to v2, and v2 is required** — not a ceiling but a floor as well.
///
/// [`build_fs_path_params`] encodes `path` as a JSON array, which is the only
/// encoding safe for a delete: a filename may contain a comma, so the
/// comma-separated form v1 expects is ambiguous exactly where being wrong is
/// irreversible. **v1 does not accept a JSON array — it kills the backend CGI
/// and DSM's nginx answers `502 Bad Gateway`.** Verified against DSM 7
/// (`FamilyNas`, `SYNO.FileStation.List` min 1 / max 2): the identical request
/// 502s at `version=1` and returns `{"isdir":…,"path":…}` at `version=2`.
///
/// So the two are a matched pair — the JSON-array encoding and v2 — and
/// negotiating *down* to v1 is what breaks. Requiring v2 turns a NAS that
/// somehow lacks it into a clear `ApiUnavailable` at startup instead of a 502
/// mid-delete. DSM 7 always ships v2, which is the only DSM this tool targets.
///
/// Note this is the opposite situation to
/// [`crate::api::download_station::DS_TASK_SUPPORTED`], which is genuinely
/// pinned *down* to v1 because v2/v3 change the response shape `model.rs`
/// parses. Do not "make them consistent".
pub const FS_LIST_SUPPORTED: VersionRange = (2, 2);

/// File deletion.
pub const FS_DELETE_API: &str = "SYNO.FileStation.Delete";
/// Version range this client implements for `SYNO.FileStation.Delete`.
///
/// Pinned to v2 alongside [`FS_LIST_SUPPORTED`], and for the same reason:
/// `start` takes the same JSON-array `path` from [`build_fs_path_params`], so
/// it inherits v1's inability to parse it.
///
/// [`classify_delete_status`] reads `finished` and `path_err_num` from the
/// `status` payload. `status` itself answers identically on v1 and v2 (both
/// return `{"error":{"code":599}}` for an unknown taskid), but the field names
/// in a *real* in-progress payload are still unverified — the probe that
/// established the version pin was necessarily non-destructive. This is why
/// `confirm_deleted` re-checks the path with `getinfo` afterwards rather than
/// trusting `path_err_num` alone.
pub const FS_DELETE_SUPPORTED: VersionRange = (2, 2);

/// File Station's "no such file or directory".
pub const FS_NO_SUCH_FILE: i32 = 408;

/// How often the delete task is asked whether it has finished.
pub const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long a single File Station delete may run before it is given up on.
///
/// Bounds the *whole* operation rather than one request. Generous, because a
/// recursive delete of a very large directory is genuinely slow on a NAS with
/// spinning disks; finite, because a delete that never finishes must not wedge
/// the op task forever.
pub const DELETE_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Parameter construction (pure)
// ---------------------------------------------------------------------------

/// The `path` parameter every File Station method takes: a **JSON array**.
///
/// This is the encoding that differs from Download Station's comma-separated
/// ids, and it is why the two builders are separate functions rather than one
/// with a flag: a path may legally contain a comma, so the two encodings are
/// not interchangeable even for a single value.
pub fn build_fs_path_params(paths: &[String]) -> Vec<(&'static str, String)> {
    vec![("path", encode_path_list(paths))]
}

/// JSON-encode a list of paths. `serde_json` owns the escaping, so a path with
/// a quote or a backslash in it cannot break out of the array.
fn encode_path_list(paths: &[String]) -> String {
    serde_json::Value::from(paths.to_vec()).to_string()
}

/// Query parameters for `SYNO.FileStation.Delete` `method=start`.
///
/// `recursive=true` is the whole point — a torrent directory is never empty.
pub fn build_fs_delete_params(paths: &[String]) -> Vec<(&'static str, String)> {
    let mut params = build_fs_path_params(paths);
    params.push(("recursive", "true".to_string()));
    params
}

/// Query parameters for `SYNO.FileStation.Delete` `method=status`.
pub fn build_fs_delete_status_params(taskid: &str) -> Vec<(&'static str, String)> {
    vec![("taskid", taskid.to_string())]
}

// ---------------------------------------------------------------------------
// getinfo — the existence check
// ---------------------------------------------------------------------------

/// One entry of a `getinfo` response.
///
/// `code` is DSM's per-path error slot: a path that could not be stat-ed comes
/// back as an entry carrying a code rather than as a missing entry or an
/// envelope-level failure, and different DSM builds pick different ones of
/// those three. [`classify_getinfo`] handles all of them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileEntry {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub isdir: Option<bool>,
    #[serde(default)]
    pub code: Option<i32>,
}

/// The `data` object of a `getinfo` response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GetInfo {
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// What the NAS knows about a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathInfo {
    /// The NAS said, in so many words, that there is nothing there — a per-entry
    /// [`FS_NO_SUCH_FILE`], or an entry with neither an `isdir` nor an error
    /// code, which is how some builds answer a failed stat. **Not an error** —
    /// see the module docs.
    Missing,
    /// It exists.
    Found { is_dir: bool },
    /// It could not be looked up for some reason other than absence — a
    /// permission problem, most likely. Deliberately distinct from
    /// [`PathInfo::Missing`]: "I am not allowed to look" must not be read as
    /// "there is nothing to delete", which would orphan the files.
    Error(i32),
    /// The response carried **no entry that can be attributed to the requested
    /// path** — an absent or empty `files` array, or several entries none of
    /// which names the path we asked about.
    ///
    /// This is the shape a `getinfo` whose payload this client cannot read
    /// produces, since [`GetInfo::files`] defaults to empty. It must never be
    /// collapsed into [`PathInfo::Missing`]: doing so would report "the files
    /// were already gone" for every item of a batch and delete every DSM task
    /// while nothing at all was reclaimed.
    Unknown,
}

/// Read a `getinfo` payload for one path.
///
/// Pure, so every shape DSM might answer with is covered by a test rather than
/// by a hopeful `unwrap`. An entry with no `isdir` and no error code is treated
/// as absent: it is the shape some builds use for a path they could not stat,
/// and "assume it is not there" is the direction that issues no destructive
/// call.
///
/// **The entry has to be attributable to the path that was asked about.** An
/// exact match on `path` is preferred; a *lone* entry is accepted even when it
/// does not match, because exactly one path was requested and DSM sometimes
/// echoes it in a different form (a trailing slash, a resolved `/volumeN/…`).
/// Several entries with no match, or no entries at all, is
/// [`PathInfo::Unknown`] — reading someone else's entry would let a
/// `Found { is_dir: true }` for a *different* directory authorize the recursive
/// delete of this one.
pub fn classify_getinfo(info: &GetInfo, path: &str) -> PathInfo {
    let lone_entry = match info.files.as_slice() {
        [only] => Some(only),
        _ => None,
    };
    let entry = info
        .files
        .iter()
        .find(|entry| entry.path == path)
        .or(lone_entry);

    let Some(entry) = entry else {
        return PathInfo::Unknown;
    };

    match entry.code {
        Some(FS_NO_SUCH_FILE) => PathInfo::Missing,
        Some(code) if code != 0 => PathInfo::Error(code),
        _ => match entry.isdir {
            Some(is_dir) => PathInfo::Found { is_dir },
            None => PathInfo::Missing,
        },
    }
}

/// Ask the NAS whether a path exists — the semantic guard, run immediately
/// before any recursive delete.
pub async fn path_info(client: &SynoClient, path: &str) -> Result<PathInfo> {
    let paths = [path.to_string()];
    let params = build_fs_path_params(&paths);
    match client
        .call::<GetInfo>(FS_LIST_API, "getinfo", FS_LIST_SUPPORTED, &params)
        .await
    {
        Ok(info) => Ok(classify_getinfo(&info, path)),
        // Some builds report an absent path at the envelope level instead of
        // per entry. Same answer either way.
        Err(Error::Dsm { code, .. }) if code == FS_NO_SUCH_FILE => Ok(PathInfo::Missing),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// delete — start, then poll
// ---------------------------------------------------------------------------

/// The `data` object of `method=start`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteStarted {
    pub taskid: String,
}

/// The `data` object of `method=status`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeleteStatus {
    #[serde(default)]
    pub finished: bool,
    /// How many paths the task could not remove. Non-zero means the delete
    /// completed *and* did not do what was asked.
    #[serde(default)]
    pub path_err_num: u32,
    #[serde(default)]
    pub processed_num: u32,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub processing_path: Option<String>,
}

/// How a poll of a delete task reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteProgress {
    /// Still working; poll again.
    Running,
    /// Everything asked for is gone.
    Finished,
    /// Finished, but this many paths were not removed.
    Failed(u32),
}

/// Read a `status` payload. Pure, so the "finished but with errors" case — the
/// one that must **not** be mistaken for success — is pinned by a test.
pub fn classify_delete_status(status: &DeleteStatus) -> DeleteProgress {
    if !status.finished {
        return DeleteProgress::Running;
    }
    if status.path_err_num > 0 {
        return DeleteProgress::Failed(status.path_err_num);
    }
    DeleteProgress::Finished
}

/// Recursively delete paths, waiting for the NAS to actually finish.
///
/// `start` hands back a task id and returns immediately; the delete is only
/// done when `status` says so. Bounded by [`DELETE_TIMEOUT`] overall.
pub async fn delete_paths(client: &SynoClient, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let params = build_fs_delete_params(paths);
    let started: DeleteStarted = client
        .call(FS_DELETE_API, "start", FS_DELETE_SUPPORTED, &params)
        .await?;
    tracing::info!(
        taskid = %started.taskid,
        paths = paths.len(),
        "File Station delete started"
    );

    await_delete(client, &started.taskid, DELETE_TIMEOUT).await
}

/// Poll a delete task until it finishes, fails, or runs out of time.
async fn await_delete(client: &SynoClient, taskid: &str, timeout: Duration) -> Result<()> {
    let params = build_fs_delete_status_params(taskid);
    let deadline = Instant::now() + timeout;

    loop {
        let status: DeleteStatus = client
            .call(FS_DELETE_API, "status", FS_DELETE_SUPPORTED, &params)
            .await?;

        match classify_delete_status(&status) {
            DeleteProgress::Finished => {
                tracing::info!(taskid, "File Station delete finished");
                return Ok(());
            }
            DeleteProgress::Failed(errors) => {
                return Err(Error::operation_failed(format!(
                    "File Station could not delete {errors} path(s) (task {taskid})"
                )));
            }
            DeleteProgress::Running => {}
        }

        if Instant::now() + DELETE_POLL_INTERVAL >= deadline {
            return Err(Error::timed_out(format!(
                "File Station delete task {taskid} did not finish within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(DELETE_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;
    use crate::api::download_station::build_ds_id_params;

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|p| (*p).to_string()).collect()
    }

    // ---- parameter encoding ------------------------------------------------

    #[test]
    fn a_single_path_is_still_a_json_array() {
        assert_eq!(
            build_fs_path_params(&paths(&["/downloads/Some.Release"])),
            vec![("path", r#"["/downloads/Some.Release"]"#.to_string())]
        );
    }

    #[test]
    fn several_paths_are_one_json_array() {
        assert_eq!(
            build_fs_path_params(&paths(&["/downloads/A", "/video/movies/B"])),
            vec![("path", r#"["/downloads/A","/video/movies/B"]"#.to_string())]
        );
    }

    #[test]
    fn an_empty_list_encodes_as_an_empty_array() {
        assert_eq!(build_fs_path_params(&[]), vec![("path", "[]".to_string())]);
    }

    #[test]
    fn a_path_with_json_metacharacters_is_escaped_not_broken() {
        // Scene names contain brackets and quotes constantly; a hand-rolled
        // `format!("[\"{path}\"]")` would produce invalid JSON for these.
        let encoded = build_fs_path_params(&paths(&[r#"/downloads/He said "hi"\x"#]));
        let (_, value) = &encoded[0];
        let decoded: Vec<String> = serde_json::from_str(value).expect("valid JSON");
        assert_eq!(decoded, vec![r#"/downloads/He said "hi"\x"#.to_string()]);
    }

    #[test]
    fn a_path_containing_a_comma_survives_the_json_encoding() {
        // The reason the two APIs cannot share one builder: this path would be
        // two paths under Download Station's comma-separated encoding.
        let encoded = build_fs_path_params(&paths(&["/downloads/Artist - A, B and C"]));
        let (_, value) = &encoded[0];
        let decoded: Vec<String> = serde_json::from_str(value).expect("valid JSON");
        assert_eq!(decoded.len(), 1);
    }

    #[test]
    fn file_station_and_download_station_encode_lists_differently() {
        // The plan's ⚠ note, pinned: JSON array here, comma-separated string
        // there. Swapping them yields a silently wrong request, not an error.
        let values = paths(&["a", "b"]);
        assert_eq!(build_fs_path_params(&values)[0].1, r#"["a","b"]"#);
        assert_eq!(build_ds_id_params(&values)[0].1, "a,b");
        assert_eq!(build_fs_path_params(&values)[0].0, "path");
        assert_eq!(build_ds_id_params(&values)[0].0, "id");
    }

    #[test]
    fn delete_start_params_are_recursive() {
        assert_eq!(
            build_fs_delete_params(&paths(&["/downloads/Some.Release"])),
            vec![
                ("path", r#"["/downloads/Some.Release"]"#.to_string()),
                ("recursive", "true".to_string()),
            ]
        );
    }

    #[test]
    fn delete_status_params_carry_only_the_task_id() {
        assert_eq!(
            build_fs_delete_status_params("FileStation_51D8CE3CB4D89622"),
            vec![("taskid", "FileStation_51D8CE3CB4D89622".to_string())]
        );
    }

    // ---- getinfo -----------------------------------------------------------

    fn getinfo(body: &str) -> GetInfo {
        parse_envelope(body, FS_LIST_API).expect("a getinfo payload")
    }

    #[test]
    fn an_existing_directory_is_found() {
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/Some.Release", "name": "Some.Release", "isdir": true,
                 "additional": {"size": 4096}}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/Some.Release"),
            PathInfo::Found { is_dir: true }
        );
    }

    #[test]
    fn an_existing_file_is_found_too() {
        // A single-file torrent resolves to the file itself, not a directory.
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/arch.iso", "name": "arch.iso", "isdir": false}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/arch.iso"),
            PathInfo::Found { is_dir: false }
        );
    }

    #[test]
    fn a_per_entry_408_means_the_path_is_gone() {
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/Gone", "name": "Gone", "code": 408}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/Gone"),
            PathInfo::Missing
        );
    }

    #[test]
    fn an_entry_with_no_isdir_is_treated_as_gone() {
        // Some builds answer a failed stat with a bare entry. Assuming absence
        // issues no destructive call, which is the safe direction.
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/Gone", "name": "Gone"}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/Gone"),
            PathInfo::Missing
        );
    }

    #[test]
    fn no_entry_at_all_is_unknown_never_gone() {
        // The shape a payload this client cannot read produces, since `files`
        // is `#[serde(default)]`. Calling it `Missing` would report "the files
        // were already gone" for every item of a batch, delete every DSM task,
        // and reclaim nothing.
        let info = getinfo(r#"{"success": true, "data": {"files": []}}"#);
        assert_eq!(
            classify_getinfo(&info, "/downloads/Gone"),
            PathInfo::Unknown
        );
        assert_eq!(
            classify_getinfo(&GetInfo::default(), "/downloads/Gone"),
            PathInfo::Unknown
        );
        // A `data` object with no `files` key at all — the getinfo shape
        // mismatch this variant exists for.
        let info = getinfo(r#"{"success": true, "data": {"total": 1}}"#);
        assert_eq!(
            classify_getinfo(&info, "/downloads/Gone"),
            PathInfo::Unknown
        );
    }

    #[test]
    fn a_lone_entry_answers_for_the_one_path_that_was_asked_about() {
        // Exactly one path is ever requested, so a single entry is that path
        // however DSM chose to spell it back — a trailing slash here.
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/Some.Release/", "name": "Some.Release", "isdir": true}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/Some.Release"),
            PathInfo::Found { is_dir: true }
        );
    }

    #[test]
    fn an_entry_for_a_different_path_never_authorizes_deleting_this_one() {
        // The hazard the lone-entry fallback would otherwise open: a
        // `Found { is_dir: true }` belonging to some *other* directory is what
        // the caller acts on, and the recursive delete goes ahead against a
        // path nothing confirmed.
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/A", "name": "A", "isdir": true},
                {"path": "/downloads/B", "name": "B", "isdir": true}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/C"),
            PathInfo::Unknown,
            "an unmatched path among several entries must not borrow one of them"
        );
    }

    #[test]
    fn a_permission_error_is_not_reported_as_absence() {
        // The distinction that matters: "I may not look" must never become
        // "there is nothing to delete", which would delete the task and leave
        // the files behind.
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/Locked", "name": "Locked", "code": 403}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/Locked"),
            PathInfo::Error(403)
        );
    }

    #[test]
    fn a_zero_code_is_success_not_a_failure() {
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/X", "name": "X", "isdir": true, "code": 0}
            ]}}"#,
        );
        assert_eq!(
            classify_getinfo(&info, "/downloads/X"),
            PathInfo::Found { is_dir: true }
        );
    }

    #[test]
    fn the_entry_matching_the_requested_path_is_the_one_read() {
        let info = getinfo(
            r#"{"success": true, "data": {"files": [
                {"path": "/downloads/A", "name": "A", "code": 408},
                {"path": "/downloads/B", "name": "B", "isdir": true}
            ]}}"#,
        );
        assert_eq!(classify_getinfo(&info, "/downloads/A"), PathInfo::Missing);
        assert_eq!(
            classify_getinfo(&info, "/downloads/B"),
            PathInfo::Found { is_dir: true }
        );
    }

    // ---- delete status -----------------------------------------------------

    #[test]
    fn a_start_response_yields_the_task_id() {
        let started: DeleteStarted = parse_envelope(
            r#"{"success": true, "data": {"taskid": "FileStation_1"}}"#,
            FS_DELETE_API,
        )
        .expect("a start payload");
        assert_eq!(started.taskid, "FileStation_1");
    }

    #[test]
    fn an_unfinished_status_keeps_polling() {
        let status: DeleteStatus = parse_envelope(
            r#"{"success": true, "data": {
                "finished": false, "processed_num": 3, "total": 40,
                "processing_path": "/downloads/X/a.mkv", "progress": 0.07
            }}"#,
            FS_DELETE_API,
        )
        .expect("a status payload");
        assert_eq!(classify_delete_status(&status), DeleteProgress::Running);
        assert_eq!(
            status.processing_path.as_deref(),
            Some("/downloads/X/a.mkv")
        );
    }

    #[test]
    fn a_finished_status_with_no_errors_is_success() {
        let status: DeleteStatus = parse_envelope(
            r#"{"success": true, "data": {
                "finished": true, "path_err_num": 0, "processed_num": 40, "total": 40
            }}"#,
            FS_DELETE_API,
        )
        .expect("a status payload");
        assert_eq!(classify_delete_status(&status), DeleteProgress::Finished);
    }

    #[test]
    fn a_finished_status_with_path_errors_is_a_failure_not_a_success() {
        // The case worth having a test for: `finished: true` alone would read
        // as "the files are gone" while the payload says two of them are not.
        let status: DeleteStatus = parse_envelope(
            r#"{"success": true, "data": {"finished": true, "path_err_num": 2}}"#,
            FS_DELETE_API,
        )
        .expect("a status payload");
        assert_eq!(classify_delete_status(&status), DeleteProgress::Failed(2));
    }

    #[test]
    fn a_status_payload_with_nothing_in_it_is_not_mistaken_for_done() {
        assert_eq!(
            classify_delete_status(&DeleteStatus::default()),
            DeleteProgress::Running
        );
    }

    // ---- delete_paths ------------------------------------------------------

    #[tokio::test]
    async fn deleting_an_empty_list_of_paths_issues_no_request_at_all() {
        // The guard that makes an all-refused batch cost nothing. The client
        // below has an empty API map (`discover()` was never called), so any
        // request would fail in `endpoint()` — `Ok(())` is therefore positive
        // proof that the early return fired.
        let client = crate::testutil::offline_client();
        delete_paths(&client, &[])
            .await
            .expect("no paths means no call");
    }

    // ---- constants ---------------------------------------------------------

    #[test]
    fn the_two_file_station_apis_require_v2_because_v1_cannot_parse_our_paths() {
        // Not a ceiling — a floor. `build_fs_path_params` sends `path` as a
        // JSON array (the only encoding safe for a filename containing a
        // comma), and v1 does not accept one: it kills the backend CGI and DSM
        // answers 502 Bad Gateway. Verified against a real DSM 7 NAS, where the
        // identical `getinfo` 502s at version=1 and succeeds at version=2.
        //
        // Lowering either bound to 1 re-breaks every delete on every NAS. If
        // the JSON-array encoding is ever replaced, revisit this together with
        // it — the encoding and the version are a matched pair.
        assert_eq!(FS_LIST_SUPPORTED, (2, 2));
        assert_eq!(FS_DELETE_SUPPORTED, (2, 2));
    }

    #[test]
    fn a_real_nas_getinfo_response_deserializes() {
        // Captured verbatim from DSM 7 (`SYNO.FileStation.List` v2 `getinfo`).
        // Three shapes in one: a file, a directory, and a path that is not
        // there. The absent entry carries `code` and no `isdir`, which is what
        // `classify_getinfo` keys `PathInfo::Missing` off.
        let file = r#"{"data":{"files":[{"isdir":false,"name":"a.mp4","path":"/video/a.mp4"}]},"success":true}"#;
        let dir =
            r#"{"data":{"files":[{"isdir":true,"name":"video","path":"/video"}]},"success":true}"#;
        let gone = r#"{"data":{"files":[{"code":408,"path":"/video/nope"}]},"success":true}"#;

        let parse = |body: &str| {
            crate::api::client::parse_envelope::<GetInfo>(body, FS_LIST_API)
                .expect("a real NAS response must deserialize")
        };

        assert_eq!(
            classify_getinfo(&parse(file), "/video/a.mp4"),
            PathInfo::Found { is_dir: false }
        );
        assert_eq!(
            classify_getinfo(&parse(dir), "/video"),
            PathInfo::Found { is_dir: true }
        );
        assert_eq!(
            classify_getinfo(&parse(gone), "/video/nope"),
            PathInfo::Missing
        );
    }

    #[test]
    fn a_file_station_error_code_reads_as_words_not_as_a_number() {
        // 403 on File Station is a permission problem, and it is the code the
        // delete path is most likely to surface. The common table has no entry
        // for it, so without the File Station table this said "unrecognized DSM
        // error code 403".
        let rendered = Error::dsm(403, FS_LIST_API).to_string();
        assert!(rendered.contains("permission denied"), "{rendered}");
        let rendered = Error::dsm(FS_NO_SUCH_FILE, FS_DELETE_API).to_string();
        assert!(rendered.contains("no such file"), "{rendered}");
    }

    #[test]
    fn the_delete_wait_is_bounded_and_longer_than_one_poll() {
        assert!(DELETE_TIMEOUT > DELETE_POLL_INTERVAL);
        // A recursive delete is deliberately allowed to outlive the per-request
        // HTTP timeout — that is the entire reason for the polling form.
        assert!(DELETE_TIMEOUT > crate::api::client::REQUEST_TIMEOUT);
    }
}
