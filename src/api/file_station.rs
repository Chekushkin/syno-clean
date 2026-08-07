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
//!
//! One thing here is **not** part of a delete: [`volume_usage`] reads
//! `list_share` for the storage band. It lives in this module because it is the
//! same API and the same version pin, but it is display-only, it is the one call
//! in the crate that deliberately bypasses [`SynoClient::call`], and its failure
//! is never fatal. See its doc comment before touching it.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::api::client::{SynoClient, VersionRange, parse_envelope};
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
/// [`build_list_share_params`] then *strengthens* the same pin rather than
/// merely riding on it: `list_share`'s `additional` is a JSON array on v2 and a
/// comma-separated list on v1, so the storage read is a second call on this API
/// whose encoding v1 would also misread. Two callers now depend on v2, and the
/// pin has two reasons to stay where it is.
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

/// Query parameters for `SYNO.FileStation.List` `method=list_share`.
///
/// `volume_status` is the free/total pair the storage band draws; `real_path` is
/// what makes the answer *dedupable*, since every share on one volume reports
/// the same numbers and only the resolved path names the volume they belong to.
///
/// The array goes through `serde_json` for the same reason
/// [`encode_path_list`] does — the encoding is owned in one place rather than
/// spelled as a literal that a later edit can quietly malform.
pub fn build_list_share_params() -> Vec<(&'static str, String)> {
    let additional = serde_json::Value::from(vec!["real_path", "volume_status"]).to_string();
    vec![("additional", additional)]
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

// ---------------------------------------------------------------------------
// list_share — how full the volumes are
// ---------------------------------------------------------------------------

/// The `data` object of `SYNO.FileStation.List` `method=list_share`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShareList {
    #[serde(default)]
    pub shares: Vec<Share>,
}

/// One share as `list_share` reports it.
///
/// Every field below `shares` is optional, exactly as `model.rs` treats a
/// task's `additional` sub-blocks and for the same reason: one share DSM
/// describes oddly must not blank the whole band. A share this client cannot
/// read fully is skipped by [`collect_volume_usage`], not fatal.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Share {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub additional: Option<ShareAdditional>,
}

/// The `additional` block requested by [`build_list_share_params`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShareAdditional {
    /// The resolved path — `/volume1/downloads` where `path` says `/downloads`.
    /// Its first component is the mount point, which is the only thing in the
    /// response that distinguishes one volume from another.
    #[serde(default)]
    pub real_path: Option<String>,
    #[serde(default)]
    pub volume_status: Option<VolumeStatus>,
}

/// Free and total bytes of the volume a share lives on.
///
/// Both sizes go through `model::de_u64` because DSM sends the same
/// numeric field as a JSON number on one build and as a string on the next — a
/// plain `u64` here is the bug the task model already guards against, and it
/// would fail the *whole* payload rather than one field.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VolumeStatus {
    #[serde(default, deserialize_with = "crate::model::de_u64")]
    pub freespace: u64,
    #[serde(default, deserialize_with = "crate::model::de_u64")]
    pub totalspace: u64,
}

/// One volume's occupancy, deduped across the shares that live on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeUsage {
    /// The mount point as DSM spells it — `volume1`, `volumeUSB1`, … Display
    /// only: nothing resolves a path through it.
    pub name: String,
    pub total: u64,
    pub free: u64,
}

impl VolumeUsage {
    /// Bytes in use. Saturating, because a NAS that reports `free > total`
    /// mid-scrub is a display oddity, not a panic.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Occupancy as a `0.0..=1.0` fraction, matching what
    /// [`crate::format::percent`] takes — and what the storage bar will.
    ///
    /// A zero-size volume is ordinary rather than an error — the same guarded
    /// denominator as [`crate::model::Task::progress`].
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.used() as f64 / self.total as f64
    }
}

