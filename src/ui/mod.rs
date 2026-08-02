//! Terminal lifecycle and frame rendering.
//!
//! Two responsibilities, kept in one module because they are two halves of the
//! same contract — one takes the terminal over, the other draws on it:
//!
//! * [`TerminalGuard`] enables raw mode and the alternate screen on
//!   construction and **restores both on `Drop`**, so every exit path — a clean
//!   `q`, a `?` bubbling out of the event loop, a panic, a caught signal that
//!   unwinds — leaves the user's shell usable. Restoring at the end of `main`
//!   instead would only cover the happy path.
//! * [`render`] draws a frame from `&App` and nothing else. It never mutates
//!   state, never reads the network and never touches a global, which is what
//!   makes it testable against ratatui's `TestBackend` with no TTY at all.
//!
//! `crossterm` is reached exclusively through the `ratatui::crossterm`
//! re-export — see `CLAUDE.md`; adding it as a direct dependency would put two
//! incompatible crossterms in the tree.
//!
//! The modals live in [`dialog`]; the task table is [`table`].

pub mod dialog;
pub mod table;

use std::io::{self, Stdout, stdout};
use std::sync::Once;

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::{App, Mode};
use crate::format;
use crate::view::{StatusFilter, View};

/// The backend this program draws on: crossterm over stdout.
pub type Backend = CrosstermBackend<Stdout>;

/// Footer hints in [`crate::app::Mode::Normal`]. The full list is the `?`
/// overlay (Task 17); this is the reminder that it exists.
const NORMAL_HINTS: &str = "d delete · p/u pause/resume · r refresh · q quit · ? help";

/// Footer hints while the search box has focus.
const SEARCH_HINTS: &str = "Enter apply · Esc cancel";

/// Prefix on the non-fatal error banner, so a failure is recognizable as one
/// even where colour is unavailable.
pub const ERROR_MARKER: &str = "⚠";

/// What the search input line opens with — the key that opened it.
pub const SEARCH_PROMPT: &str = "/";

/// The caret drawn at the end of the query.
///
/// A glyph rather than the terminal's own cursor: [`render`] stays a pure
/// function of `&App` that a `TestBackend` can assert on, and the cursor is
/// hidden for the whole session (see [`TerminalGuard::new`]) rather than being
/// shown and hidden per mode.
pub const SEARCH_CARET: &str = "█";

/// Rows the frame spends on chrome: the title bar, the table header and the
/// footer.
const CHROME_ROWS: u16 = 3;

/// Owns the terminal for as long as the TUI is running.
///
/// Construction is the *only* place raw mode and the alternate screen are
/// entered, and [`Drop`] is the only place they are left. Holding the
/// [`Terminal`] inside the guard rather than beside it makes that impossible to
/// get wrong: a terminal that can be drawn on cannot outlive the restoration.
#[derive(Debug)]
pub struct TerminalGuard {
    terminal: Terminal<Backend>,
}

impl TerminalGuard {
    /// Take over the terminal.
    ///
    /// Fails — rather than corrupting anything — when stdout is not a real
    /// terminal (a pipe, a CI log, a redirect), which is exactly what should
    /// happen: there is nothing to take over.
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;

        // Every early return from here on must undo what already succeeded,
        // otherwise a failure halfway through hands back a terminal in raw
        // mode with no program left to read the keys.
        if let Err(err) = execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(err);
        }

        let mut terminal = match Terminal::new(CrosstermBackend::new(stdout())) {
            Ok(terminal) => terminal,
            Err(err) => {
                let _ = restore();
                return Err(err);
            }
        };

        if let Err(err) = terminal.hide_cursor().and_then(|()| terminal.clear()) {
            let _ = restore();
            return Err(err);
        }

        Ok(Self { terminal })
    }

    /// Draw one frame from the current application state.
    pub fn draw(&mut self, app: &App) -> io::Result<()> {
        self.terminal.draw(|frame| render(frame, app))?;
        Ok(())
    }

    /// Rows a `PageUp`/`PageDown` should move on this terminal.
    ///
    /// The event loop feeds this to [`App::set_page_size`] after each draw, so
    /// a page is a screenful of the table rather than a fixed guess — and the
    /// app stays free of any dependency on the terminal.
    pub fn page_size(&self) -> io::Result<usize> {
        Ok(table_page_size(self.terminal.size()?.height))
    }
}

