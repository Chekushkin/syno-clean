//! `SYNO.DownloadStation.Task` — listing, pausing and deleting tasks.
//!
//! DSM 7 also ships `SYNO.DownloadStation2.Task` (what the web UI drives), but
//! its `list` method is undocumented, returns numeric statuses and a different
//! `additional` shape. This client uses the documented **v1** API, which is
//! still present and supported on DSM 7 and returns the string statuses and
//! object file lists [`crate::model`] is built around — hence
//! [`DS_TASK_SUPPORTED`] being pinned to `(1, 1)` rather than following the
//! NAS up to whatever it advertises. List, delete, pause and resume all come
//! from the same v1 API, so there is no mixed-API seam.
//!
//! Parameter construction is a pure function ([`build_list_params`],
//! [`build_ds_id_params`]) per the `build_*_params` convention: Download
//! Station encodes list-valued parameters as **comma-separated strings**, while
//! File Station wants JSON arrays, and that difference is worth having in
//! exactly one testable place.
//!
//! ⚠️ **`delete`, `pause` and `resume` report failure per task, not in the
//! envelope.** They answer `{"success": true, "data": [{"id": …, "error": 0}]}`
//! even when a task could not be touched, so [`check_task_results`] is the step
//! that turns a non-zero per-item code into an error. Reading only `success`
//! would report a failed delete as a success — and the delete ordering depends
//! on knowing that a pause actually happened.

use serde::Deserialize;

use crate::api::client::{SynoClient, VersionRange};
use crate::error::{Error, Result};
use crate::model::{Task, TaskList};

/// The Download Station task API.
pub const DS_TASK_API: &str = "SYNO.DownloadStation.Task";

/// Version range this client implements.
///
/// Pinned to v1 deliberately — see the module docs. Widening this without
/// reworking [`crate::model`] would silently change the status encoding.
pub const DS_TASK_SUPPORTED: VersionRange = (1, 1);

/// The `additional` blocks the task table needs: `detail` for destination and
/// peer counts, `transfer` for progress and speeds, `file` for the on-disk
/// name that `delete.rs` resolves paths from.
pub const LIST_ADDITIONAL: [&str; 3] = ["detail", "transfer", "file"];

/// DSM's "no limit" sentinel for `limit`.
const NO_LIMIT: &str = "-1";

/// Query parameters for `method=list`.
///
/// `limit = None` asks for every task, which is what the TUI always wants —
/// the poller reconciles the whole list each tick, and paging would only make
/// the cursor and selection reconciliation lie.
pub fn build_list_params(offset: u32, limit: Option<u32>) -> Vec<(&'static str, String)> {
    vec![
        ("additional", LIST_ADDITIONAL.join(",")),
        ("offset", offset.to_string()),
        (
            "limit",
            limit.map_or_else(|| NO_LIMIT.to_string(), |l| l.to_string()),
        ),
    ]
}

/// Fetch every task on the NAS.
pub async fn list_tasks(client: &SynoClient) -> Result<Vec<Task>> {
    let params = build_list_params(0, None);
    let list: TaskList = client
        .call(DS_TASK_API, "list", DS_TASK_SUPPORTED, &params)
        .await?;
    tracing::debug!(
        total = list.total,
        returned = list.tasks.len(),
        "listed Download Station tasks"
    );
    Ok(list.tasks)
}

/// The raw `list` response body, for the hidden `--dump-tasks-json` flag.
///
/// This is how `tests/fixtures/task_list.json` gets captured from a real NAS,
/// so it deliberately returns the untouched body rather than anything parsed.
pub async fn list_tasks_json(client: &SynoClient) -> Result<String> {
    let params = build_list_params(0, None);
    client
        .call_text(DS_TASK_API, "list", DS_TASK_SUPPORTED, &params)
        .await
}

// ---------------------------------------------------------------------------
// Per-task operations: getinfo, pause, delete
// ---------------------------------------------------------------------------

/// The `id` parameter shared by `getinfo`, `delete`, `pause` and `resume`.
///
/// **Comma-separated**, which is the Download Station v1 encoding — File
/// Station spells the same idea as a JSON array (see
/// [`crate::api::file_station::build_fs_path_params`]). Task ids are opaque
/// DSM handles like `dbid_042` and never contain a comma, so the encoding is
/// unambiguous here in a way it would not be for paths.
pub fn build_ds_id_params(ids: &[String]) -> Vec<(&'static str, String)> {
    vec![("id", ids.join(","))]
}

/// Query parameters for `method=delete`.
///
/// `force_complete=false` matters: the `true` form tells Download Station to
/// mark an unfinished task complete and *keep* what it downloaded, which is the
/// opposite of what this program is for.
pub fn build_delete_params(ids: &[String]) -> Vec<(&'static str, String)> {
    let mut params = build_ds_id_params(ids);
    params.push(("force_complete", "false".to_string()));
    params
}

