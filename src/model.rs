//! The task model, and the DSM JSON → [`Task`] mapping.
//!
//! This is the v1 `SYNO.DownloadStation.Task` `list` shape, requested with
//! `additional=detail,transfer,file`. The wire format is deliberately kept in
//! private `Raw*` structs and collapsed into a flat [`Task`] by `From`, so the
//! rest of the program never has to reach through `additional.transfer.…` or
//! reason about which sub-block might be missing.
//!
//! Three robustness rules, all of which the tests pin down:
//!
//! * **An unrecognized status never drops a row.** [`TaskStatus::Unknown`]
//!   keeps the raw string, so a DSM build that invents a state still renders.
//! * **Every `additional` sub-block is optional.** A task listed without
//!   `additional`, or with only some of `detail` / `transfer` / `file`, parses
//!   into a `Task` with zeroed counters rather than failing the whole list —
//!   one odd task must not blank the table.
//! * **Numbers may arrive as JSON numbers or as strings.** DSM is inconsistent
//!   about this between fields, versions and builds (file sizes and timestamps
//!   are the usual offenders), so every numeric field goes through the
//!   permissive deserializers below.
//!
//! Note that `Task` carries no divide-by-zero hazards: [`Task::progress`],
//! [`Task::ratio`] and [`Task::eta`] all guard their denominators, because a
//! zero-size task is a perfectly ordinary thing for Download Station to hold.

use serde::{Deserialize, Deserializer};

/// The v1 status strings, in the order DSM documents them.
///
/// [`TaskStatus::Unknown`] is the catch-all: DSM adds states between builds and
/// a task the client cannot name is still a task the user may want to delete.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(from = "String")]
pub enum TaskStatus {
    Waiting,
    Downloading,
    Paused,
    Finishing,
    Finished,
    HashChecking,
    Seeding,
    FilehostingWaiting,
    Extracting,
    Error,
    /// A status string this client does not recognize, kept verbatim.
    Unknown(String),
}

impl TaskStatus {
    /// Map a DSM status string onto a variant.
    ///
    /// Case and surrounding whitespace are ignored: the exact casing is not
    /// something worth failing over, and lowercase is only a convention.
    pub fn from_dsm_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "waiting" => TaskStatus::Waiting,
            "downloading" => TaskStatus::Downloading,
            "paused" => TaskStatus::Paused,
            "finishing" => TaskStatus::Finishing,
            "finished" => TaskStatus::Finished,
            "hash_checking" => TaskStatus::HashChecking,
            "seeding" => TaskStatus::Seeding,
            "filehosting_waiting" => TaskStatus::FilehostingWaiting,
            "extracting" => TaskStatus::Extracting,
            "error" => TaskStatus::Error,
            _ => TaskStatus::Unknown(raw.trim().to_string()),
        }
    }

    /// The DSM spelling of this status. Round-trips [`Self::from_dsm_str`].
    pub fn as_dsm_str(&self) -> &str {
        match self {
            TaskStatus::Waiting => "waiting",
            TaskStatus::Downloading => "downloading",
            TaskStatus::Paused => "paused",
            TaskStatus::Finishing => "finishing",
            TaskStatus::Finished => "finished",
            TaskStatus::HashChecking => "hash_checking",
            TaskStatus::Seeding => "seeding",
            TaskStatus::FilehostingWaiting => "filehosting_waiting",
            TaskStatus::Extracting => "extracting",
            TaskStatus::Error => "error",
            TaskStatus::Unknown(raw) => raw,
        }
    }

    /// Every recognized variant, in one place so a test can enumerate the
    /// documented statuses — that a new variant is classified by
    /// `view::StatusFilter`, labelled by `ui::table` and round-trips through
    /// [`TaskStatus::as_dsm_str`] is checked by walking this array. Production code
    /// matches on the variants directly.
    pub const KNOWN: [TaskStatus; 10] = [
        TaskStatus::Waiting,
        TaskStatus::Downloading,
        TaskStatus::Paused,
        TaskStatus::Finishing,
        TaskStatus::Finished,
        TaskStatus::HashChecking,
        TaskStatus::Seeding,
        TaskStatus::FilehostingWaiting,
        TaskStatus::Extracting,
        TaskStatus::Error,
    ];
}