/// Height of the table body inside a terminal `terminal_height` rows tall.
///
/// At least one row: a terminal too short for the chrome still has to let the
/// user move.
pub fn table_page_size(terminal_height: u16) -> usize {
    usize::from(terminal_height.saturating_sub(CHROME_ROWS)).max(1)
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Errors cannot be propagated out of `Drop`, and there is nowhere to
        // print them — the terminal is precisely what is in doubt — so they go
        // to the log file. Restoring as much as possible beats bailing out on
        // the first failure.
        let _ = self.terminal.show_cursor();
        if let Err(err) = restore() {
            tracing::warn!(%err, "could not fully restore the terminal");
        }
    }
}

/// Undo [`TerminalGuard::new`]: leave the alternate screen and raw mode.
///
/// Idempotent, and safe to call when the terminal was never taken over, so the
/// panic hook and [`Drop`] can both run it without coordinating. Raw mode is
/// disabled *first* — it has the wider blast radius of the two.
pub fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// Restore the terminal before any panic message is printed.
///
/// A panic inside the alternate screen would otherwise scroll its message off
/// with the screen and leave the shell in raw mode — no echo, no newline
/// handling, Ctrl-C dead. The previous hook is **chained, not discarded**, so
/// the default backtrace still prints (and so a test harness or a custom hook
/// installed earlier keeps working). Guarded by a [`Once`]: installing twice
/// would nest the hooks and restore twice.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore();
            previous(info);
        }));
    });
}

/// Draw the whole frame. A pure function of `&App`.
///
/// Three bands: a one-line title bar, the body, and a one-line footer. The body
/// is the task table, or a message when there is nothing to put in it — the
/// table draws its own header row, so an empty table would be a header over
/// blank space with no explanation.
///
/// A modal is drawn **last, over everything**, so the table it describes is
/// still visible around it but nothing can be mistaken for the dialog's own
/// content.
pub fn render(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_title_bar(frame, app, header);
    if app.visible_count() == 0 {
        frame.render_widget(empty_state(app), body);
    } else {
        table::render(frame, app, body);
    }
    frame.render_widget(footer_bar(app), footer);

    // `pending_delete` and `Mode::Confirm` are set together, but the render
    // path asks for both rather than assuming: a mode with no plan behind it
    // must draw no dialog, not an empty one promising to delete nothing.
    if app.mode == Mode::Confirm
        && let Some(plan) = app.pending_delete()
    {
        let summary = dialog::build_confirmation(plan, app.delete_options);
        dialog::render_confirm(
            frame,
            frame.area(),
            &summary,
            app.confirm_scroll(),
            app.confirm_focus(),
        );
    }

    if app.mode == Mode::Help {
        dialog::render_help(frame, frame.area());
    }
}

/// The title bar: what this is on the left, how much of it is on screen on the
/// right, drawn as two halves of one reversed line so the bar stays solid
/// across the full width.
fn render_title_bar(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let style = Style::default().add_modifier(Modifier::REVERSED);
    frame.render_widget(Block::default().style(style), area);

    let [left, right] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(20)]).areas(area);

    let title = format!(" {} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    frame.render_widget(Paragraph::new(Line::from(title)).style(style), left);

    let counts = format!("{} / {} tasks ", app.visible_count(), app.tasks.len());
    frame.render_widget(
        Paragraph::new(Line::from(counts))
            .style(style)
            .right_aligned(),
        right,
    );
}

/// The body when the table has no rows to show.
///
/// **"The NAS has no tasks" and "your filter is hiding all of them" are
/// different problems with different fixes**, and a user who cannot tell them
/// apart will go looking for a network fault that is really an `f` press. So
/// the two states name their own cause and their own way out:
///
/// * nothing to show at all → say so, and point at refresh
/// * rows exist but none survive the view → say **how many** are hidden and
///   **what is hiding them** (the live filter and query, from
///   [`narrowing_summary`]), then name the keys that widen it again
///
/// The test for which one to draw is [`App::tasks`] being empty, *not*
/// [`View::is_narrowed`]: with zero tasks and a filter set, both are true and
/// only the first is the user's actual problem.
fn empty_state(app: &App) -> Paragraph<'static> {
    let (headline, hint) = if app.tasks.is_empty() {
        (
            "No Download Station tasks".to_string(),
            "nothing is queued on the NAS · r refresh · ? help · q quit".to_string(),
        )
    } else {
        let total = app.tasks.len();
        let plural = if total == 1 { "task" } else { "tasks" };
        (
            "No tasks match the current view".to_string(),
            format!(
                "all {total} {plural} hidden by {} · f filter · / search · Esc clears the selection",
                narrowing_summary(&app.view)
            ),
        )
    };

    Paragraph::new(vec![
        Line::from(headline),
        Line::from(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        )),
    ])
    .centered()
    .block(Block::new().padding(Padding::top(1)))
}

