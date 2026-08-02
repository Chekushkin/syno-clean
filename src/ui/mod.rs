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
//! The task table and the modals land in `ui::table` (Task 9) and `ui::dialog`
//! (Task 14); for now the body is a bordered placeholder.

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
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;

/// The backend this program draws on: crossterm over stdout.
pub type Backend = CrosstermBackend<Stdout>;

/// Footer hints in [`crate::app::Mode::Normal`]. The full list is the `?`
/// overlay (Task 17); this is the reminder that it exists.
const NORMAL_HINTS: &str = "q quit · ? help";

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
/// is a bordered placeholder until the table lands in Task 9.
pub fn render(frame: &mut Frame, app: &App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_title_bar(frame, app, header);
    frame.render_widget(body_placeholder(app), body);
    frame.render_widget(footer_bar(app), footer);
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

/// The body. Task 9 replaces this with the task table.
fn body_placeholder(app: &App) -> Paragraph<'static> {
    let message = if !app.tasks.is_empty() {
        // The table itself lands in Task 9; until then, say what is loaded.
        format!("{} tasks loaded", app.tasks.len())
    } else if app.view.is_narrowed() {
        "No tasks match the current filter".to_string()
    } else {
        "No tasks".to_string()
    };

    Paragraph::new(message)
        .centered()
        .block(Block::bordered().title(" Tasks "))
}

/// The footer: the last status message, or the key hints when there is none.
fn footer_bar(app: &App) -> Paragraph<'static> {
    let text = match &app.status_message {
        Some(message) => format!(" {message} "),
        None => format!(" {NORMAL_HINTS} "),
    };
    Paragraph::new(Line::from(text)).style(Style::default().add_modifier(Modifier::DIM))
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

    #[test]
    fn an_empty_app_renders_a_title_bar_a_bordered_body_and_a_footer() {
        let lines = frame_lines(&App::default(), 60, 8);
        assert_eq!(lines.len(), 8);

        assert!(lines[0].contains(env!("CARGO_PKG_NAME")), "{:?}", lines[0]);
        assert!(lines[0].contains("0 / 0 tasks"), "{:?}", lines[0]);
        // The body is bordered on all four sides.
        assert!(lines[1].starts_with('┌') && lines[1].ends_with('┐'));
        assert!(lines[6].starts_with('└') && lines[6].ends_with('┘'));
        assert!(lines[1].contains("Tasks"));
        assert!(lines.iter().any(|line| line.contains("No tasks")));
        assert!(lines[7].contains(NORMAL_HINTS), "{:?}", lines[7]);
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
        let mut app = App::default();
        app.view.search = "no-such-task".to_string();
        assert!(frame_text(&app, 60, 8).contains("No tasks match"));
        // ...whereas a plain empty list does not claim a filter is to blame.
        assert!(!frame_text(&App::default(), 60, 8).contains("No tasks match"));
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
    fn rendering_survives_a_terminal_too_small_for_the_layout() {
        // A one-column, one-row terminal has no room for the header, the body
        // and the footer. ratatui must clip rather than panic — resizing a
        // terminal down to nothing is a thing users do.
        for (width, height) in [(1_u16, 1_u16), (1, 3), (3, 1), (2, 2)] {
            let lines = frame_lines(&App::new(fixture_tasks()), width, height);
            assert_eq!(lines.len(), height as usize);
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