impl Default for TaskStatus {
    /// A task listed with no `status` at all — never seen in practice, but the
    /// alternative is rejecting the whole response over one field.
    fn default() -> Self {
        TaskStatus::Unknown(String::new())
    }
}

impl From<String> for TaskStatus {
    fn from(raw: String) -> Self {
        TaskStatus::from_dsm_str(&raw)
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_dsm_str())
    }
}

/// The v1 `type` field — which protocol Download Station used to fetch the
/// task.
///
/// It is not display data: `delete.rs` reads it to decide whether an **absent
/// file list** is ordinary or anomalous. An HTTP, FTP, NZB or eMule task has no
/// `additional.file` block at all, so its on-disk name can only come from the
/// title; a BitTorrent task always has one, so a BT task that arrives without
/// it is a task this client cannot safely name a directory for. See
/// [`crate::delete::resolve_delete_target`].
///
/// [`TaskType::Unknown`] keeps the raw string, and — like [`TaskStatus`] — a
/// type this client cannot name never drops a row.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(from = "String")]
pub enum TaskType {
    /// BitTorrent. **The one type whose file list is mandatory.**
    BitTorrent,
    Http,
    Https,
    Ftp,
    Ftps,
    Nzb,
    Emule,
    /// A type string this client does not recognize, kept verbatim. Also what
    /// a task listed with no `type` at all becomes (the empty string).
    Unknown(String),
}

impl TaskType {
    /// Map a DSM type string onto a variant. Case and surrounding whitespace
    /// are ignored, exactly as for [`TaskStatus::from_dsm_str`].
    pub fn from_dsm_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "bt" => TaskType::BitTorrent,
            "http" => TaskType::Http,
            "https" => TaskType::Https,
            "ftp" => TaskType::Ftp,
            "ftps" => TaskType::Ftps,
            "nzb" => TaskType::Nzb,
            "emule" => TaskType::Emule,
            _ => TaskType::Unknown(raw.trim().to_string()),
        }
    }

    /// The DSM spelling of this type. Round-trips [`Self::from_dsm_str`].
    pub fn as_dsm_str(&self) -> &str {
        match self {
            TaskType::BitTorrent => "bt",
            TaskType::Http => "http",
            TaskType::Https => "https",
            TaskType::Ftp => "ftp",
            TaskType::Ftps => "ftps",
            TaskType::Nzb => "nzb",
            TaskType::Emule => "emule",
            TaskType::Unknown(raw) => raw,
        }
    }

    /// Whether DSM should always have sent a file list for this task.
    ///
    /// **Only BitTorrent answers yes**, and only BitTorrent is treated as
    /// anomalous when the list is missing: a torrent's `additional.file` block
    /// is the metadata the client downloaded before it wrote anything, so it is
    /// there for every BT task DSM knows about. Every other type — including an
    /// unrecognized one — is left to the title fallback, because refusing a
    /// type this client has simply never heard of would strand tasks over a
    /// string comparison.
    pub fn file_list_is_mandatory(&self) -> bool {
        matches!(self, TaskType::BitTorrent)
    }

    /// Every recognized variant, so a test can walk them rather than trusting
    /// two `match` arms to stay in step.
    pub const KNOWN: [TaskType; 7] = [
        TaskType::BitTorrent,
        TaskType::Http,
        TaskType::Https,
        TaskType::Ftp,
        TaskType::Ftps,
        TaskType::Nzb,
        TaskType::Emule,
    ];
}

impl Default for TaskType {
    /// A task listed with no `type` — the same reasoning as
    /// [`TaskStatus::default`]: one absent field must not reject the response.
    fn default() -> Self {
        TaskType::Unknown(String::new())
    }
}

impl From<String> for TaskType {
    fn from(raw: String) -> Self {
        TaskType::from_dsm_str(&raw)
    }
}

