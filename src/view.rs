//! Sort, filter and search — the pure layer between [`Task`] and the table.
//!
//! Nothing here owns or mutates task data. [`visible_indices`] answers "which
//! rows, in what order" with a `Vec<usize>` of indices into the caller's slice,
//! so the task list is never cloned or reordered and the cursor/selection
//! reconciliation in `app.rs` keeps working against stable positions.
//!
//! Three rules the tests pin down:
//!
//! * **Filter, then search, then sort.** The search box narrows what the filter
//!   already allowed, never the other way round.
//! * **The sort is stable, in both directions.** Descending reverses the
//!   *comparator*, not the result vector, so tasks that tie on the sort key keep
//!   their original relative order whichever way the column points. Reversing
//!   the vector would make ties shuffle every time the user presses `S`, which
//!   reads as data corruption even though nothing changed.
//! * **Comparisons never panic.** `f64` keys use
//!   [`total_cmp`](f64::total_cmp) rather than `partial_cmp().unwrap()`; the
//!   derived values are guarded upstream but a `NaN` must never take the
//!   process down mid-frame.

use std::cmp::Ordering;

use crate::model::{Task, TaskStatus};

/// The column a sort is keyed on. Cycled by `s`, in this declaration order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortKey {
    #[default]
    Name,
    Status,
    Size,
    Progress,
    DownSpeed,
    UpSpeed,
    Ratio,
    /// Creation time, as reported by `additional.detail.create_time`.
    Added,
}

impl SortKey {
    /// Every key, in cycle order.
    pub const ALL: [SortKey; 8] = [
        SortKey::Name,
        SortKey::Status,
        SortKey::Size,
        SortKey::Progress,
        SortKey::DownSpeed,
        SortKey::UpSpeed,
        SortKey::Ratio,
        SortKey::Added,
    ];

    /// The table header this key sorts on. Matches the column titles so the
    /// status bar and the header cannot drift apart.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Status => "Status",
            SortKey::Size => "Size",
            SortKey::Progress => "Progress",
            SortKey::DownSpeed => "↓ Speed",
            SortKey::UpSpeed => "↑ Speed",
            SortKey::Ratio => "Ratio",
            SortKey::Added => "Added",
        }
    }

    /// The next key in [`Self::ALL`], wrapping.
    pub fn next(self) -> SortKey {
        cycle(&SortKey::ALL, self)
    }

    /// Compare two tasks on this key, **ascending**.
    ///
    /// Returning [`Ordering::Equal`] is how stability becomes visible: a stable
    /// sort leaves tied rows in their incoming order, which is the DSM list
    /// order.
    pub fn compare(self, a: &Task, b: &Task) -> Ordering {
        match self {
            // Case-insensitive, and without allocating a lowercased copy per
            // comparison — this runs O(n log n) times on every re-sort.
            SortKey::Name => a
                .title
                .chars()
                .flat_map(char::to_lowercase)
                .cmp(b.title.chars().flat_map(char::to_lowercase)),
            // `TaskStatus` derives `Ord` over its declaration order precisely
            // for this: the DSM statuses read as a lifecycle, so that order is
            // more useful than alphabetical.
            SortKey::Status => a.status.cmp(&b.status),
            SortKey::Size => a.size.cmp(&b.size),
            SortKey::Progress => a.progress().total_cmp(&b.progress()),
            SortKey::DownSpeed => a.download_speed.cmp(&b.download_speed),
            SortKey::UpSpeed => a.upload_speed.cmp(&b.upload_speed),
            SortKey::Ratio => a.ratio().total_cmp(&b.ratio()),
            // `None` sorts before `Some` (the derived `Option` ordering), so a
            // task DSM gave no timestamp for leads the ascending list rather
            // than being silently treated as brand new.
            SortKey::Added => a.create_time.cmp(&b.create_time),
        }
    }
}

/// The element after `current` in `all`, wrapping round at the end.
///
/// A free function rather than a trait: `s` and `f` cycle two unrelated enums
/// with the same three lines, and two copies could drift into disagreeing about
/// whether the cycle wraps. A value not in `all` restarts the cycle, which is
/// the only sensible answer and keeps this total.
fn cycle<T: PartialEq + Copy>(all: &[T], current: T) -> T {
    if all.is_empty() {
        return current;
    }
    let position = all.iter().position(|item| *item == current).unwrap_or(0);
    all[(position + 1) % all.len()]
}