/// Collapse a `list_share` payload into one entry per volume. Pure.
///
/// Shares on one volume all report the *same* `volume_status`, so the raw list
/// would draw the same bar once per share. The rules, in order:
///
/// 1. a share with no `volume_status`, or whose `totalspace` is 0, is skipped —
///    there is nothing to draw a bar of;
/// 2. the volume key is the first component of `real_path` when that path is
///    absolute and the component is a mount point;
/// 3. a share whose `real_path` is absent or does not look like a mount is
///    **skipped**, never given a synthetic key. Keying on `{total}:{free}`
///    instead would merge two genuinely distinct volumes and label them with a
///    name DSM never sent — refusing to display beats displaying an invention;
/// 4. the first share seen for a key wins, and the result is sorted by name so
///    the band keeps a stable order across polls instead of reshuffling under
///    the user every time DSM returns the shares in a different order.
pub fn collect_volume_usage(list: &ShareList) -> Vec<VolumeUsage> {
    let mut volumes: Vec<VolumeUsage> = Vec::new();

    for share in &list.shares {
        let Some(additional) = share.additional.as_ref() else {
            continue;
        };
        let Some(status) = additional.volume_status.as_ref() else {
            continue;
        };
        if status.totalspace == 0 {
            continue;
        }
        let Some(name) = additional.real_path.as_deref().and_then(mount_component) else {
            continue;
        };
        if volumes.iter().any(|known| known.name == name) {
            continue;
        }
        volumes.push(VolumeUsage {
            name: name.to_string(),
            total: status.totalspace,
            free: status.freespace,
        });
    }

    volumes.sort_by(|left, right| left.name.cmp(&right.name));
    volumes
}

/// The mount component of an absolute DSM path, if it has one.
///
/// On DSM the first component of an absolute *real* path is always the mount
/// point, and every spelling starts with `volume` (`/volume1`, `/volumeUSB1`,
/// `/volumeSATA2`, the bare `/volume`).
///
/// This deliberately does **not** reuse `delete`'s stricter mount test, and the
/// duplication is the point: there, mis-reading a component re-roots a
/// recursive delete, so the rule matches by exact shape. Here the component is
/// only ever a label on a progress bar and a dedupe key, so the looser prefix
/// test costs nothing if it is ever wrong — and coupling a display helper to a
/// guard whose whole job is to be paranoid invites "simplifying" the guard.
fn mount_component(real_path: &str) -> Option<&str> {
    let first = real_path.strip_prefix('/')?.split('/').next()?;
    first.starts_with("volume").then_some(first)
}

/// Ask the NAS how full its volumes are.
///
/// ⚠️ **This deliberately does not go through [`SynoClient::call`], and must
/// never be "simplified" to.** `call` treats DSM 105 as a possibly-stale
/// session: it throws away the working sid, re-logs-in, and — if 105 survives
/// the fresh session — latches `permission_is_real` **client-wide, not per
/// API**. That latch then disables the 105 retry for every API, including
/// `SYNO.DownloadStation.Task`.
///
/// A restricted download-only account is exactly what this tool is usually
/// pointed at, and exactly the kind that answers 105 to `list_share`. Routing
/// the storage read through `call` would therefore force a re-login on the first
/// storage poll and then leave a genuinely stale Download Station session
/// unrepairable — reinstating the failure where every poll fails until
/// `session.json` is deleted by hand.
///
/// So this uses the documented no-retry escape hatch: [`SynoClient::endpoint`] +
/// [`SynoClient::send`] + [`parse_envelope`]. The cost is that a storage read
/// against a genuinely expired session simply fails; the *task* poller repairs
/// the session a moment later and the next storage read succeeds. That is the
/// right trade for a display-only number.
pub async fn volume_usage(client: &SynoClient) -> Result<Vec<VolumeUsage>> {
    let endpoint = client.endpoint(FS_LIST_API, FS_LIST_SUPPORTED)?;
    let params = build_list_share_params();
    let body = client
        .send(&endpoint, "list_share", &params, client.sid())
        .await?;
    let list: ShareList = parse_envelope(&body, FS_LIST_API)?;
    Ok(collect_volume_usage(&list))
}

#[cfg(test)]
mod tests {
    use super::*;
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