impl std::fmt::Display for TaskType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_dsm_str())
    }
}

/// One entry of `additional.file`.
///
/// The v1 API returns **objects**, not bare filename strings, and `filename`
/// is a path *relative to the task's on-disk root* — which is what makes
/// `delete.rs` able to recover the real directory name even when it differs
/// from the display title.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TaskFile {
    #[serde(default)]
    pub filename: String,
    #[serde(default, deserialize_with = "de_u64")]
    pub size: u64,
    /// DSM priority word (`normal`, `low`, `high`, `skip`).
    #[serde(default)]
    pub priority: String,
    /// Whether this file is part of the download. Absent means yes — a file
    /// DSM bothered to list is downloaded unless it says otherwise.
    ///
    /// ⚠️ The wire name is **`wanted`**. This field was called `selected` and
    /// deserialized from that name until a real DSM 7 capture showed no such key
    /// exists: it had been silently defaulting to `true` on every NAS this
    /// program has ever talked to. Nothing depended on the value — the delete
    /// path reads only `filename` — but the wrong name is the kind of thing that
    /// stays wrong until something is captured, so it is spelled out here.
    ///
    /// A real entry also carries `index` and `size_downloaded`; neither is
    /// modelled, because nothing reads them and this crate drops wire fields it
    /// does not use.
    #[serde(default = "yes", rename = "wanted")]
    pub wanted: bool,
}

/// One Download Station task, flattened out of the DSM wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "RawTask")]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    /// Which protocol fetched this task. Read by the delete path, not by the
    /// table; see [`TaskType`].
    pub task_type: TaskType,
    /// Total size of the task in bytes, as DSM reports it.
    pub size: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    pub download_speed: u64,
    pub upload_speed: u64,
    /// Usually share-relative with no leading slash (`downloads`,
    /// `video/movies`); some configurations surface `/volumeN/share/…`.
    /// `delete.rs` owns the normalization.
    pub destination: String,
    pub files: Vec<TaskFile>,
    pub seeders: u32,
    pub leechers: u32,
    /// Unix seconds, when DSM supplied one.
    pub create_time: Option<i64>,
}

impl Task {
    /// Completion as a fraction in `0.0..=1.0`.
    ///
    /// A zero-size task reports `0.0` rather than dividing by zero, and a task
    /// reporting more downloaded than its own size (BT re-downloads make this
    /// possible) is clamped instead of overflowing a progress gauge.
    pub fn progress(&self) -> f64 {
        if self.size == 0 {
            return 0.0;
        }
        (self.downloaded as f64 / self.size as f64).clamp(0.0, 1.0)
    }

    /// Uploaded over downloaded. Zero when nothing has been downloaded yet —
    /// the alternative is an infinity in the table.
    pub fn ratio(&self) -> f64 {
        if self.downloaded == 0 {
            return 0.0;
        }
        self.uploaded as f64 / self.downloaded as f64
    }

    /// Seconds until completion at the current rate, or `None` when that is
    /// not a meaningful question: nothing is downloading, or nothing is left.
    pub fn eta(&self) -> Option<u64> {
        if self.download_speed == 0 {
            return None;
        }
        let remaining = self.size.checked_sub(self.downloaded)?;
        if remaining == 0 {
            return None;
        }
        Some(remaining.div_ceil(self.download_speed))
    }
}

/// The `data` object of a `list` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct TaskList {
    /// How many tasks exist on the NAS, which may exceed `tasks.len()` when a
    /// `limit` was applied.
    #[serde(default, deserialize_with = "de_u32")]
    pub total: u32,
    #[serde(default, deserialize_with = "de_u32")]
    pub offset: u32,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

// ---------------------------------------------------------------------------
// Wire shape
// ---------------------------------------------------------------------------

/// A task exactly as DSM sends it. Private: nothing outside this module should
/// have to know that `destination` lives two levels down.
#[derive(Debug, Default, Deserialize)]
struct RawTask {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: TaskStatus,
    #[serde(default, rename = "type")]
    task_type: TaskType,
    #[serde(default, deserialize_with = "de_u64")]
    size: u64,
    /// Absent whenever the caller did not ask for `additional`, and absent per
    /// task on some DSM builds.
    #[serde(default)]
    additional: RawAdditional,
}