/// Which way a [`SortKey`] points. Toggled by `S`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

impl SortDir {
    /// The header marker for this direction. One cell wide.
    pub fn arrow(self) -> &'static str {
        match self {
            SortDir::Asc => "▲",
            SortDir::Desc => "▼",
        }
    }

    pub fn toggled(self) -> SortDir {
        match self {
            SortDir::Asc => SortDir::Desc,
            SortDir::Desc => SortDir::Asc,
        }
    }

    /// Apply this direction to an ascending comparison.
    fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        }
    }
}

/// The status filter, cycled by `f`.
///
/// [`StatusFilter::Downloading`] deliberately means **in progress**, not the
/// single `downloading` status: `waiting`, `finishing`, `hash_checking`,
/// `extracting` and `filehosting_waiting` are all states a download passes
/// through on its way to finished, and a user filtering for "downloading"
/// wants to see them. The alternative — one filter per DSM status — would
/// need ten of them and still not be what anyone asked for.
///
/// A task whose status this client does not recognize
/// ([`TaskStatus::Unknown`]) is only visible under [`StatusFilter::All`]. It
/// cannot be classified without guessing, and guessing it into `Error` would
/// libel a perfectly healthy task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StatusFilter {
    #[default]
    All,
    Downloading,
    Seeding,
    Finished,
    Paused,
    Error,
}

impl StatusFilter {
    /// Every filter, in cycle order.
    pub const ALL: [StatusFilter; 6] = [
        StatusFilter::All,
        StatusFilter::Downloading,
        StatusFilter::Seeding,
        StatusFilter::Finished,
        StatusFilter::Paused,
        StatusFilter::Error,
    ];

    /// Name for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::Downloading => "Downloading",
            StatusFilter::Seeding => "Seeding",
            StatusFilter::Finished => "Finished",
            StatusFilter::Paused => "Paused",
            StatusFilter::Error => "Error",
        }
    }

    /// The next filter in [`Self::ALL`], wrapping.
    pub fn next(self) -> StatusFilter {
        cycle(&StatusFilter::ALL, self)
    }

    /// Whether a task in `status` passes this filter.
    pub fn matches(self, status: &TaskStatus) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Downloading => matches!(
                status,
                TaskStatus::Waiting
                    | TaskStatus::Downloading
                    | TaskStatus::Finishing
                    | TaskStatus::HashChecking
                    | TaskStatus::Extracting
                    | TaskStatus::FilehostingWaiting
            ),
            StatusFilter::Seeding => matches!(status, TaskStatus::Seeding),
            StatusFilter::Finished => matches!(status, TaskStatus::Finished),
            StatusFilter::Paused => matches!(status, TaskStatus::Paused),
            StatusFilter::Error => matches!(status, TaskStatus::Error),
        }
    }
}

/// How the task list is currently being presented.
///
/// Pure display state: it holds no tasks, so it survives a refresh untouched
/// and can be reasoned about (and tested) without one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub sort_key: SortKey,
    pub sort_dir: SortDir,
    pub filter: StatusFilter,
    /// Case-insensitive substring matched against task titles. Empty means
    /// "no search", not "match nothing".
    pub search: String,
}

impl View {
    /// Advance to the next sort column (`s`).
    ///
    /// The direction is left alone: a user stepping across columns looking for
    /// the biggest task does not want the sort flipping under them.
    pub fn cycle_sort(&mut self) {
        self.sort_key = self.sort_key.next();
    }

    /// Reverse the sort direction (`S`).
    pub fn toggle_dir(&mut self) {
        self.sort_dir = self.sort_dir.toggled();
    }

    /// Advance to the next status filter (`f`).
    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
    }

    /// Whether anything is currently hiding rows, for the empty-state message
    /// in Task 17 — "no tasks" and "nothing matches your filter" are different
    /// things to tell the user.
    pub fn is_narrowed(&self) -> bool {
        self.filter != StatusFilter::All || !self.search.is_empty()
    }
}

/// Indices into `tasks` of the rows to display, in display order.
///
/// Filter → search → stable sort. The returned indices are always valid
/// positions in `tasks`, and every index appears at most once.
pub fn visible_indices(tasks: &[Task], view: &View) -> Vec<usize> {
    let needle: String = view.search.chars().flat_map(char::to_lowercase).collect();

    let mut indices: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| {
            view.filter.matches(&task.status) && title_matches(&task.title, &needle)
        })
        .map(|(index, _)| index)
        .collect();

    // `sort_by` is stable, and the direction is applied to the comparison
    // rather than to the result, so ties hold their DSM order either way.
    indices.sort_by(|&a, &b| {
        view.sort_dir
            .apply(view.sort_key.compare(&tasks[a], &tasks[b]))
    });

    indices
}

