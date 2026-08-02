//! Application state and key handling.
//!
//! [`App`] holds *everything* the program knows; rendering (`crate::ui`) is a
//! pure function of `&App`, and every key press is a `&mut App` transition. No
//! widget reads the network, and no networking code touches a widget — the
//! poller and the op tasks in `event.rs` (Task 11) will hand their results to
//! `App` as plain data.
//!
//! Two conventions set here and relied on by later tasks:
//!
//! * **The task list is never reordered or cloned for display.** What is
//!   visible, and in what order, comes from [`crate::view::visible_indices`] as
//!   a `Vec<usize>` into [`App::tasks`].
//! * **Selection is keyed by task ID, not row index**, so a refresh that
//!   reorders or removes rows can never silently reassign what is selected.
//!   [`App::cursor`] is a position in the *visible* list and is reconciled by
//!   ID in Task 11.

use std::collections::HashSet;
use std::path::Path;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::api::client::parse_envelope;
use crate::api::download_station::DS_TASK_API;
use crate::error::Result;
use crate::model::{Task, TaskList};
use crate::view::{self, View};

/// Rows a `PageUp`/`PageDown` moves before the first frame has been drawn.
///
/// The real page is the height of the table body and is pushed in by the event
/// loop after each draw ([`App::set_page_size`]); this is only what the very
/// first key press uses.
pub const DEFAULT_PAGE_SIZE: usize = 20;

/// What the UI is currently doing, and therefore which keys mean what.
///
/// Only [`Mode::Normal`] is reachable so far: search lands in Task 12, the
/// confirmation modal in Task 14 and the help overlay in Task 17.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Browsing the table.
    #[default]
    Normal,
    /// Typing into the search box.
    Search,
    /// The delete confirmation modal is open. Refreshes are suspended in this
    /// mode so the plan on screen cannot go stale under the user.
    Confirm,
    /// The help overlay is open.
    Help,
}

/// The whole of the application state.
#[derive(Debug, Clone)]
pub struct App {
    /// Every task DSM reported, in the order it reported them. Display order
    /// is a separate question — see [`App::visible`].
    pub tasks: Vec<Task>,
    /// Sort, filter and search state.
    pub view: View,
    /// Cursor position **within the visible list**, not an index into
    /// [`App::tasks`].
    pub cursor: usize,
    /// Task IDs the user has selected. IDs rather than rows, deliberately.
    pub selected: HashSet<String>,
    pub mode: Mode,
    /// One line of feedback shown in the footer: the result of the last
    /// operation, a poll failure, or the startup banner.
    pub status_message: Option<String>,
    /// How far `PageUp`/`PageDown` jump: the height of the table body, as of
    /// the last frame. See [`DEFAULT_PAGE_SIZE`].
    page_size: usize,
    /// Set by `q` / `Ctrl-C`; the event loop owns the actual exit.
    quit: bool,
}

impl Default for App {
    /// Hand-written rather than derived: `page_size` must start at a usable
    /// value, and `0` would make a page jump do nothing until the first draw.
    fn default() -> Self {
        Self {
            tasks: Vec::new(),
            view: View::default(),
            cursor: 0,
            selected: HashSet::new(),
            mode: Mode::Normal,
            status_message: None,
            page_size: DEFAULT_PAGE_SIZE,
            quit: false,
        }
    }
}

impl App {
    /// An app over a task list. `Vec::new()` is the normal startup state — the
    /// poller fills it in on the first tick.
    pub fn new(tasks: Vec<Task>) -> Self {
        Self {
            tasks,
            ..Self::default()
        }
    }

    /// An app over a captured DSM `list` response on disk — the hidden
    /// `--fixture` mode.
    ///
    /// Offline verification hangs off this: the poller does not exist until
    /// Task 11, so the table, multi-select and the sort/filter keys have no
    /// other way to be exercised, and a 500-task file is the cheapest possible
    /// render-performance check.
    pub fn from_fixture(path: &Path) -> Result<Self> {
        Ok(Self::new(read_fixture(path)?))
    }

    /// Replace the footer message.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Indices into [`App::tasks`] of the rows to display, in display order.
    pub fn visible(&self) -> Vec<usize> {
        view::visible_indices(&self.tasks, &self.view)
    }