#[derive(Debug, Default, Deserialize)]
struct RawAdditional {
    #[serde(default)]
    detail: RawDetail,
    #[serde(default)]
    transfer: RawTransfer,
    #[serde(default)]
    file: Vec<TaskFile>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDetail {
    #[serde(default)]
    destination: String,
    #[serde(default, deserialize_with = "de_u32")]
    connected_seeders: u32,
    #[serde(default, deserialize_with = "de_u32")]
    connected_leechers: u32,
    #[serde(default, deserialize_with = "de_i64_opt")]
    create_time: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawTransfer {
    #[serde(default, deserialize_with = "de_u64")]
    size_downloaded: u64,
    #[serde(default, deserialize_with = "de_u64")]
    size_uploaded: u64,
    #[serde(default, deserialize_with = "de_u64")]
    speed_download: u64,
    #[serde(default, deserialize_with = "de_u64")]
    speed_upload: u64,
}

impl From<RawTask> for Task {
    fn from(raw: RawTask) -> Self {
        let RawAdditional {
            detail,
            transfer,
            file,
        } = raw.additional;
        Task {
            id: raw.id,
            title: raw.title,
            status: raw.status,
            task_type: raw.task_type,
            size: raw.size,
            downloaded: transfer.size_downloaded,
            uploaded: transfer.size_uploaded,
            download_speed: transfer.speed_download,
            upload_speed: transfer.speed_upload,
            destination: detail.destination,
            files: file,
            seeders: detail.connected_seeders,
            leechers: detail.connected_leechers,
            create_time: detail.create_time,
        }
    }
}

// ---------------------------------------------------------------------------
// Permissive numeric deserializers
// ---------------------------------------------------------------------------

/// A JSON number or a string holding one. DSM uses both, sometimes for the
/// same field on different versions, so neither form may be an error.
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Int(i64),
    Float(f64),
    Str(String),
}

impl NumOrStr {
    /// Best-effort integer value; an unparseable string is `None`.
    fn to_i64(&self) -> Option<i64> {
        match self {
            NumOrStr::Int(n) => Some(*n),
            NumOrStr::Float(n) => Some(*n as i64),
            NumOrStr::Str(s) => {
                let s = s.trim();
                s.parse::<i64>()
                    .ok()
                    .or_else(|| s.parse::<f64>().ok().map(|n| n as i64))
            }
        }
    }
}

/// Read an optional signed integer, tolerating the string form.
///
/// An absent field, an empty string and a value that is not a number all
/// collapse to `None`: a missing timestamp is normal, not a parse failure.
fn de_i64_opt<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Ok(Option::<NumOrStr>::deserialize(deserializer)?.and_then(|raw| raw.to_i64()))
}

/// Read an unsigned 64-bit count, tolerating the string form. Anything
/// negative or unparseable becomes `0` — byte counts and speeds have no
/// meaningful negative value.
fn de_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    Ok(de_i64_opt(deserializer)?.unwrap_or(0).max(0) as u64)
}

/// As [`de_u64`], saturating into `u32` for peer counts.
fn de_u32<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    Ok(de_u64(deserializer)?.min(u32::MAX as u64) as u32)
}

