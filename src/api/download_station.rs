//! `SYNO.DownloadStation.Task` — listing tasks.
//!
//! DSM 7 also ships `SYNO.DownloadStation2.Task` (what the web UI drives), but
//! its `list` method is undocumented, returns numeric statuses and a different
//! `additional` shape. This client uses the documented **v1** API, which is
//! still present and supported on DSM 7 and returns the string statuses and
//! object file lists [`crate::model`] is built around — hence
//! [`DS_TASK_SUPPORTED`] being pinned to `(1, 1)` rather than following the
//! NAS up to whatever it advertises. Delete, pause and resume (Tasks 15 and
//! 16) come from the same v1 API, so there is no mixed-API seam.
//!
//! Parameter construction is a pure function ([`build_list_params`]) per the
//! `build_*_params` convention: Download Station encodes list-valued
//! parameters as **comma-separated strings**, while File Station wants JSON
//! arrays, and that difference is worth having in exactly one testable place.

use crate::api::client::{SynoClient, VersionRange};
use crate::error::Result;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
