//! Fixtures and stubs shared by the unit tests. `#[cfg(test)]` only.
//!
//! Both helpers here were copy-pasted into eight and four test modules
//! respectively before they lived in one place, and the copies had already
//! started to drift — seven of them spelled the API name as a string literal
//! beside a `DS_TASK_API` the same file imported for production. The reason to
//! centralize is not the line count: the fixture loader and the offline client
//! both carry *why they are safe* (see below), and an explanation that exists
//! eight times is an explanation that is true in seven places after the next
//! change.

use crate::api::client::{SynoClient, parse_envelope};
use crate::api::download_station::DS_TASK_API;
use crate::config::ResolvedConfig;
use crate::model::{Task, TaskList};

/// The checked-in `SYNO.DownloadStation.Task` `list` response.
///
/// Shape captured from a real DSM 7 NAS, content synthetic; see the note on
/// `model`'s fixture tests.
pub const FIXTURE_JSON: &str = include_str!("../tests/fixtures/task_list.json");

/// Every task in the fixture, in the order DSM listed them.
///
/// Parsed through the real [`parse_envelope`] rather than hand-built, so a
/// change to the wire mapping is felt by every test that leans on the fixture
/// instead of only by `model`'s own.
pub fn fixture_tasks() -> Vec<Task> {
    parse_envelope::<TaskList>(FIXTURE_JSON, DS_TASK_API)
        .expect("the fixture must parse")
        .tasks
}

/// One fixture task by id. Panics naming the id, because a test asking for a
/// task the fixture no longer has is a broken test, not a failing assertion.
pub fn fixture_task(id: &str) -> Task {
    fixture_tasks()
        .into_iter()
        .find(|task| task.id == id)
        .unwrap_or_else(|| panic!("fixture has no task {id}"))
}

/// A minimal task to build a test case on top of with struct-update syntax.
///
/// A completed HTTP download in `downloads` with no file list — the shape that
/// resolves through rule 3 and touches the fewest guards, so a test overriding
/// two fields is exercising those two fields and nothing else. `Task` has no
/// `Default` on purpose: every field is real DSM data, and a silently zeroed one
/// in production would be a parse bug rather than a sensible default.
pub fn bare_task() -> Task {
    Task {
        id: "synthetic".to_string(),
        title: "Some.Release".to_string(),
        status: crate::model::TaskStatus::Finished,
        task_type: crate::model::TaskType::Http,
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

/// A resolved configuration pointing at a host that does not exist.
///
/// `nas.invalid` is in the reserved `.invalid` TLD, so it can never resolve,
/// however the machine running the tests is configured.
pub fn offline_config() -> ResolvedConfig {
    ResolvedConfig {
        host: "nas.invalid".to_string(),
        port: 5001,
        https: true,
        insecure: false,
        username: "tester".to_string(),
        refresh_secs: 3,
        delete_files: true,
        dry_run: true,
        logout: false,
    }
}

/// A client that cannot reach anything.
///
/// Constructing it opens no connection, and — the property the tests actually
/// lean on — its `ApiInfoMap` is **empty** because `discover()` was never
/// called. Every request therefore fails in `endpoint()`, before a socket is
/// opened, so a test asserting `failed: 0` is asserting that no request was
/// even attempted rather than that the network happened to be slow. (The host
/// does not resolve either, but that is the second line of defence, not the
/// mechanism: pre-populating the API map would silently turn these into
/// real-network tests with a 10-second connect timeout.)
pub fn offline_client() -> SynoClient {
    SynoClient::new(&offline_config()).expect("building a client issues no request")
}