/// What is currently hiding rows, as a phrase for the empty state.
///
/// Only the parts that narrow — the sort orders rows, it never removes them —
/// so the sentence names something the user can actually undo.
fn narrowing_summary(view: &View) -> String {
    let mut parts = Vec::new();
    if view.filter != StatusFilter::All {
        parts.push(format!("filter {}", view.filter.label()));
    }
    if !view.search.is_empty() {
        parts.push(format!("search \"{}\"", view.search));
    }
    if parts.is_empty() {
        // Unreachable in practice — with nothing narrowing, every task is
        // visible — but the sentence must still parse if it ever is reached.
        return "the current view".to_string();
    }
    parts.join(" and ")
}

/// How many tasks are selected and how much space deleting them would free.
///
/// `None` when nothing is selected, so the footer is not permanently carrying a
/// `0 selected` that means nothing. The size is the *reason* the count is worth
/// showing — reclaiming the volume is the whole point of the tool — so the two
/// always appear together.
fn selection_summary(app: &App) -> Option<String> {
    let count = app.selected_count();
    if count == 0 {
        return None;
    }
    Some(format!(
        "{count} selected · {}",
        format::bytes(app.selected_size())
    ))
}

/// How the table is currently sorted, and what is narrowing it.
///
/// The sort is always shown: there is always one, and the header marker alone
/// does not say which way an off-screen column points. The filter and the
/// search appear **only when they are hiding rows** — a permanent `filter All`
/// is noise, and its disappearing is exactly the feedback that `f` wrapped back
/// round to showing everything.
fn view_summary(view: &View) -> String {
    let mut parts = vec![format!(
        "sort {}{}",
        view.sort_key.label(),
        view.sort_dir.arrow()
    )];
    if view.filter != StatusFilter::All {
        parts.push(format!("filter {}", view.filter.label()));
    }
    if !view.search.is_empty() {
        parts.push(format!("search \"{}\"", view.search));
    }
    parts.join(" · ")
}

/// The footer: the selection summary, the sort/filter state, then the error
/// banner if there is one, otherwise the last status message or the key hints.
///
/// The selection comes first because it is the state that changes what the next
/// `d` will do. An error outranks the status message and is *not* dimmed — a
/// poll failure is non-fatal, so the only way the user learns the numbers on
/// screen have stopped moving is by reading it.
///
/// While the search box has focus the whole line becomes the input
/// ([`search_bar`]): the query is the only state the user is manipulating, and
/// it must be legible with a long one typed.
fn footer_bar(app: &App) -> Paragraph<'static> {
    if app.mode == Mode::Search {
        return search_bar(&app.view.search);
    }

    let (tail, style) = match &app.error {
        Some(error) => (
            format!("{ERROR_MARKER} {error}"),
            Style::default().fg(Color::Red),
        ),
        None => (
            app.status_message
                .clone()
                .unwrap_or_else(|| NORMAL_HINTS.to_string()),
            Style::default().add_modifier(Modifier::DIM),
        ),
    };

    let mut segments: Vec<String> = Vec::new();
    segments.extend(selection_summary(app));
    segments.push(view_summary(&app.view));
    segments.push(tail);
    Paragraph::new(Line::from(format!(" {} ", segments.join(" · ")))).style(style)
}

/// The search input line, drawn in place of the footer while typing.
///
/// Undimmed and prompt-led, so the mode is obvious at a glance: the one thing
/// worse than a search box is a search box the user does not know they are in.
fn search_bar(query: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(format!(
        " {SEARCH_PROMPT}{query}{SEARCH_CARET} · {SEARCH_HINTS} "
    )))
    .style(Style::default().fg(Color::Yellow))
}

#[cfg(test)]
mod tests {
    //! Rendering tests run against ratatui's `TestBackend`, which draws into an
    //! in-memory `Buffer` — no TTY, no raw mode, nothing to restore. The
    //! terminal *lifecycle* (raw mode, alternate screen, the panic hook) is not
    //! testable this way and is verified by running the binary; what is checked
    //! here is that a frame renders at all and says the right things, which is
    //! the part that silently breaks when a layout constraint changes.