/// Query parameters for `method=getinfo`, asking for the same `additional`
/// blocks the list does so the result parses into the same [`Task`].
pub fn build_getinfo_params(ids: &[String]) -> Vec<(&'static str, String)> {
    let mut params = build_ds_id_params(ids);
    params.push(("additional", LIST_ADDITIONAL.join(",")));
    params
}

/// One entry of the per-task result array that `delete` / `pause` / `resume`
/// answer with. `error` is `0` on success.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskOpResult {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub error: i32,
}

/// Collapse a per-task result array into one [`Result`].
///
/// The first non-zero code wins; there is nothing useful to do with the rest,
/// and the caller acts on one task at a time anyway.
///
/// ⚠️ **An empty slice is `Ok`**, which is only correct when the caller has
/// already established that the slice covers the task it asked about. Callers
/// acting on a specific id want [`check_task_result`] instead.
pub fn check_task_results(results: &[TaskOpResult]) -> Result<()> {
    match results.iter().find(|result| result.error != 0) {
        Some(failed) => Err(Error::dsm(failed.error, DS_TASK_API)),
        None => Ok(()),
    }
}

/// What the per-task result array says about **one specific id**.
///
/// The distinction from [`check_task_results`] is the whole point: DSM
/// answering `{"success": true, "data": []}` says nothing about the task that
/// was requested, and reading that as success reports a delete that did not
/// happen — by which time the *files* are already gone, so the task is left
/// pointing at nothing and the user is told it was removed. An id the NAS
/// reported nothing for is therefore a failure.
pub fn check_task_result(id: &str, results: &[TaskOpResult]) -> Result<()> {
    match results.iter().find(|result| result.id == id) {
        Some(result) => check_task_results(std::slice::from_ref(result)),
        None => Err(Error::Io(std::io::Error::other(format!(
            "DSM reported no result for task {id}"
        )))),
    }
}

/// Fetch the current state of specific tasks.
///
/// Used to **confirm a pause took effect** before anything is deleted: the
/// pause call returning `error: 0` says DSM accepted the request, not that the
/// task has stopped writing.
pub async fn task_info(client: &SynoClient, ids: &[String]) -> Result<Vec<Task>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let params = build_getinfo_params(ids);
    let list: TaskList = client
        .call(DS_TASK_API, "getinfo", DS_TASK_SUPPORTED, &params)
        .await?;
    Ok(list.tasks)
}

/// Pause tasks. Returns the per-task results; see [`check_task_results`].
pub async fn pause_tasks(client: &SynoClient, ids: &[String]) -> Result<Vec<TaskOpResult>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let params = build_ds_id_params(ids);
    let results: Vec<TaskOpResult> = client
        .call(DS_TASK_API, "pause", DS_TASK_SUPPORTED, &params)
        .await?;
    tracing::info!(count = ids.len(), "paused Download Station tasks");
    Ok(results)
}

/// Resume tasks. Returns the per-task results; see [`check_task_results`].
///
/// The exact mirror of [`pause_tasks`] — same id encoding, same per-task result
/// array, same trap of a `success: true` envelope hiding a task that did not
/// move.
pub async fn resume_tasks(client: &SynoClient, ids: &[String]) -> Result<Vec<TaskOpResult>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let params = build_ds_id_params(ids);
    let results: Vec<TaskOpResult> = client
        .call(DS_TASK_API, "resume", DS_TASK_SUPPORTED, &params)
        .await?;
    tracing::info!(count = ids.len(), "resumed Download Station tasks");
    Ok(results)
}

