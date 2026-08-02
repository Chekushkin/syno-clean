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
//! anything is deleted, the resolved path is looked up, and a path that is not
//! there is reported as *skipped* rather than as an error the user has to chase.
//! For an incomplete task that is the expected answer — Download Station cleans
//! up its own partial data — and for a finished one it usually means the folder
//! was already removed by hand.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::api::client::{SynoClient, VersionRange};
use crate::error::{Error, Result};

/// Path lookup — used for the pre-delete existence check.
pub const FS_LIST_API: &str = "SYNO.FileStation.List";
/// Version range this client implements for `SYNO.FileStation.List`.
///
/// Only `path`, `name`, `isdir` and the per-path `code` are read, and those are
/// stable across v1 and v2.
pub const FS_LIST_SUPPORTED: VersionRange = (1, 2);

/// File deletion.
pub const FS_DELETE_API: &str = "SYNO.FileStation.Delete";
/// Version range this client implements for `SYNO.FileStation.Delete`.
pub const FS_DELETE_SUPPORTED: VersionRange = (1, 2);

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
    /// Nothing there. **Not an error** — see the module docs.
    Missing,
    /// It exists.
    Found { is_dir: bool },
    /// It could not be looked up for some reason other than absence — a
    /// permission problem, most likely. Deliberately distinct from
    /// [`PathInfo::Missing`]: "I am not allowed to look" must not be read as
    /// "there is nothing to delete", which would orphan the files.
    Error(i32),
}

/// Read a `getinfo` payload for one path.
///
/// Pure, so every shape DSM might answer with is covered by a test rather than
/// by a hopeful `unwrap`. An entry with no `isdir` and no error code is treated
/// as absent: it is the shape some builds use for a path they could not stat,
/// and "assume it is not there" is the direction that issues no destructive
/// call.
pub fn classify_getinfo(info: &GetInfo, path: &str) -> PathInfo {
    let entry = info
        .files
        .iter()
        .find(|entry| entry.path == path)
        .or_else(|| info.files.first());

    let Some(entry) = entry else {
        return PathInfo::Missing;
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
                return Err(fs_error(format!(
                    "File Station could not delete {errors} path(s) (task {taskid})"
                )));
            }
            DeleteProgress::Running => {}
        }

        if Instant::now() + DELETE_POLL_INTERVAL >= deadline {
            return Err(fs_timeout(format!(
                "File Station delete task {taskid} did not finish within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(DELETE_POLL_INTERVAL).await;
    }
}

/// A File Station failure with no DSM code behind it.
///
/// Reuses [`Error::Io`] rather than adding an enum variant, the same way
/// `api::client` reuses [`Error::Parse`] for protocol violations: the delete
/// did not happen, and the plan's variant list stays as documented.
fn fs_error(message: String) -> Error {
    Error::Io(std::io::Error::other(message))
}

/// A bounded wait that ran out.
fn fs_timeout(message: String) -> Error {
    Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, message))
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
    fn no_entry_at_all_means_gone() {
        let info = getinfo(r#"{"success": true, "data": {"files": []}}"#);
        assert_eq!(
            classify_getinfo(&info, "/downloads/Gone"),
            PathInfo::Missing
        );
        assert_eq!(
            classify_getinfo(&GetInfo::default(), "/downloads/Gone"),
            PathInfo::Missing
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

    // ---- constants ---------------------------------------------------------

    #[test]
    fn the_delete_wait_is_bounded_and_longer_than_one_poll() {
        assert!(DELETE_TIMEOUT > DELETE_POLL_INTERVAL);
        // A recursive delete is deliberately allowed to outlive the per-request
        // HTTP timeout — that is the entire reason for the polling form.
        assert!(DELETE_TIMEOUT > crate::api::client::REQUEST_TIMEOUT);
    }
}