    use super::*;
    use ratatui::backend::TestBackend;

    use crate::api::client::parse_envelope;
    use crate::model::{Task, TaskList};
    use crate::view::StatusFilter;

    const FIXTURE: &str = include_str!("../../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        parse_envelope::<TaskList>(FIXTURE, "SYNO.DownloadStation.Task")
            .expect("the fixture must parse")
            .tasks
    }

    /// Render one frame at `width` x `height` and return it as plain text, one
    /// `String` per row.
    fn frame_lines(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("TestBackend cannot fail");
        terminal
            .draw(|frame| render(frame, app))
            .expect("TestBackend cannot fail");

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn frame_text(app: &App, width: u16, height: u16) -> String {
        frame_lines(app, width, height).join("\n")
    }

    /// The frame as text with the **continuation cell of every double-width
    /// glyph dropped**.
    ///
    /// ratatui stores a wide symbol in one buffer cell and leaves the next one
    /// holding a space it never emits to the terminal. [`frame_lines`] keeps
    /// that space — which is exactly what makes its "every row is one character
    /// per cell" check work — but it means a CJK title read back out of it has
    /// a space after every glyph, and no `contains` against the original string
    /// can match. Use this when asserting that *text* reached the screen.
    fn frame_text_narrow(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("TestBackend cannot fail");
        terminal
            .draw(|frame| render(frame, app))
            .expect("TestBackend cannot fail");

        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        (0..area.height)
            .map(|y| {
                let mut line = String::new();
                let mut skip = 0usize;
                for x in 0..area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    skip = crate::format::display_width(symbol).saturating_sub(1);
                    line.push_str(symbol);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The frame as one whitespace-normalized string.
    ///
    /// For asserting on prose that a paragraph may have **wrapped**: a sentence
    /// broken across two rows still reads as one run of words here, so the test
    /// checks the wording without pinning the modal's width.
    fn frame_words(app: &App, width: u16, height: u16) -> String {
        frame_lines(app, width, height)
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn an_empty_app_renders_a_title_bar_an_empty_state_and_a_footer() {
        // Wide enough for the whole hint line: the footer is clipped rather
        // than wrapped, and this asserts on its text.
        let lines = frame_lines(&App::default(), 90, 8);
        assert_eq!(lines.len(), 8);

        assert!(lines[0].contains(env!("CARGO_PKG_NAME")), "{:?}", lines[0]);
        assert!(lines[0].contains("0 / 0 tasks"), "{:?}", lines[0]);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("No Download Station tasks"))
        );
        // No table header when there is no table.
        assert!(!lines.iter().any(|line| line.contains("Destination")));
        assert!(lines[7].contains(NORMAL_HINTS), "{:?}", lines[7]);
    }

    #[test]
    fn a_populated_app_renders_the_table_with_its_header_row() {
        let lines = frame_lines(&App::new(fixture_tasks()), 140, 20);
        // The header sits directly under the title bar and names every column.
        for header in [
            "Name",
            "Status",
            "Size",
            "Progress",
            "↓ Speed",
            "↑ Speed",
            "Ratio",
            "Seeds/Peers",
            "ETA",
            "Destination",
        ] {
            assert!(
                lines[1].contains(header),
                "{header} missing: {:?}",
                lines[1]
            );
        }
        // ...and the rows below it are tasks, formatted.
        let body = lines[2..].join("\n");
        assert!(body.contains("Ubuntu.24.04.3.LTS.Desktop.amd64"), "{body}");
        assert!(body.contains("downloading"), "{body}");
        assert!(body.contains("8.5 MiB/s"), "{body}");
        assert!(!body.contains("No tasks"), "{body}");
    }

    #[test]
    fn the_header_marks_the_sorted_column() {
        let mut app = App::new(fixture_tasks());
        app.view.sort_key = crate::view::SortKey::Size;
        assert!(frame_lines(&app, 140, 20)[1].contains("Size▲"));
        app.view.toggle_dir();
        assert!(frame_lines(&app, 140, 20)[1].contains("Size▼"));
    }

    #[test]
    fn the_cursor_row_scrolls_into_view_on_a_short_terminal() {
        // Four body rows for fourteen tasks: the last task is only reachable
        // by scrolling, and jumping to it must bring it on screen.
        let mut app = App::new(fixture_tasks());
        app.view.sort_key = crate::view::SortKey::Added;
        app.cursor_to_last();
        let last = app
            .cursor_task()
            .expect("a row under the cursor")
            .title
            .clone();
        let text = frame_text(&app, 140, 7);
        assert!(text.contains(&last), "{last} is off screen:\n{text}");
    }

    #[test]
    fn a_list_far_longer_than_the_screen_scrolls_and_still_fits() {
        // The 500+ task case. The fixture's fourteen tasks never exercise a
        // scrolled window, and the cheapest way a long list breaks is a cursor
        // at the far end scrolling the table off its own frame.
        let mut tasks = Vec::new();
        for round in 0..45 {
            for task in fixture_tasks() {
                tasks.push(Task {
                    id: format!("{}_{round}", task.id),
                    ..task
                });
            }
        }
        let total = tasks.len();
        assert!(total >= 500, "{total}");

        let mut app = App::new(tasks);
        app.set_page_size(40);
        app.cursor_to_last();
        assert_eq!(app.cursor, total - 1);

        // The window holding the cursor is the last full page, not a partial
        // one scrolled past the end.
        let offset = table::scroll_offset(app.cursor, total, 40);
        assert_eq!(offset, total - 40);

        for (width, height) in [(80_u16, 24_u16), (160, 50)] {
            for line in frame_lines(&app, width, height) {
                assert_eq!(line.chars().count(), width as usize);
            }
        }
        // The row the cursor is on is on screen, which is what `End` has to
        // mean over a list twenty screens long.
        let last = app
            .cursor_task()
            .expect("a row under the cursor")
            .title
            .clone();
        let text = frame_text_narrow(&app, 160, 50);
        assert!(text.contains(&last), "the last row is off screen: {last}");
    }

    #[test]
    fn every_row_is_exactly_the_terminal_width() {
        // A layout constraint that overflows shows up here first: the buffer is
        // fixed-size, so a widget that wants more simply loses content.
        for (width, height) in [(20_u16, 5_u16), (60, 24), (200, 60)] {
            for line in frame_lines(&App::new(fixture_tasks()), width, height) {
                assert_eq!(line.chars().count(), width as usize);
            }
        }
    }

    #[test]
    fn the_title_bar_counts_visible_rows_against_the_total() {
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        let text = frame_text(&app, 60, 8);
        assert!(text.contains("2 / 14 tasks"), "{text}");
    }

    #[test]
    fn a_narrowed_view_with_no_matches_reads_differently_from_no_tasks() {
        let mut app = App::new(fixture_tasks());
        app.view.search = "no-such-task".to_string();
        let narrowed = frame_words(&app, 90, 8);
        assert!(
            narrowed.contains("No tasks match the current view"),
            "{narrowed}"
        );
        // It says how many rows are hidden and what is hiding them, so the fix
        // is on screen rather than guessed at.
        assert!(narrowed.contains("all 14 tasks hidden"), "{narrowed}");
        assert!(narrowed.contains("search \"no-such-task\""), "{narrowed}");
        assert!(narrowed.contains("f filter"), "{narrowed}");

        // ...whereas a plain empty list does not claim a filter is to blame.
        let empty = frame_words(&App::default(), 90, 8);
        assert!(empty.contains("No Download Station tasks"), "{empty}");
        assert!(!empty.contains("No tasks match"), "{empty}");
        assert!(empty.contains("r refresh"), "{empty}");
    }

    #[test]
    fn zero_tasks_beats_a_filter_as_the_explanation() {
        // Both are true with an empty list and a filter set, and only one of
        // them is the user's actual problem: pressing `f` will not conjure a
        // download that does not exist.
        let mut app = App::default();
        app.view.filter = StatusFilter::Seeding;
        app.view.search = "anything".to_string();
        assert!(app.view.is_narrowed());

        let text = frame_words(&app, 90, 8);
        assert!(text.contains("No Download Station tasks"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn the_narrowed_empty_state_names_both_the_filter_and_the_search() {
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Error;
        app.view.search = "zzz".to_string();
        let text = frame_words(&app, 90, 8);
        assert!(text.contains("filter Error and search \"zzz\""), "{text}");
    }

    // ---- the help overlay --------------------------------------------------

    #[test]
    fn the_help_overlay_draws_over_the_table_with_every_binding_on_it() {
        let mut app = App::new(fixture_tasks());
        app.show_help();
        let text = frame_words(&app, 120, 40);

        assert!(text.contains(dialog::HELP_TITLE), "{text}");
        assert!(text.contains(dialog::HELP_DISMISS), "{text}");
        for section in dialog::HELP_SECTIONS {
            assert!(text.contains(section.title), "{} missing", section.title);
            for entry in section.entries {
                assert!(text.contains(entry.action), "{:?} missing", entry.action);
            }
        }
        // The table is still underneath, but the overlay is over it.
        assert!(text.contains("Destination"), "{text}");
    }

    #[test]
    fn the_help_overlay_never_overflows_the_terminal() {
        let mut app = App::new(fixture_tasks());
        app.show_help();
        // Including sizes too narrow for two columns and too short for the
        // whole card: it clips, it does not panic or spill.
        for (width, height) in [(120, 40), (100, 30), (80, 24), (60, 20), (30, 10), (1, 1)] {
            let lines = frame_lines(&app, width, height);
            assert_eq!(lines.len(), usize::from(height), "{width}x{height}");
            for line in &lines {
                assert_eq!(
                    line.chars().count(),
                    usize::from(width),
                    "{width}x{height}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_whole_card_fits_an_ordinary_terminal() {
        // 80x24 is the size a help overlay has to work at, and the two-column
        // layout plus the tightening in `render_help` is what buys it. If a new
        // section pushes it over, this fails rather than silently clipping the
        // last bindings off the bottom.
        let mut app = App::new(fixture_tasks());
        app.show_help();
        let text = frame_words(&app, 80, 24);
        for section in dialog::HELP_SECTIONS {
            assert!(text.contains(section.title), "{} missing", section.title);
            for entry in section.entries {
                assert!(text.contains(entry.action), "{:?} missing", entry.action);
            }
        }
        assert!(text.contains(dialog::HELP_DISMISS), "{text}");
    }

    #[test]
    fn the_help_overlay_is_gone_once_the_mode_is() {
        let app = App::new(fixture_tasks());
        assert!(!frame_words(&app, 120, 40).contains(dialog::HELP_DISMISS));
    }

    #[test]
    fn a_status_message_replaces_the_key_hints_in_the_footer() {
        let mut app = App::default();
        app.set_status("connected to nas.local");
        let lines = frame_lines(&app, 60, 8);
        assert!(
            lines[7].contains("connected to nas.local"),
            "{:?}",
            lines[7]
        );
        assert!(!lines[7].contains(NORMAL_HINTS));
    }

    #[test]
    fn the_footer_reports_the_selection_count_and_the_space_it_would_free() {
        let mut app = App::new(fixture_tasks());
        // Nothing selected: no permanent "0 selected" taking up the footer.
        assert!(!frame_text(&app, 120, 8).contains("selected"));

        app.handle_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('a'),
        ));
        let total: u64 = app.tasks.iter().map(|task| task.size).sum();
        let lines = frame_lines(&app, 120, 8);
        assert!(lines[7].contains("14 selected"), "{:?}", lines[7]);
        assert!(
            lines[7].contains(&crate::format::bytes(total)),
            "{:?}",
            lines[7]
        );
        // The hints are still there — the summary is a prefix, not a takeover.
        assert!(lines[7].contains(NORMAL_HINTS), "{:?}", lines[7]);
    }

    #[test]
    fn a_failed_poll_shows_a_banner_in_the_footer_over_the_status_message() {
        // Non-fatal: the table is still there, still showing the last good
        // data, and the only sign the numbers have stopped moving is this line.
        let mut app = App::new(fixture_tasks());
        app.set_status("nas.local as eduard");
        app.set_error("refresh failed: connection refused");

        let lines = frame_lines(&app, 120, 8);
        assert!(lines[7].contains(ERROR_MARKER), "{:?}", lines[7]);
        assert!(lines[7].contains("connection refused"), "{:?}", lines[7]);
        assert!(!lines[7].contains("nas.local as eduard"), "{:?}", lines[7]);
        assert!(lines[1].contains("Name"), "the table is still drawn");
    }

    #[test]
    fn clearing_the_error_brings_the_status_message_back() {
        let mut app = App::default();
        app.set_status("nas.local as eduard");
        app.set_error("refresh failed");
        app.clear_error();
        assert!(frame_lines(&app, 120, 8)[7].contains("nas.local as eduard"));
    }

    #[test]
    fn the_selection_summary_survives_an_error_banner() {
        // Both matter at once: what is armed, and that the list is stale.
        let mut app = App::new(fixture_tasks());
        app.toggle_select_all_visible();
        app.set_error("refresh failed");
        let lines = frame_lines(&app, 120, 8);
        assert!(lines[7].contains("14 selected"), "{:?}", lines[7]);
        assert!(lines[7].contains("refresh failed"), "{:?}", lines[7]);
    }

    #[test]
    fn a_selected_row_is_marked_in_the_table() {
        let mut app = App::new(fixture_tasks());
        app.toggle_selection();
        let lines = frame_lines(&app, 140, 20);
        assert_eq!(
            lines[2..]
                .iter()
                .filter(|line| line.contains(table::SELECTED_MARKER))
                .count(),
            1,
            "exactly the one selected row is marked:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn the_footer_names_the_active_sort_column_and_direction() {
        let mut app = App::new(fixture_tasks());
        assert!(frame_lines(&app, 120, 8)[7].contains("sort Name▲"));

        app.cycle_sort();
        app.toggle_sort_dir();
        let footer = frame_lines(&app, 120, 8)[7].clone();
        assert!(footer.contains("sort Status▼"), "{footer:?}");
    }

    #[test]
    fn the_footer_names_a_filter_and_a_search_only_while_they_hide_rows() {
        // A permanent "filter All" is noise; the segment appearing is the
        // feedback that `f` did something, and its going away is the feedback
        // that the cycle wrapped back round.
        let mut app = App::new(fixture_tasks());
        assert!(!frame_lines(&app, 120, 8)[7].contains("filter"));

        app.cycle_filter();
        let footer = frame_lines(&app, 120, 8)[7].clone();
        assert!(footer.contains("filter Downloading"), "{footer:?}");

        app.view.search = "1080p".to_string();
        let footer = frame_lines(&app, 120, 8)[7].clone();
        assert!(footer.contains("search \"1080p\""), "{footer:?}");

        // ...and back to All with the query cleared, both segments are gone.
        for _ in 1..StatusFilter::ALL.len() {
            app.cycle_filter();
        }
        app.view.search.clear();
        let footer = frame_lines(&app, 120, 8)[7].clone();
        assert!(!footer.contains("filter"), "{footer:?}");
        assert!(!footer.contains("search"), "{footer:?}");
    }

    #[test]
    fn the_search_box_takes_over_the_footer_while_typing() {
        let mut app = App::new(fixture_tasks());
        app.begin_search();
        for c in "108".chars() {
            app.search_push(c);
        }

        let lines = frame_lines(&app, 120, 8);
        let footer = &lines[7];
        assert!(footer.contains("/108"), "{footer:?}");
        assert!(footer.contains(SEARCH_CARET), "{footer:?}");
        assert!(footer.contains(SEARCH_HINTS), "{footer:?}");
        assert!(!footer.contains(NORMAL_HINTS), "{footer:?}");
        // The table keeps rendering underneath, already narrowed.
        assert!(lines[1].contains("Name"), "{:?}", lines[1]);
        assert!(lines[0].contains("3 / 14 tasks"), "{:?}", lines[0]);
    }

    #[test]
    fn leaving_the_search_box_gives_the_footer_back() {
        let mut app = App::new(fixture_tasks());
        app.set_status("nas.local as eduard");
        app.begin_search();
        app.search_push('x');
        assert!(!frame_lines(&app, 120, 8)[7].contains("nas.local as eduard"));

        app.cancel_search();
        let footer = frame_lines(&app, 120, 8)[7].clone();
        assert!(footer.contains("nas.local as eduard"), "{footer:?}");
        assert!(!footer.contains(SEARCH_CARET), "{footer:?}");
    }

    #[test]
    fn rendering_survives_a_terminal_too_small_for_the_layout() {
        // A one-column, one-row terminal has no room for the header, the body
        // and the footer. ratatui must clip rather than panic — resizing a
        // terminal down to nothing is a thing users do.
        for (width, height) in [(1_u16, 1_u16), (1, 3), (3, 1), (2, 2)] {
            let lines = frame_lines(&App::new(fixture_tasks()), width, height);
            assert_eq!(lines.len(), height as usize);
        }
    }

    // ---- the delete confirmation modal -------------------------------------

    /// An app with the confirmation modal open over the named fixture tasks.
    fn confirming(ids: &[&str]) -> App {
        let mut app = App::new(fixture_tasks());
        for id in ids {
            app.selected.insert((*id).to_string());
        }
        app.begin_delete();
        assert_eq!(app.mode, Mode::Confirm, "the dialog must have opened");
        app
    }

    #[test]
    fn the_confirmation_modal_lists_what_will_go_and_what_it_frees() {
        let app = confirming(&["dbid_001"]);
        let text = frame_text(&app, 120, 24);

        assert!(text.contains("Delete 1 task"), "{text}");
        assert!(text.contains("Ubuntu.24.04.3.LTS.Desktop.amd64"), "{text}");
        assert!(
            text.contains("/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64"),
            "the resolved path must be on screen:\n{text}"
        );
        assert!(text.contains("to free"), "{text}");
        assert!(text.contains(dialog::CANCEL_LABEL), "{text}");
        assert!(text.contains(dialog::DELETE_LABEL), "{text}");
    }

    #[test]
    fn the_modal_shows_a_refused_task_as_skipped_with_its_reason() {
        // Never silently dropped: the user has to be able to see that this one
        // was left alone, and why.
        let app = confirming(&["dbid_001", "dbid_013"]);
        let text = frame_text(&app, 120, 30);

        assert!(text.contains(dialog::SKIP_MARKER), "{text}");
        assert!(text.contains("Mixed.Root.Release"), "{text}");
        assert!(text.contains("no single top-level"), "{text}");
        assert!(text.contains("1 skipped"), "{text}");
    }

    #[test]
    fn the_modal_says_when_only_the_task_goes_and_the_files_stay() {
        let mut app = confirming(&["dbid_001"]);
        app.delete_options = crate::delete::DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        // The sentence itself is asserted in `dialog`'s own tests; here it only
        // has to reach the screen, and a border sits between its wrapped rows.
        let text = frame_words(&app, 120, 24);
        assert!(text.contains("task only"), "{text}");
        assert!(text.contains("left on disk"), "{text}");
        assert!(!text.contains("to free"), "nothing is freed:\n{text}");
    }

    #[test]
    fn a_dry_run_modal_is_labelled_as_one() {
        let mut app = confirming(&["dbid_001"]);
        app.delete_options = crate::delete::DeleteOptions::dry_run();
        let text = frame_words(&app, 120, 24);
        assert!(text.contains(dialog::DRY_RUN_MARKER), "{text}");
        assert!(text.contains("nothing is deleted"), "{text}");
    }

    #[test]
    fn no_modal_is_drawn_outside_confirm_mode() {
        let mut app = confirming(&["dbid_001"]);
        assert!(frame_text(&app, 120, 24).contains(dialog::CANCEL_LABEL));

        app.cancel_delete();
        let text = frame_text(&app, 120, 24);
        assert!(!text.contains(dialog::CANCEL_LABEL), "{text}");
        // ...and the table is back, unobscured (its Name column truncates).
        assert!(text.contains("Ubuntu.24.04"), "{text}");
    }

    #[test]
    fn a_confirm_mode_with_no_plan_behind_it_draws_no_dialog() {
        // Belt and braces: the two are set together, but a mode alone must not
        // produce an empty modal.
        let mut app = App::new(fixture_tasks());
        app.mode = Mode::Confirm;
        assert!(!frame_text(&app, 120, 24).contains(dialog::CANCEL_LABEL));
    }

    #[test]
    fn the_modal_scrolls_a_plan_taller_than_it_is() {
        let mut app = App::new(fixture_tasks());
        app.toggle_select_all_visible();
        app.begin_delete();

        let top = frame_text(&app, 120, 16);
        assert!(top.contains("scroll"), "a hint that there is more:\n{top}");

        for _ in 0..8 {
            app.scroll_confirm(1);
        }
        let scrolled = frame_text(&app, 120, 16);
        assert_ne!(top, scrolled, "the body did not move");
        // The chrome stays put while the body moves.
        assert!(scrolled.contains(dialog::CANCEL_LABEL), "{scrolled}");
        assert!(scrolled.contains("to free"), "{scrolled}");
    }

    #[test]
    fn the_modal_never_overflows_the_terminal() {
        for (width, height) in [(1_u16, 1_u16), (10, 4), (40, 8), (120, 24), (200, 60)] {
            let app = confirming(&["dbid_001", "dbid_013"]);
            for line in frame_lines(&app, width, height) {
                assert_eq!(line.chars().count(), width as usize, "{width}x{height}");
            }
        }
    }

    #[test]
    fn rendering_never_mutates_the_app() {
        // `render` takes `&App` and the event loop relies on that: a redraw
        // triggered by a resize must not change what the next key press means.
        let app = App::new(fixture_tasks());
        let before = format!("{app:?}");
        let _ = frame_text(&app, 80, 24);
        assert_eq!(format!("{app:?}"), before);
    }
}