    /// How many rows the current sort/filter/search leaves on screen.
    pub fn visible_count(&self) -> usize {
        self.visible().len()
    }

    /// The task under the cursor, if any row is visible at all.
    pub fn cursor_task(&self) -> Option<&Task> {
        self.visible()
            .get(self.cursor)
            .map(|&index| &self.tasks[index])
    }

    /// Rows a page jump moves.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    // ---- selection ---------------------------------------------------------
    //
    // The set holds **task IDs**, never row indices. A refresh that reorders
    // the list, or a filter that hides half of it, therefore cannot silently
    // reassign what the user selected — the ID either still names a task or it
    // does not.

    /// Whether `id` is in the selection set.
    pub fn is_selected(&self, id: &str) -> bool {
        self.selected.contains(id)
    }

    /// The selected tasks that still exist, in [`App::tasks`] order.
    ///
    /// Selections are dropped when a refresh removes their task (Task 11), but
    /// the footer must not over-report in the window before that happens, so
    /// both the count and the size sum are derived from the *tasks*, not from
    /// the raw set — they can never disagree with each other.
    pub fn selected_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks
            .iter()
            .filter(|task| self.selected.contains(&task.id))
    }

    /// How many existing tasks are selected.
    pub fn selected_count(&self) -> usize {
        self.selected_tasks().count()
    }

    /// Total size of the selected tasks — the space a delete would free.
    pub fn selected_size(&self) -> u64 {
        self.selected_tasks().map(|task| task.size).sum()
    }

    /// Toggle the row under the cursor (`Space`). A no-op when nothing is
    /// visible.
    pub fn toggle_selection(&mut self) {
        let Some(id) = self.cursor_task().map(|task| task.id.clone()) else {
            return;
        };
        // `remove` reports whether it was there, so this is one lookup, not two.
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    /// Toggle select-all over the **currently visible rows only** (`a`).
    ///
    /// Hidden tasks are never touched in either direction: with a filter or a
    /// search active, `a` must not quietly arm a delete against rows the user
    /// cannot see. Selecting is the default; the whole visible set already
    /// being selected is what turns the key into a deselect.
    pub fn toggle_select_all_visible(&mut self) {
        let ids: Vec<String> = self
            .visible()
            .into_iter()
            .map(|index| self.tasks[index].id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }

        if ids.iter().all(|id| self.selected.contains(id)) {
            for id in &ids {
                self.selected.remove(id);
            }
        } else {
            self.selected.extend(ids);
        }
    }

    /// Drop the whole selection (`Esc`) — including anything currently hidden,
    /// which is the point: `Esc` is the "I am not sure what is armed" key.
    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Tell the app how tall the table body is, so `PageUp`/`PageDown` move by
    /// a screenful. Clamped to at least one row — a zero-row page would make
    /// the key silently dead.
    pub fn set_page_size(&mut self, rows: usize) {
        self.page_size = rows.max(1);
    }

    /// Pull the cursor back inside the visible list.
    ///
    /// Called after anything that can shrink the list — a filter change (Task
    /// 12), a refresh that removed rows (Task 11) — and by every movement, so
    /// [`App::cursor`] is never a position that does not exist.
    pub fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.visible_count().saturating_sub(1));
    }

    /// Move the cursor by `delta` rows, clamped to the ends of the list.
    ///
    /// Deliberately does **not** wrap: holding `j` at the bottom of a long list
    /// jumping back to the top is how the wrong row gets deleted.
    pub fn move_cursor(&mut self, delta: isize) {
        let rows = self.visible_count();
        if rows == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs())
        } else {
            self.cursor.saturating_add(delta.unsigned_abs())
        };
        self.clamp_cursor();
    }

    /// Jump to the first visible row (`Home`, `g`).
    pub fn cursor_to_first(&mut self) {
        self.cursor = 0;
    }

    /// Jump to the last visible row (`End`, `G`).
    pub fn cursor_to_last(&mut self) {
        self.cursor = self.visible_count().saturating_sub(1);
    }

    /// Up one screenful (`PageUp`).
    pub fn page_up(&mut self) {
        self.move_cursor(-page_delta(self.page_size));
    }

    /// Down one screenful (`PageDown`).
    pub fn page_down(&mut self) {
        self.move_cursor(page_delta(self.page_size));
    }

    /// Whether the event loop should stop.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Ask the event loop to stop after this iteration.
    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// Feed one terminal event to the state machine.
    ///
    /// Resize and focus events need no state change: the next `draw` measures
    /// the terminal again, so simply looping is the whole response.
    pub fn handle_event(&mut self, event: Event) {
        if let Event::Key(key) = event {
            self.handle_key(key);
        }
    }

    /// Feed one key press to the state machine.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Terminals that report key *releases* (Windows, and the kitty
        // protocol) would otherwise run every binding twice.
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Ctrl-C is unconditional: it must work from inside a modal too.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit();
            return;
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            // Search input (Task 12), the confirmation modal (Task 14) and the
            // help overlay (Task 17) each own their keys. Until they land,
            // nothing can put the app into these modes; falling back to Normal
            // means a stray mode can never trap the user.
            Mode::Search | Mode::Confirm | Mode::Help => self.mode = Mode::Normal,
        }
    }

    /// Keys while browsing the table. Sort/filter and search land in Task 12,
    /// the operations in Tasks 14-16.
    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.quit(),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Home | KeyCode::Char('g') => self.cursor_to_first(),
            KeyCode::End | KeyCode::Char('G') => self.cursor_to_last(),
            KeyCode::Char(' ') => self.toggle_selection(),
            KeyCode::Char('a') => self.toggle_select_all_visible(),
            // Task 12 gives `Esc` its other jobs (leave search, dismiss a
            // dialog); in Normal mode it is the panic button for a selection.
            KeyCode::Esc => self.clear_selection(),
            _ => {}
        }
    }
}

