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

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::model::Task;
use crate::view::{self, View};

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
#[derive(Debug, Clone, Default)]
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
    /// Set by `q` / `Ctrl-C`; the event loop owns the actual exit.
    quit: bool,
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

    /// Keys while browsing the table. Navigation, selection, sorting and the
    /// operations are wired in Tasks 9-16.
    fn handle_normal_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('q') {
            self.quit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;
    use crate::model::TaskList;
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
}