/// `#[serde(default = ...)]` needs a function, and `true` is not one.
fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;

    /// The checked-in list response: **shape captured from a real DSM 7 NAS,
    /// content synthetic.** Every key name, nesting level and value type is the
    /// real one; the titles and filenames are invented so the file can live in a
    /// public repository.
    ///
    /// That capture is what caught `TaskFile`'s `selected` field, which DSM has
    /// never sent — the real key is `wanted` — and it is why the fixture no
    /// longer carries the invented `detail.priority` or `status_extra`.
    const FIXTURE: &str = include_str!("../tests/fixtures/task_list.json");

    const DS_TASK: &str = "SYNO.DownloadStation.Task";

    fn fixture() -> TaskList {
        parse_envelope(FIXTURE, DS_TASK).expect("the fixture must parse")
    }

    fn task(id: &str) -> Task {
        fixture()
            .tasks
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("fixture has no task {id}"))
    }

    // ---- fixture shape ----------------------------------------------------

    #[test]
    fn the_fixture_still_carries_only_keys_a_real_nas_sends() {
        // The fixture's shape came from a real capture; its content did not.
        // This is the guard against drifting back to invented keys — the
        // previous fixture had three (`selected` on a file, `priority` on the
        // detail block, `status_extra` on the task), and `selected` was
        // deserialized by the model for the whole of the project's life without
        // ever matching a byte DSM sent.
        for invented in ["\"selected\"", "\"status_extra\""] {
            assert!(
                !FIXTURE.contains(invented),
                "{invented} is not a key any real DSM 7 response carries"
            );
        }
        // …and the real names that replaced them are present.
        for real in [
            "\"wanted\"",
            "\"index\"",
            "\"size_downloaded\"",
            "\"seedelapsed\"",
        ] {
            assert!(
                FIXTURE.contains(real),
                "{real} is a real key and should be modelled"
            );
        }
    }

    #[test]
    fn the_whole_fixture_parses_into_a_task_list() {
        let list = fixture();
        assert_eq!(list.offset, 0);
        assert_eq!(list.total, 14);
        assert_eq!(list.tasks.len(), 14);
        assert_eq!(list.total as usize, list.tasks.len());
    }

    #[test]
    fn task_ids_are_unique() {
        // Selection and the cursor are keyed by ID; duplicates would alias.
        let list = fixture();
        let mut ids: Vec<&str> = list.tasks.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate task id in the fixture");
    }

    // ---- field mapping ----------------------------------------------------

    #[test]
    fn every_field_maps_out_of_the_nested_additional_blocks() {
        let task = task("dbid_001");
        assert_eq!(task.title, "Ubuntu.24.04.3.LTS.Desktop.amd64");
        assert_eq!(task.status, TaskStatus::Downloading);
        assert_eq!(task.size, 6_231_819_257);
        assert_eq!(task.downloaded, 2_429_550_592);
        assert_eq!(task.uploaded, 118_325_248);
        assert_eq!(task.download_speed, 8_912_896);
        assert_eq!(task.upload_speed, 524_288);
        assert_eq!(task.destination, "downloads");
        assert_eq!(task.seeders, 12);
        assert_eq!(task.leechers, 4);
        assert_eq!(task.create_time, Some(1_753_960_800));
        assert_eq!(task.files.len(), 3);
    }

    #[test]
    fn file_entries_are_objects_with_a_relative_path() {
        let task = task("dbid_001");
        assert_eq!(
            task.files[0],
            TaskFile {
                filename: "Ubuntu.24.04.3.LTS.Desktop.amd64/ubuntu-24.04.3-desktop-amd64.iso"
                    .to_string(),
                size: 6_231_818_240,
                priority: "normal".to_string(),
                wanted: true,
            }
        );
        // A deselected file is still listed, and says so.
        assert!(!task.files[2].wanted);
        assert_eq!(task.files[2].priority, "low");
    }

    #[test]
    fn a_file_entry_without_selected_defaults_to_selected() {
        let task = task("dbid_011");
        assert_eq!(task.files.len(), 1);
        assert!(task.files[0].wanted);
    }

    #[test]
    fn numeric_fields_parse_from_both_json_numbers_and_strings() {
        // dbid_001 sends file sizes as strings, dbid_002 as numbers.
        assert_eq!(task("dbid_001").files[1].size, 184);
        assert_eq!(task("dbid_002").files[1].size, 35_283);
        // `create_time` is a string of unix seconds on every task that has it.
        assert_eq!(task("dbid_002").create_time, Some(1_750_000_000));
    }

    #[test]
    fn a_cjk_title_survives_parsing_intact() {
        // Also the fixture's display-width case for `format::truncate_ellipsis`.
        let task = task("dbid_006");
        assert_eq!(task.title, "千と千尋の神隠し.2001.1080p.日本語音声");
        assert_eq!(task.status, TaskStatus::Extracting);
        assert_eq!(task.destination, "video/movies");
    }

    #[test]
    fn destinations_cover_flat_nested_and_absolute_forms() {
        assert_eq!(task("dbid_001").destination, "downloads");
        assert_eq!(task("dbid_009").destination, "video/tv");
        assert_eq!(task("dbid_008").destination, "downloads/incoming");
        // `delete.rs` is responsible for stripping the /volumeN prefix; the
        // model reports what DSM said.
        assert_eq!(task("dbid_014").destination, "/volume1/downloads");
    }

    // ---- statuses ---------------------------------------------------------

    #[test]
    fn the_fixture_exercises_every_known_status() {
        let list = fixture();
        for status in TaskStatus::KNOWN {
            assert!(
                list.tasks.iter().any(|t| t.status == status),
                "no fixture task has status {status}"
            );
        }
    }

    #[test]
    fn an_unrecognized_status_is_kept_verbatim_and_does_not_drop_the_row() {
        let task = task("dbid_011");
        assert_eq!(task.status, TaskStatus::Unknown("captcha_needed".into()));
        assert_eq!(task.status.as_dsm_str(), "captcha_needed");
    }

    #[test]
    fn every_status_string_round_trips() {
        for status in TaskStatus::KNOWN {
            assert_eq!(
                TaskStatus::from_dsm_str(status.as_dsm_str()),
                status,
                "{status}"
            );
        }
        let unknown = TaskStatus::Unknown("captcha_needed".into());
        assert_eq!(TaskStatus::from_dsm_str("captcha_needed"), unknown);
        assert_eq!(unknown.to_string(), "captcha_needed");
    }

    // ---- types ------------------------------------------------------------

    #[test]
    fn the_task_type_is_parsed_off_the_wire() {
        // Not decoration: `delete.rs` refuses a BitTorrent task whose file list
        // is missing, and can only tell which those are from this field.
        assert_eq!(task("dbid_001").task_type, TaskType::BitTorrent);
        assert_eq!(task("dbid_007").task_type, TaskType::Http);
        assert_eq!(task("dbid_010").task_type, TaskType::Nzb);
    }

    #[test]
    fn every_type_string_round_trips() {
        for task_type in TaskType::KNOWN {
            assert_eq!(
                TaskType::from_dsm_str(task_type.as_dsm_str()),
                task_type,
                "{task_type}"
            );
        }
    }

    #[test]
    fn an_unrecognized_type_is_kept_verbatim_rather_than_assumed_to_be_a_torrent() {
        // The direction matters: guessing BitTorrent for a type this client has
        // never seen would refuse every such task's delete.
        let invented = TaskType::from_dsm_str("magnet_v3");
        assert_eq!(invented, TaskType::Unknown("magnet_v3".into()));
        assert!(!invented.file_list_is_mandatory());
        assert_eq!(invented.to_string(), "magnet_v3");
    }

    #[test]
    fn a_task_listed_with_no_type_parses_and_is_not_treated_as_a_torrent() {
        let task = task_from(r#"{"id": "x", "title": "t"}"#);
        assert_eq!(task.task_type, TaskType::default());
        assert!(!task.task_type.file_list_is_mandatory());
    }

    #[test]
    fn only_bittorrent_must_have_a_file_list() {
        for task_type in TaskType::KNOWN {
            assert_eq!(
                task_type.file_list_is_mandatory(),
                task_type == TaskType::BitTorrent,
                "{task_type}"
            );
        }
    }

    #[test]
    fn type_parsing_ignores_case_and_surrounding_whitespace() {
        assert_eq!(TaskType::from_dsm_str(" BT "), TaskType::BitTorrent);
        assert_eq!(TaskType::from_dsm_str("NZB"), TaskType::Nzb);
    }

    #[test]
    fn status_parsing_ignores_case_and_surrounding_whitespace() {
        assert_eq!(
            TaskStatus::from_dsm_str("Downloading"),
            TaskStatus::Downloading
        );
        assert_eq!(
            TaskStatus::from_dsm_str(" HASH_CHECKING "),
            TaskStatus::HashChecking
        );
        // The raw text of an unknown status is trimmed but not case-folded.
        assert_eq!(
            TaskStatus::from_dsm_str("  Weird_State "),
            TaskStatus::Unknown("Weird_State".into())
        );
    }

    #[test]
    fn a_task_with_no_status_field_is_unknown_rather_than_a_parse_error() {
        let list: TaskList =
            serde_json::from_str(r#"{"total": 1, "offset": 0, "tasks": [{"id": "x"}]}"#)
                .expect("a task with only an id must still parse");
        assert_eq!(list.tasks[0].status, TaskStatus::default());
        assert!(matches!(list.tasks[0].status, TaskStatus::Unknown(_)));
    }

    // ---- missing / empty additional data ----------------------------------

    #[test]
    fn a_task_with_no_additional_block_parses_with_zeroed_counters() {
        let task = task("dbid_010");
        assert_eq!(task.status, TaskStatus::FilehostingWaiting);
        assert_eq!(task.size, 524_288_000);
        assert_eq!(task.destination, "");
        assert!(task.files.is_empty());
        assert_eq!(task.downloaded, 0);
        assert_eq!(task.uploaded, 0);
        assert_eq!(task.download_speed, 0);
        assert_eq!(task.upload_speed, 0);
        assert_eq!(task.seeders, 0);
        assert_eq!(task.leechers, 0);
        assert_eq!(task.create_time, None);
    }

    #[test]
    fn a_partial_additional_block_only_zeroes_what_is_missing() {
        // dbid_011 has `file` but neither `detail` nor `transfer`.
        let task = task("dbid_011");
        assert_eq!(task.files.len(), 1);
        assert_eq!(task.destination, "");
        assert_eq!(task.downloaded, 0);
        assert_eq!(task.create_time, None);
    }

    #[test]
    fn an_empty_file_list_is_empty_not_absent_data() {
        for id in ["dbid_008", "dbid_012"] {
            assert!(task(id).files.is_empty(), "{id}");
        }
        // A non-BT task simply has no `file` key at all.
        let http = task("dbid_007");
        assert!(http.files.is_empty());
        assert_eq!(http.destination, "downloads");
    }

    // ---- derived values ---------------------------------------------------

    #[test]
    fn progress_is_the_downloaded_fraction() {
        let progress = task("dbid_001").progress();
        assert!((0.38..0.40).contains(&progress), "{progress}");
        assert_eq!(task("dbid_003").progress(), 1.0, "a finished task is 100%");
    }

    #[test]
    fn a_zero_size_task_does_not_divide_by_zero() {
        let task = task("dbid_012");
        assert_eq!(task.size, 0);
        assert_eq!(task.progress(), 0.0);
        assert_eq!(task.ratio(), 0.0);
        assert_eq!(task.eta(), None);
    }

    #[test]
    fn progress_is_clamped_when_downloaded_exceeds_size() {
        let task = Task {
            size: 100,
            downloaded: 250,
            ..task("dbid_003")
        };
        assert_eq!(task.progress(), 1.0);
    }

    #[test]
    fn ratio_is_uploaded_over_downloaded_and_zero_before_any_download() {
        // dbid_002 uploaded 4137684173 of 1932735283 downloaded.
        let ratio = task("dbid_002").ratio();
        assert!((2.13..2.15).contains(&ratio), "{ratio}");
        // dbid_005 errored before downloading anything.
        assert_eq!(task("dbid_005").ratio(), 0.0);
    }

    #[test]
    fn eta_is_remaining_over_speed_and_none_when_stalled() {
        let downloading = task("dbid_001");
        let remaining = downloading.size - downloading.downloaded;
        assert_eq!(
            downloading.eta(),
            Some(remaining.div_ceil(downloading.download_speed))
        );

        // Seeding: nothing left to download.
        assert_eq!(task("dbid_002").eta(), None);
        // Paused: no speed at all.
        assert_eq!(task("dbid_004").eta(), None);
    }

    #[test]
    fn eta_is_none_when_more_was_downloaded_than_the_reported_size() {
        let task = Task {
            size: 100,
            downloaded: 250,
            download_speed: 10,
            ..task("dbid_001")
        };
        assert_eq!(task.eta(), None);
    }

    // ---- envelope integration ---------------------------------------------

    #[test]
    fn a_failed_list_response_is_a_dsm_error_not_an_empty_table() {
        let err =
            parse_envelope::<TaskList>(r#"{"success": false, "error": {"code": 105}}"#, DS_TASK)
                .expect_err("failed list");
        assert!(
            matches!(err, crate::error::Error::Dsm { code: 105, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_task_list_is_valid() {
        let list: TaskList = parse_envelope(
            r#"{"success": true, "data": {"total": 0, "offset": 0, "tasks": []}}"#,
            DS_TASK,
        )
        .expect("empty list");
        assert_eq!(list, TaskList::default());
    }

    // ---- the lenient number readers ----------------------------------------
    //
    // These decide what `size` is, and `size` is the number the confirmation
    // dialog promises the user they will get back. Every collapse below is
    // deliberate — DSM has been seen to send all of these forms — but each one
    // is also a way for a real value to silently become `0`, so they are
    // pinned rather than assumed.

    /// One task parsed from a bare `transfer`/`detail`-less body.
    fn task_from(json: &str) -> Task {
        serde_json::from_str(json).expect("a task")
    }

    #[test]
    fn a_number_sent_as_a_string_is_read_as_the_number() {
        // DSM v1 quotes its 64-bit counts on some builds.
        let task = task_from(r#"{"id": "x", "title": "t", "size": "8589934592"}"#);
        assert_eq!(task.size, 8_589_934_592);
    }

    #[test]
    fn a_negative_size_clamps_to_zero_rather_than_wrapping() {
        // `-1 as u64` would be 18 exabytes in the "to free" line.
        let task = task_from(r#"{"id": "x", "title": "t", "size": -1}"#);
        assert_eq!(task.size, 0);
        let task = task_from(r#"{"id": "x", "title": "t", "size": "-1"}"#);
        assert_eq!(task.size, 0);
    }

    #[test]
    fn an_unparseable_number_collapses_to_zero_instead_of_failing_the_whole_list() {
        // The trade: one junk field must not cost the user their entire table.
        // The cost is that a real size can read as 0, which understates what a
        // delete frees — the safe direction for a number the dialog promises.
        let task = task_from(r#"{"id": "x", "title": "t", "size": "not a number"}"#);
        assert_eq!(task.size, 0);
        let task = task_from(r#"{"id": "x", "title": "t", "size": null}"#);
        assert_eq!(task.size, 0);
    }

    #[test]
    fn a_fractional_number_truncates() {
        let task = task_from(r#"{"id": "x", "title": "t", "size": 1024.9}"#);
        assert_eq!(task.size, 1024);
        let task = task_from(r#"{"id": "x", "title": "t", "size": "2048.5"}"#);
        assert_eq!(task.size, 2048);
    }

    #[test]
    fn a_peer_count_too_large_for_u32_saturates_rather_than_wrapping() {
        let list: TaskList =
            serde_json::from_str(r#"{"total": 99999999999, "tasks": []}"#).expect("a task list");
        assert_eq!(list.total, u32::MAX);
        let list: TaskList =
            serde_json::from_str(r#"{"total": -5, "tasks": []}"#).expect("a task list");
        assert_eq!(list.total, 0);
    }

    #[test]
    fn a_zeroed_size_still_yields_finite_derived_numbers() {
        // The reason the clamps matter beyond the one field: `progress` and
        // `ratio` divide by these.
        let task = task_from(r#"{"id": "x", "title": "t", "size": "-1"}"#);
        assert!(task.progress().is_finite(), "{}", task.progress());
        assert!(task.ratio().is_finite(), "{}", task.ratio());
        assert_eq!(task.eta(), None);
    }
}