/// A page jump as a signed row count, saturating rather than wrapping on the
/// (impossible in practice) terminal taller than `isize::MAX` rows.
fn page_delta(page_size: usize) -> isize {
    isize::try_from(page_size).unwrap_or(isize::MAX)
}

/// Parse a captured DSM `list` response envelope into tasks.
///
/// Deliberately the *same* path a live response takes —
/// [`parse_envelope`] into [`TaskList`] — rather than a second, laxer parser:
/// a fixture that only the fixture loader can read would verify nothing about
/// what the NAS actually sends, and `tests/fixtures/task_list.json` is the same
/// file the model tests are built on.
pub fn parse_fixture(body: &str) -> Result<Vec<Task>> {
    let list: TaskList = parse_envelope(body, DS_TASK_API)?;
    Ok(list.tasks)
}

/// Read and parse a captured `list` response from disk.
pub fn read_fixture(path: &Path) -> Result<Vec<Task>> {
    let body = std::fs::read_to_string(path)?;
    let tasks = parse_fixture(&body)?;
    tracing::info!(
        fixture = %path.display(),
        tasks = tasks.len(),
        "loaded an offline fixture"
    );
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskStatus;
    use crate::view::StatusFilter;

    const FIXTURE: &str = include_str!("../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        parse_envelope::<TaskList>(FIXTURE, "SYNO.DownloadStation.Task")
            .expect("the fixture must parse")
            .tasks
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_new_app_is_in_normal_mode_with_nothing_selected() {
        let app = App::default();
        assert!(app.tasks.is_empty());
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.cursor, 0);
        assert!(app.selected.is_empty());
        assert!(app.status_message.is_none());
        assert!(!app.should_quit());
        assert_eq!(app.visible_count(), 0);
    }

    #[test]
    fn an_app_over_the_fixture_shows_every_task_by_default() {
        let app = App::new(fixture_tasks());
        assert_eq!(app.visible_count(), app.tasks.len());
        assert_eq!(app.visible(), view::visible_indices(&app.tasks, &app.view));
    }

    #[test]
    fn the_view_narrows_what_the_app_shows() {
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        assert_eq!(app.visible_count(), 2);
    }

    #[test]
    fn q_quits() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::Char('q')));
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_quits_from_any_mode() {
        for mode in [Mode::Normal, Mode::Search, Mode::Confirm, Mode::Help] {
            let mut app = App {
                mode,
                ..App::default()
            };
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.should_quit(), "{mode:?}");
        }
    }

    #[test]
    fn a_bare_c_does_not_quit() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::Char('c')));
        assert!(!app.should_quit());
    }

    #[test]
    fn key_releases_are_ignored() {
        // Windows and the kitty protocol report both halves of a keystroke;
        // acting on each would run every binding twice.
        let mut release = press(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        let mut app = App::default();
        app.handle_key(release);
        assert!(!app.should_quit());
    }

    #[test]
    fn an_unbound_key_changes_nothing() {
        let mut app = App::new(fixture_tasks());
        let before = format!("{app:?}");
        app.handle_key(press(KeyCode::Char('z')));
        assert_eq!(format!("{app:?}"), before);
    }

    #[test]
    fn a_resize_event_is_absorbed_without_a_state_change() {
        // The next draw measures the terminal itself, so there is nothing to
        // record — but the event must not be mistaken for a key press either.
        let mut app = App::default();
        app.handle_event(Event::Resize(20, 5));
        assert!(!app.should_quit());
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn a_key_event_arrives_through_handle_event() {
        let mut app = App::default();
        app.handle_event(Event::Key(press(KeyCode::Char('q'))));
        assert!(app.should_quit());
    }

    #[test]
    fn any_key_leaves_a_mode_that_has_no_handler_yet() {
        // Placeholder behaviour until Tasks 12/14/17: a mode with no key
        // handling must never trap the user in it.
        let mut app = App {
            mode: Mode::Help,
            ..App::default()
        };
        app.handle_key(press(KeyCode::Char('x')));
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn the_status_message_is_settable() {
        let mut app = App::default();
        app.set_status("hello");
        assert_eq!(app.status_message.as_deref(), Some("hello"));
    }

    // ---- cursor movement ---------------------------------------------------

    /// An app over `count` synthetic tasks, all visible.
    fn app_with(count: usize) -> App {
        let tasks = (0..count)
            .map(|n| Task {
                id: format!("id_{n:03}"),
                title: format!("task {n:03}"),
                status: TaskStatus::Paused,
                size: 0,
                downloaded: 0,
                uploaded: 0,
                download_speed: 0,
                upload_speed: 0,
                destination: "downloads".to_string(),
                files: Vec::new(),
                seeders: 0,
                leechers: 0,
                create_time: None,
            })
            .collect();
        App::new(tasks)
    }

    #[test]
    fn moving_the_cursor_on_an_empty_list_leaves_it_at_zero() {
        let mut app = App::default();
        for key in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
        ] {
            app.handle_key(press(key));
            assert_eq!(app.cursor, 0, "{key:?}");
        }
        assert!(app.cursor_task().is_none());
    }

    #[test]
    fn a_single_row_list_pins_the_cursor_to_that_row() {
        let mut app = app_with(1);
        for key in [KeyCode::Down, KeyCode::Down, KeyCode::Up, KeyCode::End] {
            app.handle_key(press(key));
            assert_eq!(app.cursor, 0, "{key:?}");
        }
        assert_eq!(app.cursor_task().map(|t| t.id.as_str()), Some("id_000"));
    }

    #[test]
    fn the_cursor_stops_at_the_end_of_the_list() {
        let mut app = app_with(5);
        for _ in 0..20 {
            app.handle_key(press(KeyCode::Down));
        }
        assert_eq!(app.cursor, 4, "past-the-end must clamp, not wrap");
        assert_eq!(app.cursor_task().map(|t| t.id.as_str()), Some("id_004"));
    }

    #[test]
    fn the_cursor_stops_at_the_start_of_the_list() {
        let mut app = app_with(5);
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.cursor, 4);
        for _ in 0..20 {
            app.handle_key(press(KeyCode::Up));
        }
        assert_eq!(app.cursor, 0, "past-the-start must clamp, not wrap");
    }

    #[test]
    fn vi_keys_move_the_cursor_like_the_arrows() {
        let mut app = app_with(5);
        app.handle_key(press(KeyCode::Char('j')));
        app.handle_key(press(KeyCode::Char('j')));
        assert_eq!(app.cursor, 2);
        app.handle_key(press(KeyCode::Char('k')));
        assert_eq!(app.cursor, 1);
        app.handle_key(press(KeyCode::Char('G')));
        assert_eq!(app.cursor, 4);
        app.handle_key(press(KeyCode::Char('g')));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut app = app_with(50);
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.cursor, 49);
        app.handle_key(press(KeyCode::Home));
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn a_page_jump_moves_a_screenful_and_clamps_at_both_ends() {
        let mut app = app_with(100);
        app.set_page_size(10);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, 10);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, 20);
        app.handle_key(press(KeyCode::PageUp));
        assert_eq!(app.cursor, 10);
        for _ in 0..20 {
            app.handle_key(press(KeyCode::PageDown));
        }
        assert_eq!(app.cursor, 99);
        for _ in 0..20 {
            app.handle_key(press(KeyCode::PageUp));
        }
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn the_page_size_starts_usable_and_never_falls_to_zero() {
        let mut app = app_with(100);
        assert_eq!(app.page_size(), DEFAULT_PAGE_SIZE);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, DEFAULT_PAGE_SIZE);

        // A terminal too short for even one table row still has to page.
        app.set_page_size(0);
        assert_eq!(app.page_size(), 1);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, DEFAULT_PAGE_SIZE + 1);
    }

    #[test]
    fn the_cursor_walks_the_visible_list_not_the_underlying_one() {
        // With a filter applied, "down" means the next *visible* row, and the
        // cursor cannot leave the filtered subset.
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        assert_eq!(app.visible_count(), 2);
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.cursor, 1);
        assert_eq!(app.cursor_task().map(|t| t.id.as_str()), Some("dbid_013"));
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn clamping_pulls_a_stale_cursor_back_into_a_narrowed_list() {
        // Task 12 changes the filter under the cursor and Task 11 refreshes the
        // list under it; both rely on this.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.cursor, 13);
        app.view.filter = StatusFilter::Paused;
        app.clamp_cursor();
        assert_eq!(app.cursor, 0);
        assert_eq!(app.cursor_task().map(|t| t.id.as_str()), Some("dbid_004"));
    }

    // ---- selection ---------------------------------------------------------

    /// The IDs currently selected, sorted so assertions are deterministic.
    fn selected_ids(app: &App) -> Vec<String> {
        let mut ids: Vec<String> = app.selected.iter().cloned().collect();
        ids.sort();
        ids
    }

    #[test]
    fn space_toggles_the_row_under_the_cursor_on_and_off() {
        let mut app = app_with(3);
        app.handle_key(press(KeyCode::Down));

        app.handle_key(press(KeyCode::Char(' ')));
        assert_eq!(selected_ids(&app), ["id_001"]);
        assert!(app.is_selected("id_001"));
        assert_eq!(app.selected_count(), 1);

        app.handle_key(press(KeyCode::Char(' ')));
        assert!(app.selected.is_empty());
        assert!(!app.is_selected("id_001"));
    }

    #[test]
    fn space_selects_the_task_not_the_row_number() {
        // The set holds IDs, so a re-sort that moves the row cannot reassign
        // the selection to whatever landed in that position.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char(' ')));
        let picked = app.tasks[app.visible()[0]].id.clone();

        app.view.toggle_dir();
        assert_ne!(app.tasks[app.visible()[0]].id, picked, "the sort must move");
        assert_eq!(selected_ids(&app), [picked]);
    }

    #[test]
    fn space_on_an_empty_list_selects_nothing() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::Char(' ')));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn a_selects_every_visible_row_and_a_second_press_deselects_them() {
        let mut app = app_with(4);
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 4);

        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 0);
    }

    #[test]
    fn select_all_never_touches_a_task_the_filter_hides() {
        // The heart of Task 10: with a filter on, `a` must not arm a delete
        // against rows that are not on screen — in either direction.
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        let visible: Vec<String> = app
            .visible()
            .into_iter()
            .map(|index| app.tasks[index].id.clone())
            .collect();
        assert_eq!(visible.len(), 2, "the fixture must have hidden tasks too");

        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(selected_ids(&app), visible);
        assert_eq!(app.selected.len(), 2, "no hidden task was selected");

        // A hidden task selected earlier must survive a visible-set deselect.
        let hidden = app
            .tasks
            .iter()
            .find(|task| !visible.contains(&task.id))
            .expect("a hidden task")
            .id
            .clone();
        app.selected.insert(hidden.clone());
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(
            selected_ids(&app),
            [hidden],
            "deselecting the visible rows must leave the hidden one alone"
        );
    }

    #[test]
    fn select_all_over_a_partial_selection_selects_the_rest_rather_than_clearing() {
        let mut app = app_with(4);
        app.handle_key(press(KeyCode::Char(' ')));
        assert_eq!(app.selected_count(), 1);
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 4, "a partial set fills up first");
    }

    #[test]
    fn select_all_on_an_empty_visible_set_is_a_no_op() {
        let mut app = App::new(fixture_tasks());
        app.view.search = "no-such-task".to_string();
        assert_eq!(app.visible_count(), 0);
        app.handle_key(press(KeyCode::Char('a')));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn esc_clears_the_whole_selection_including_hidden_tasks() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 14);

        app.view.filter = StatusFilter::Seeding;
        app.handle_key(press(KeyCode::Esc));
        assert!(
            app.selected.is_empty(),
            "Esc clears everything, not just the visible rows"
        );
        assert_eq!(app.selected_count(), 0);
        assert_eq!(app.selected_size(), 0);
    }

    #[test]
    fn the_selected_size_is_the_sum_of_the_selected_tasks() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('a')));
        let total: u64 = app.tasks.iter().map(|task| task.size).sum();
        assert_eq!(app.selected_size(), total);
        assert!(total > 0, "the fixture must have sizes to sum");

        // ...and drops back to just the remaining task when one is deselected.
        let first = app.tasks[app.visible()[0]].clone();
        app.handle_key(press(KeyCode::Char(' ')));
        assert_eq!(app.selected_size(), total - first.size);
        assert_eq!(app.selected_count(), 13);
    }

    #[test]
    fn a_selected_id_with_no_task_behind_it_is_not_counted_or_summed() {
        // Task 11 prunes these on refresh; until it does, the footer must not
        // claim a task that is not there.
        let mut app = app_with(2);
        app.selected.insert("id_000".to_string());
        app.selected.insert("ghost".to_string());
        assert_eq!(app.selected_count(), 1);
        assert_eq!(app.selected_size(), 0);
    }

    // ---- fixture mode ------------------------------------------------------

    #[test]
    fn the_checked_in_fixture_loads_straight_off_disk() {
        // The same file the model tests parse, through the same envelope
        // reader — `--fixture` is not a second, laxer parser.
        let app = App::from_fixture(Path::new("tests/fixtures/task_list.json"))
            .expect("the checked-in fixture must load");
        assert_eq!(app.tasks.len(), 14);
        assert_eq!(app.visible_count(), 14);
        assert_eq!(app.cursor, 0);
        assert_eq!(app.tasks, fixture_tasks());
    }

    #[test]
    fn a_fixture_is_a_full_dsm_list_envelope() {
        let tasks = parse_fixture(
            r#"{"success": true, "data": {"total": 1, "offset": 0,
                 "tasks": [{"id": "x", "title": "t", "status": "paused"}]}}"#,
        )
        .expect("a well-formed envelope");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "x");
    }

    #[test]
    fn a_fixture_that_is_not_an_envelope_is_an_error_not_an_empty_table() {
        // A bare task array is the tempting mistake — it must not silently
        // yield zero tasks.
        assert!(parse_fixture(r#"[{"id": "x"}]"#).is_err());
        assert!(parse_fixture("not json at all").is_err());
    }

    #[test]
    fn a_fixture_holding_a_dsm_error_reports_that_error() {
        let err = parse_fixture(r#"{"success": false, "error": {"code": 105}}"#)
            .expect_err("a captured failure");
        assert!(
            matches!(err, crate::error::Error::Dsm { code: 105, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_missing_fixture_file_is_an_io_error() {
        let err = App::from_fixture(Path::new("tests/fixtures/no-such-file.json"))
            .expect_err("missing file");
        assert!(matches!(err, crate::error::Error::Io(_)), "{err:?}");
    }
}