/// Remove tasks from Download Station.
///
/// **This does not touch the files** — that is the entire reason this program
/// exists. The payload goes via [`crate::api::file_station::delete_paths`],
/// and it goes *first*; see `delete::plan_delete_ops` for the ordering.
pub async fn delete_tasks(client: &SynoClient, ids: &[String]) -> Result<Vec<TaskOpResult>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let params = build_delete_params(ids);
    let results: Vec<TaskOpResult> = client
        .call(DS_TASK_API, "delete", DS_TASK_SUPPORTED, &params)
        .await?;
    tracing::info!(count = ids.len(), "deleted Download Station tasks");
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;

    #[test]
    fn list_params_encode_additional_as_a_comma_separated_string() {
        // Download Station v1 takes `a,b,c`; File Station would want a JSON
        // array here. Getting this wrong yields a task list with no detail
        // blocks rather than an error, so it is pinned.
        let params = build_list_params(0, None);
        assert_eq!(
            params,
            vec![
                ("additional", "detail,transfer,file".to_string()),
                ("offset", "0".to_string()),
                ("limit", "-1".to_string()),
            ]
        );
    }

    #[test]
    fn an_explicit_limit_and_offset_are_passed_through() {
        assert_eq!(
            build_list_params(40, Some(20)),
            vec![
                ("additional", "detail,transfer,file".to_string()),
                ("offset", "40".to_string()),
                ("limit", "20".to_string()),
            ]
        );
    }

    #[test]
    fn the_additional_set_is_exactly_what_the_model_needs() {
        assert_eq!(LIST_ADDITIONAL, ["detail", "transfer", "file"]);
    }

    #[test]
    fn the_supported_version_is_pinned_to_v1() {
        // The v1 string statuses are what `model::TaskStatus` parses; v2/v3
        // return numeric codes and a different `additional` shape.
        assert_eq!(DS_TASK_SUPPORTED, (1, 1));
    }

    // ---- id-taking methods -------------------------------------------------

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn ids_are_encoded_as_one_comma_separated_string() {
        // Download Station v1's encoding. File Station spells the same list as
        // a JSON array; see `file_station`'s cross-API test.
        assert_eq!(
            build_ds_id_params(&ids(&["dbid_001", "dbid_002", "dbid_003"])),
            vec![("id", "dbid_001,dbid_002,dbid_003".to_string())]
        );
    }

    #[test]
    fn a_single_id_carries_no_separator() {
        assert_eq!(
            build_ds_id_params(&ids(&["dbid_001"])),
            vec![("id", "dbid_001".to_string())]
        );
    }

    #[test]
    fn delete_never_asks_to_force_complete() {
        // `force_complete=true` marks an unfinished task complete and keeps
        // what it downloaded — the opposite of this program's job.
        let params = build_delete_params(&ids(&["dbid_001", "dbid_002"]));
        assert_eq!(
            params,
            vec![
                ("id", "dbid_001,dbid_002".to_string()),
                ("force_complete", "false".to_string()),
            ]
        );
    }

    #[test]
    fn getinfo_asks_for_the_same_additional_blocks_the_list_does() {
        // Otherwise a task fetched to confirm a pause would parse with a
        // different shape than the same task from the poller.
        assert_eq!(
            build_getinfo_params(&ids(&["dbid_001"])),
            vec![
                ("id", "dbid_001".to_string()),
                ("additional", "detail,transfer,file".to_string()),
            ]
        );
    }

    // ---- per-task result array ---------------------------------------------

    fn results(body: &str) -> Vec<TaskOpResult> {
        parse_envelope(body, DS_TASK_API).expect("a per-task result array")
    }

    #[test]
    fn a_successful_delete_reports_zero_for_every_task() {
        let parsed = results(
            r#"{"success": true, "data": [
                {"id": "dbid_001", "error": 0},
                {"id": "dbid_002", "error": 0}
            ]}"#,
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "dbid_001");
        check_task_results(&parsed).expect("all zero is success");
    }

    #[test]
    fn a_per_task_error_is_a_failure_even_though_the_envelope_succeeded() {
        // The trap this whole result array exists to avoid: `success: true`
        // with a task that was not deleted.
        let parsed = results(
            r#"{"success": true, "data": [
                {"id": "dbid_001", "error": 0},
                {"id": "dbid_002", "error": 544}
            ]}"#,
        );
        let err = check_task_results(&parsed).expect_err("544 is a failure");
        assert!(
            matches!(err, crate::error::Error::Dsm { code: 544, ref api } if api == DS_TASK_API),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_result_array_says_nothing_about_the_task_that_was_asked_about() {
        // `check_task_results` over an empty slice is vacuously fine — there is
        // no non-zero code in it. That is exactly why a caller acting on one id
        // must not use it: for a *delete*, "success" here means the files have
        // already gone and the task that still points at them is reported as
        // removed.
        let parsed = results(r#"{"success": true, "data": []}"#);
        assert!(parsed.is_empty());
        check_task_results(&parsed).expect("no non-zero code to find");

        let err = check_task_result("dbid_001", &parsed).expect_err("nothing was reported");
        assert!(err.to_string().contains("no result"), "{err}");
    }

    #[test]
    fn a_result_array_naming_other_tasks_is_a_failure_for_the_one_requested() {
        // The same trap one step subtler: DSM answers about a task that was not
        // asked about. Scanning for "any non-zero code" would pass.
        let parsed = results(r#"{"success": true, "data": [{"id": "dbid_999", "error": 0}]}"#);
        let err = check_task_result("dbid_001", &parsed).expect_err("wrong task");
        assert!(err.to_string().contains("no result"), "{err}");
    }

    #[test]
    fn the_matching_entry_is_the_one_that_decides() {
        let parsed = results(
            r#"{"success": true, "data": [
                {"id": "dbid_001", "error": 0},
                {"id": "dbid_002", "error": 544}
            ]}"#,
        );
        check_task_result("dbid_001", &parsed).expect("this one succeeded");
        let err = check_task_result("dbid_002", &parsed).expect_err("this one did not");
        assert!(
            matches!(err, crate::error::Error::Dsm { code: 544, .. }),
            "{err:?}"
        );
    }
}