/// Case-insensitive substring test. `needle` must already be lowercased.
///
/// An empty needle matches everything — the search box being empty is the
/// normal state, not a filter that hides the whole table.
fn title_matches(title: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let title: String = title.chars().flat_map(char::to_lowercase).collect();
    title.contains(needle)
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

    /// A view sorted on `key` in `dir`, everything visible.
    fn sorted(key: SortKey, dir: SortDir) -> View {
        View {
            sort_key: key,
            sort_dir: dir,
            ..View::default()
        }
    }

    /// The task IDs a view shows, in display order.
    fn ids(tasks: &[Task], view: &View) -> Vec<String> {
        visible_indices(tasks, view)
            .into_iter()
            .map(|i| tasks[i].id.clone())
            .collect()
    }

    /// A field of every visible task, in display order.
    fn field<T>(tasks: &[Task], view: &View, get: impl Fn(&Task) -> T) -> Vec<T> {
        visible_indices(tasks, view)
            .into_iter()
            .map(|i| get(&tasks[i]))
            .collect()
    }

    /// Both directions of one sort key, as an extracted comparable field.
    fn both_ways<T: PartialOrd>(key: SortKey, get: impl Fn(&Task) -> T + Copy) -> (Vec<T>, Vec<T>) {
        let tasks = fixture_tasks();
        (
            field(&tasks, &sorted(key, SortDir::Asc), get),
            field(&tasks, &sorted(key, SortDir::Desc), get),
        )
    }

    fn is_ascending<T: PartialOrd>(values: &[T]) -> bool {
        values.windows(2).all(|w| w[0] <= w[1])
    }

    fn is_descending<T: PartialOrd>(values: &[T]) -> bool {
        values.windows(2).all(|w| w[0] >= w[1])
    }

    /// A minimal task, for the cases the fixture cannot express.
    fn task(id: &str, title: &str, status: TaskStatus, size: u64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            size,
            downloaded: 0,
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

    // ---- defaults and cycling ---------------------------------------------

    #[test]
    fn the_default_view_shows_everything_by_name_ascending() {
        let view = View::default();
        assert_eq!(view.sort_key, SortKey::Name);
        assert_eq!(view.sort_dir, SortDir::Asc);
        assert_eq!(view.filter, StatusFilter::All);
        assert!(view.search.is_empty());
        assert!(!view.is_narrowed());
        assert_eq!(visible_indices(&fixture_tasks(), &view).len(), 14);
    }

    #[test]
    fn cycling_the_sort_key_visits_every_column_and_wraps() {
        let mut view = View::default();
        let mut seen = vec![view.sort_key];
        for _ in 1..SortKey::ALL.len() {
            view.cycle_sort();
            seen.push(view.sort_key);
        }
        assert_eq!(seen, SortKey::ALL.to_vec());

        view.cycle_sort();
        assert_eq!(view.sort_key, SortKey::Name, "the cycle must wrap");
        // Stepping across columns must not disturb the direction.
        assert_eq!(view.sort_dir, SortDir::Asc);
    }

    #[test]
    fn cycling_the_filter_visits_every_filter_and_wraps() {
        let mut view = View::default();
        let mut seen = vec![view.filter];
        for _ in 1..StatusFilter::ALL.len() {
            view.cycle_filter();
            seen.push(view.filter);
        }
        assert_eq!(seen, StatusFilter::ALL.to_vec());

        view.cycle_filter();
        assert_eq!(view.filter, StatusFilter::All, "the cycle must wrap");
    }

    #[test]
    fn toggling_the_direction_twice_returns_to_the_start() {
        let mut view = View::default();
        view.toggle_dir();
        assert_eq!(view.sort_dir, SortDir::Desc);
        view.toggle_dir();
        assert_eq!(view.sort_dir, SortDir::Asc);
        assert_ne!(SortDir::Asc.arrow(), SortDir::Desc.arrow());
    }

    #[test]
    fn a_filter_or_a_search_marks_the_view_as_narrowed() {
        let mut view = View::default();
        view.cycle_filter();
        assert!(view.is_narrowed());

        let searching = View {
            search: "x".to_string(),
            ..View::default()
        };
        assert!(searching.is_narrowed());
    }

    // ---- one test per sort key, both directions ---------------------------

    #[test]
    fn sorting_by_name_is_case_insensitive_in_both_directions() {
        // The fixture titles are all distinct in case, so the case-folding
        // itself needs a hand-built list: a byte-wise sort would put every
        // capital ahead of every lowercase letter.
        let tasks = vec![
            task("a", "beta", TaskStatus::Paused, 1),
            task("b", "Alpha", TaskStatus::Paused, 1),
            task("c", "Zulu", TaskStatus::Paused, 1),
            task("d", "gamma", TaskStatus::Paused, 1),
        ];
        assert_eq!(
            ids(&tasks, &sorted(SortKey::Name, SortDir::Asc)),
            ["b", "a", "d", "c"]
        );
        assert_eq!(
            ids(&tasks, &sorted(SortKey::Name, SortDir::Desc)),
            ["c", "d", "a", "b"]
        );

        // And over the real fixture: lowercased titles come out ordered.
        let (asc, desc) = both_ways(SortKey::Name, |t| t.title.to_lowercase());
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
    }

    #[test]
    fn sorting_by_status_follows_the_dsm_lifecycle_order() {
        let (asc, desc) = both_ways(SortKey::Status, |t| t.status.clone());
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        // `TaskStatus` derives `Ord` over declaration order, which starts at
        // `Waiting` and ends past `Error` at `Unknown`.
        assert_eq!(asc.first(), Some(&TaskStatus::Waiting));
        assert_eq!(
            asc.last(),
            Some(&TaskStatus::Unknown("captcha_needed".into()))
        );
        assert_eq!(desc.first(), asc.last());
    }

    #[test]
    fn sorting_by_size_puts_the_zero_size_task_at_one_end() {
        let (asc, desc) = both_ways(SortKey::Size, |t| t.size);
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        assert_eq!(asc.first(), Some(&0), "the zero-size task leads ascending");
        assert_eq!(desc.first(), asc.last());
        assert_eq!(desc.last(), asc.first());
    }

    #[test]
    fn sorting_by_progress_orders_the_completion_fraction() {
        let (asc, desc) = both_ways(SortKey::Progress, |t| t.progress());
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        assert_eq!(desc.first(), Some(&1.0), "finished tasks lead descending");
    }

    #[test]
    fn sorting_by_download_speed_orders_the_active_transfers() {
        let (asc, desc) = both_ways(SortKey::DownSpeed, |t| t.download_speed);
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        // dbid_001 is the fastest downloader in the fixture.
        let tasks = fixture_tasks();
        let fastest = ids(&tasks, &sorted(SortKey::DownSpeed, SortDir::Desc));
        assert_eq!(fastest.first().map(String::as_str), Some("dbid_001"));
    }

    #[test]
    fn sorting_by_upload_speed_orders_the_seeders() {
        let (asc, desc) = both_ways(SortKey::UpSpeed, |t| t.upload_speed);
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        let tasks = fixture_tasks();
        let fastest = ids(&tasks, &sorted(SortKey::UpSpeed, SortDir::Desc));
        assert_eq!(fastest.first().map(String::as_str), Some("dbid_002"));
    }

    #[test]
    fn sorting_by_ratio_orders_upload_over_download() {
        let (asc, desc) = both_ways(SortKey::Ratio, |t| t.ratio());
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        // dbid_002 seeded 4137684173 of 1932735283 — the best ratio present.
        let tasks = fixture_tasks();
        let best = ids(&tasks, &sorted(SortKey::Ratio, SortDir::Desc));
        assert_eq!(best.first().map(String::as_str), Some("dbid_002"));
        // Tasks that downloaded nothing report 0.0 rather than an infinity.
        assert_eq!(asc.first(), Some(&0.0));
    }

    #[test]
    fn sorting_by_added_puts_tasks_with_no_timestamp_first_ascending() {
        let (asc, desc) = both_ways(SortKey::Added, |t| t.create_time);
        assert!(is_ascending(&asc), "{asc:?}");
        assert!(is_descending(&desc), "{desc:?}");
        // dbid_010 (no `additional`) and dbid_011 (no `detail`) have none.
        let tasks = fixture_tasks();
        let oldest_first = ids(&tasks, &sorted(SortKey::Added, SortDir::Asc));
        assert_eq!(&oldest_first[..2], ["dbid_010", "dbid_011"]);
        assert_eq!(asc[0], None);
    }

    // ---- stability ---------------------------------------------------------

    #[test]
    fn tied_rows_keep_their_original_order_in_both_directions() {
        // The fixture holds two `seeding` tasks (002 before 013), two
        // `finished` (003 before 014) and two `waiting` (007 before 012). A
        // sort keyed on status ties each pair, so their relative order is
        // entirely down to the sort being stable — and it must hold when the
        // direction flips, which is what proves the comparator is reversed
        // rather than the result vector.
        let tasks = fixture_tasks();
        for dir in [SortDir::Asc, SortDir::Desc] {
            let order = ids(&tasks, &sorted(SortKey::Status, dir));
            let position = |id: &str| order.iter().position(|x| x == id).expect(id);
            for (first, second) in [
                ("dbid_002", "dbid_013"),
                ("dbid_003", "dbid_014"),
                ("dbid_007", "dbid_012"),
            ] {
                assert!(
                    position(first) < position(second),
                    "{first} must stay before {second} sorting {dir:?}: {order:?}"
                );
            }
        }
    }

    #[test]
    fn a_reversed_sort_is_not_merely_the_ascending_list_backwards() {
        // The distinction only shows up with ties, and it is the whole reason
        // `SortDir::apply` reverses the `Ordering` instead of the `Vec`.
        let tasks = fixture_tasks();
        let mut asc = ids(&tasks, &sorted(SortKey::Status, SortDir::Asc));
        let desc = ids(&tasks, &sorted(SortKey::Status, SortDir::Desc));
        asc.reverse();
        assert_ne!(asc, desc, "reversing the vector would shuffle tied rows");
    }

    #[test]
    fn every_sort_key_preserves_input_order_when_all_rows_tie() {
        // Four tasks identical on every sort key but their IDs: whichever
        // column is chosen, and whichever way it points, the order is the
        // order they arrived in.
        let tasks: Vec<Task> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| task(id, "same title", TaskStatus::Seeding, 100))
            .collect();
        for key in SortKey::ALL {
            for dir in [SortDir::Asc, SortDir::Desc] {
                assert_eq!(
                    ids(&tasks, &sorted(key, dir)),
                    ["a", "b", "c", "d"],
                    "{key:?} {dir:?}"
                );
            }
        }
    }

    // ---- filtering ---------------------------------------------------------

    #[test]
    fn the_all_filter_hides_nothing() {
        let tasks = fixture_tasks();
        let view = View::default();
        assert_eq!(visible_indices(&tasks, &view).len(), tasks.len());
    }

    #[test]
    fn the_downloading_filter_covers_every_in_progress_status() {
        let tasks = fixture_tasks();
        let view = View {
            filter: StatusFilter::Downloading,
            ..View::default()
        };
        let mut shown = ids(&tasks, &view);
        shown.sort();
        assert_eq!(
            shown,
            [
                "dbid_001", // downloading
                "dbid_006", // extracting
                "dbid_007", // waiting
                "dbid_008", // finishing
                "dbid_009", // hash_checking
                "dbid_010", // filehosting_waiting
                "dbid_012", // waiting
            ]
        );
    }

    #[test]
    fn the_single_status_filters_show_exactly_their_status() {
        let tasks = fixture_tasks();
        for (filter, expected) in [
            (StatusFilter::Seeding, vec!["dbid_002", "dbid_013"]),
            (StatusFilter::Finished, vec!["dbid_003", "dbid_014"]),
            (StatusFilter::Paused, vec!["dbid_004"]),
            (StatusFilter::Error, vec!["dbid_005"]),
        ] {
            let view = View {
                filter,
                ..View::default()
            };
            let mut shown = ids(&tasks, &view);
            shown.sort();
            assert_eq!(shown, expected, "{filter:?}");
        }
    }

    #[test]
    fn an_unrecognized_status_is_only_visible_under_the_all_filter() {
        // Deliberate: it cannot be classified without guessing, and filing it
        // under Error would libel a task that may be perfectly healthy.
        let tasks = fixture_tasks();
        for filter in StatusFilter::ALL {
            let view = View {
                filter,
                ..View::default()
            };
            let visible = ids(&tasks, &view).contains(&"dbid_011".to_string());
            assert_eq!(visible, filter == StatusFilter::All, "{filter:?}");
        }
    }

    #[test]
    fn the_filters_between_them_account_for_every_fixture_task() {
        // Nothing is silently unreachable except the documented unknown case.
        let tasks = fixture_tasks();
        let mut covered: Vec<String> = StatusFilter::ALL
            .iter()
            .filter(|f| **f != StatusFilter::All)
            .flat_map(|filter| {
                ids(
                    &tasks,
                    &View {
                        filter: *filter,
                        ..View::default()
                    },
                )
            })
            .collect();
        covered.sort();
        let before = covered.len();
        covered.dedup();
        assert_eq!(before, covered.len(), "a task matched two filters");
        assert_eq!(covered.len(), tasks.len() - 1, "only dbid_011 is uncovered");
        assert!(!covered.contains(&"dbid_011".to_string()));
    }

    // ---- search ------------------------------------------------------------

    fn searching(query: &str) -> View {
        View {
            search: query.to_string(),
            ..View::default()
        }
    }

    #[test]
    fn an_empty_search_returns_everything() {
        let tasks = fixture_tasks();
        assert_eq!(visible_indices(&tasks, &searching("")).len(), tasks.len());
    }

    #[test]
    fn search_is_a_case_insensitive_substring_of_the_title() {
        let tasks = fixture_tasks();
        for query in ["ubuntu", "UBUNTU", "Ubuntu", "uBuNtU"] {
            assert_eq!(ids(&tasks, &searching(query)), ["dbid_001"], "{query}");
        }
        // A substring from the middle of a title, not just a prefix.
        assert_eq!(ids(&tasks, &searching("desktop")), ["dbid_001"]);
    }

    #[test]
    fn search_matches_cjk_and_emoji_titles() {
        let tasks = fixture_tasks();
        assert_eq!(ids(&tasks, &searching("神隠し")), ["dbid_006"]);
        assert_eq!(ids(&tasks, &searching("🐰")), ["dbid_002"]);
    }

    #[test]
    fn a_search_matching_nothing_returns_an_empty_list() {
        let tasks = fixture_tasks();
        assert!(ids(&tasks, &searching("no-such-title-anywhere")).is_empty());
    }

    #[test]
    fn a_search_matching_several_titles_returns_all_of_them() {
        let tasks = fixture_tasks();
        let mut shown = ids(&tasks, &searching("1080p"));
        shown.sort();
        assert_eq!(shown, ["dbid_002", "dbid_006", "dbid_009"]);
        // dbid_005 is `720p` — a substring search must not match it.
        assert!(!shown.contains(&"dbid_005".to_string()));
    }

    #[test]
    fn search_narrows_what_the_filter_already_allowed() {
        let tasks = fixture_tasks();
        let view = View {
            filter: StatusFilter::Seeding,
            search: "1080p".to_string(),
            ..View::default()
        };
        // dbid_002 is seeding and matches; dbid_006 matches but is extracting;
        // dbid_013 is seeding but does not match.
        assert_eq!(ids(&tasks, &view), ["dbid_002"]);
    }

    #[test]
    fn filter_and_search_do_not_disturb_the_sort() {
        let tasks = fixture_tasks();
        let view = View {
            sort_key: SortKey::Size,
            sort_dir: SortDir::Desc,
            search: "1080p".to_string(),
            ..View::default()
        };
        let sizes = field(&tasks, &view, |t| t.size);
        assert!(is_descending(&sizes), "{sizes:?}");
        assert_eq!(sizes.len(), 3);
    }

    // ---- degenerate input --------------------------------------------------

    #[test]
    fn an_empty_task_list_is_empty_under_every_view() {
        for key in SortKey::ALL {
            for filter in StatusFilter::ALL {
                let view = View {
                    sort_key: key,
                    filter,
                    search: "anything".to_string(),
                    ..View::default()
                };
                assert!(visible_indices(&[], &view).is_empty());
            }
        }
    }

    #[test]
    fn the_returned_indices_are_unique_and_in_range() {
        let tasks = fixture_tasks();
        for key in SortKey::ALL {
            for dir in [SortDir::Asc, SortDir::Desc] {
                let mut indices = visible_indices(&tasks, &sorted(key, dir));
                assert_eq!(indices.len(), tasks.len());
                assert!(indices.iter().all(|i| *i < tasks.len()));
                indices.sort_unstable();
                indices.dedup();
                assert_eq!(indices.len(), tasks.len(), "{key:?} {dir:?}");
            }
        }
    }
}
