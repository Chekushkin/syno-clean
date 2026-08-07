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
use crate::model::VolumeUsage;
use crate::view::{StatusFilter, View};

/// The backend this program draws on: crossterm over stdout.
pub type Backend = CrosstermBackend<Stdout>;

/// Footer hints in [`crate::app::Mode::Normal`]. The full list is the `?`
/// overlay ([`dialog::HELP_SECTIONS`]); this is the reminder that it exists.
const NORMAL_HINTS: &str = "d delete · p/u pause/resume · r refresh · q quit · ? help";

/// Footer hints while the search box has focus.
const SEARCH_HINTS: &str = "Enter commit · Esc cancel";

/// Narrowest the title bar's connection segment may be before it is dropped.
///
/// A `user@host:port` sheared to a few cells is worse than nothing: it costs
/// the space anyway and cannot be read. Below this the title bar keeps just the
/// name and version, and the connection stays reachable in the log.
const MIN_CONNECTION_WIDTH: usize = 12;

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

/// Rows the frame spends on chrome that is **always** there: the title bar,
/// the table header and the footer. The storage band is not in this number —
/// it is passed to [`table_page_size`] as `extra_chrome`.
const CHROME_ROWS: u16 = 3;

/// Cells the storage bar's `████░░░░` body occupies, brackets excluded.
const STORAGE_GAUGE_WIDTH: usize = 20;

/// Occupancy at which the filled run turns yellow.
const STORAGE_WARN_FRACTION: f64 = 0.75;

/// Occupancy at which the filled run turns red.
const STORAGE_CRITICAL_FRACTION: f64 = 0.90;

/// What separates one volume's segment from the next.
const STORAGE_SEPARATOR: &str = "   ";

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
    /// The event loop feeds this to [`App::set_page_size`] after each draw.
    /// `extra_chrome` is a parameter (the caller passes
    /// [`storage_band_height`]) rather than read off an [`App`], keeping the
    /// terminal guard ignorant of application state.
    pub fn page_size(&self, extra_chrome: u16) -> io::Result<usize> {
        Ok(table_page_size(self.terminal.size()?.height, extra_chrome))
    }
}

/// Rows the storage band occupies. The single definition of the band's
/// existence, read by both [`render`] and [`TerminalGuard::page_size`] so the
/// frame and the page size cannot disagree.
pub fn storage_band_height(app: &App) -> u16 {
    u16::from(!app.storage.is_empty())
}

/// Height of the table body inside a terminal `terminal_height` rows tall,
/// given `extra_chrome` rows of optional bands above it.
///
/// At least one row: a terminal too short for the chrome still has to let the
/// user move.
pub fn table_page_size(terminal_height: u16, extra_chrome: u16) -> usize {
    usize::from(
        terminal_height
            .saturating_sub(CHROME_ROWS)
            .saturating_sub(extra_chrome),
    )
    .max(1)
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
/// Four bands: a one-line title bar, the storage band (`Length(0)` — genuinely
/// zero rows — until a storage read has succeeded), the body, and a one-line
/// footer. The body is the task table, or a message when there is nothing to
/// put in it.
///
/// A modal is drawn **last, over `frame.area()`**, so the table it describes is
/// still visible around it but nothing can be mistaken for the dialog's own
/// content.
pub fn render(frame: &mut Frame, app: &App) {
    let [header, storage, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(storage_band_height(app)),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // One [`App::visible`] for the whole frame. It filters, searches and sorts
    // on every call, and the title bar, the empty-state test and the table all
    // ask the same question.
    let visible = app.visible();
    render_title_bar(frame, app, header, visible.len());
    // The *area*, not the app: a too-short terminal yields a zero-height band
    // even with storage to show.
    if storage.height > 0 {
        frame.render_widget(
            Paragraph::new(storage_line(&app.storage, usize::from(storage.width))),
            storage,
        );
    }
    if visible.is_empty() {
        frame.render_widget(empty_state(app), body);
    } else {
        table::render(frame, app, body, &visible);
    }
    frame.render_widget(footer_bar(app, footer.width), footer);

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

    // Same "ask for both" rule as the confirmation: a mode with no report
    // behind it draws nothing rather than an empty box.
    if app.mode == Mode::Results
        && let Some(report) = app.last_op_report()
    {
        dialog::render_results(frame, frame.area(), report, app.results_scroll());
    }

    if app.mode == Mode::Help {
        dialog::render_help(frame, frame.area());
    }
}

/// The title bar: what this is on the left, how much of it is on screen on the
/// right, drawn as two halves of one reversed line so the bar stays solid
/// across the full width.
fn render_title_bar(
    frame: &mut Frame,
    app: &App,
    area: ratatui::layout::Rect,
    visible_count: usize,
) {
    let style = Style::default().add_modifier(Modifier::REVERSED);
    frame.render_widget(Block::default().style(style), area);

    let [left, right] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(20)]).areas(area);

    // The connection rides here rather than in the footer. It is true for the
    // whole session, so it earns permanent space — and the footer's default
    // content is the key hints, which a standing message would hide for the
    // entire run.
    let mut title = format!(" {} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
    if let Some(connection) = &app.connection {
        let room = usize::from(left.width).saturating_sub(format::display_width(&title) + 3);
        if room >= MIN_CONNECTION_WIDTH {
            title.push_str(" · ");
            title.push_str(&format::truncate_ellipsis(connection, room));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(title)).style(style), left);

    let counts = format!("{visible_count} / {} tasks ", app.tasks.len());
    frame.render_widget(
        Paragraph::new(Line::from(counts))
            .style(style)
            .right_aligned(),
        right,
    );
}

/// The storage band's contents: one segment per volume
/// (`volume1 [████░░░░] 78.0%  3.1 TiB free of 14.0 TiB`). Pure.
///
/// Never wraps; on a narrow terminal it degrades in three steps: full form,
/// drop each segment's ` free of {total}` tail, then
/// [`format::truncate_ellipsis`] (which drops the colour — re-deriving the cut
/// inside styled spans isn't worth it at that width). All widths via
/// [`format::display_width`].
fn storage_line(volumes: &[VolumeUsage], width: usize) -> Line<'static> {
    let full = storage_spans(volumes, true);
    if spans_width(&full) <= width {
        return Line::from(full);
    }

    let trimmed = storage_spans(volumes, false);
    if spans_width(&trimmed) <= width {
        return Line::from(trimmed);
    }

    let text: String = trimmed.iter().map(|span| span.content.as_ref()).collect();
    Line::from(format::truncate_ellipsis(&text, width))
}

/// The band as styled spans, with or without each segment's size tail. The two
/// runs are built separately so only the occupied part carries colour.
fn storage_spans(volumes: &[VolumeUsage], with_tail: bool) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for volume in volumes {
        if !spans.is_empty() {
            spans.push(Span::raw(STORAGE_SEPARATOR));
        }

        let fraction = volume.fraction();
        let filled_cells = format::gauge_cells(fraction, STORAGE_GAUGE_WIDTH);
        let filled: String = std::iter::repeat_n(format::GAUGE_FILLED, filled_cells).collect();
        let free: String =
            std::iter::repeat_n(format::GAUGE_EMPTY, STORAGE_GAUGE_WIDTH - filled_cells).collect();

        spans.push(Span::raw(format!("{} [", volume.name)));
        spans.push(Span::styled(
            filled,
            Style::default().fg(storage_colour(fraction)),
        ));
        spans.push(Span::raw(free));
        spans.push(Span::raw(format!("] {}", format::percent(fraction))));

        if with_tail {
            spans.push(Span::raw(format!(
                "  {} free of {}",
                format::bytes(volume.free),
                format::bytes(volume.total)
            )));
        }
    }
    spans
}

/// Terminal cells a run of spans will occupy once drawn.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| format::display_width(span.content.as_ref()))
        .sum()
}

/// The filled run's colour, so "almost full" is legible without reading digits.
fn storage_colour(fraction: f64) -> Color {
    if fraction >= STORAGE_CRITICAL_FRACTION {
        Color::Red
    } else if fraction >= STORAGE_WARN_FRACTION {
        Color::Yellow
    } else {
        Color::Green
    }
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
/// whether the view narrows anything: with zero tasks and a filter set both are
/// true, and only the first is the user's actual problem. That is why no
/// `View::is_narrowed` predicate exists — the one caller it would have had must
/// not use it.
///
/// There is a **third** state in front of both: before the first poll has come
/// back — and permanently, if every poll fails — there is no list to describe.
/// Saying "nothing is queued on the NAS" there is an assertion the program
/// cannot make, and directly underneath the red banner that says the NAS is
/// unreachable it contradicts itself. [`App::loaded`] is what distinguishes it.
fn empty_state(app: &App) -> Paragraph<'static> {
    let (headline, hint) = if !app.loaded {
        (
            "Waiting for the task list".to_string(),
            "nothing has come back from the NAS yet · r refresh · ? help · q quit".to_string(),
        )
    } else if app.tasks.is_empty() {
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
    let parts = narrowing_parts(view);
    if parts.is_empty() {
        // Unreachable in practice — with nothing narrowing, every task is
        // visible — but the sentence must still parse if it ever is reached.
        return "the current view".to_string();
    }
    parts.join(" and ")
}

/// The phrases naming whatever is currently removing rows, in a fixed order.
///
/// One definition for both readers — [`narrowing_summary`] joins them with
/// "and" for the empty state, [`view_summary`] appends them to the sort for the
/// footer — so the two can never describe the same view differently.
fn narrowing_parts(view: &View) -> Vec<String> {
    let mut parts = Vec::new();
    if view.filter != StatusFilter::All {
        parts.push(format!("filter {}", view.filter.label()));
    }
    if !view.search.is_empty() {
        parts.push(format!("search \"{}\"", view.search));
    }
    parts
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
    parts.extend(narrowing_parts(view));
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
fn footer_bar(app: &App, width: u16) -> Paragraph<'static> {
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
    Paragraph::new(Line::from(fit_footer(&segments, &tail, width))).style(style)
}

/// The footer line, narrowed to `width` by **dropping context before clipping
/// the message**.
///
/// The line is one row and does not wrap, so something has to go on a narrow
/// terminal. Ratatui's own answer is to clip the right-hand end — which is the
/// message, the only part that is new. So the *context* is dropped first, from
/// the right: the sort goes first (the header carries its marker anyway), then
/// the selection (every selected row is marked in the table). Only when nothing
/// but the message is left is it truncated, with an ellipsis, so that it is at
/// least visibly incomplete rather than silently sheared.
///
/// The whole message is never in here anyway when it is a delete failure: those
/// run past 200 characters and live in the results modal (`v`). This is about
/// not lying about how much of it is on screen.
fn fit_footer(segments: &[String], tail: &str, width: u16) -> String {
    let render = |parts: &[&str]| format!(" {} ", parts.join(" · "));
    let fits = |line: &str| format::display_width(line) <= usize::from(width);

    let mut context: Vec<&str> = segments.iter().map(String::as_str).collect();
    loop {
        let mut parts = context.clone();
        parts.push(tail);
        let line = render(&parts);
        if fits(&line) {
            return line;
        }
        if context.pop().is_none() {
            break;
        }
    }

    // Nothing but the message left, and it is still too long.
    format!(
        " {} ",
        format::truncate_ellipsis(tail, usize::from(width).saturating_sub(2))
    )
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

    use crate::model::Task;
    use crate::testutil::fixture_tasks;
    use crate::view::StatusFilter;

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

    /// An app the poller has answered, with nothing queued on the NAS.
    ///
    /// Distinct from `App::default()`, which is the state *before* the first
    /// poll comes back — see [`empty_state`].
    fn loaded_empty() -> App {
        let mut app = App::default();
        app.loaded = true;
        app
    }

    #[test]
    fn an_empty_app_renders_a_title_bar_an_empty_state_and_a_footer() {
        // Wide enough for the whole hint line: the footer is clipped rather
        // than wrapped, and this asserts on its text.
        let lines = frame_lines(&loaded_empty(), 90, 8);
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
    fn the_window_moves_only_at_its_edges_on_a_twelve_row_terminal() {
        // Found by driving the binary in a pty against the fixture: with the
        // offset derived from the cursor, the cursor was welded to the bottom
        // row — no row below it was ever visible while moving down, and a
        // single Up press slid the whole table up with it.
        //
        // Twelve rows is nine body rows for fourteen tasks.
        let mut app = App::new(fixture_tasks());
        let height = table_page_size(12, storage_band_height(&app));
        assert_eq!(height, 9);
        app.set_page_size(height);

        let title = |app: &App, row: usize| app.tasks[app.visible()[row]].title.clone();

        // Down inside the first screenful: nothing scrolls.
        for _ in 0..8 {
            app.move_cursor(1);
        }
        assert_eq!((app.cursor, app.scroll_offset(height)), (8, 0));

        // Two more presses take the cursor off the bottom edge, and the window
        // follows by exactly those two rows.
        app.move_cursor(1);
        app.move_cursor(1);
        assert_eq!((app.cursor, app.scroll_offset(height)), (10, 2));

        let text = frame_text_narrow(&app, 160, 12);
        assert!(
            text.contains(&title(&app, 10)),
            "the cursor row is off screen:\n{text}"
        );
        assert!(
            !text.contains(&title(&app, 0)),
            "row 0 should have scrolled off:\n{text}"
        );

        // The press that mattered: coming back up *inside* the window must not
        // move it, so the rows the user is reading stay where they are.
        app.move_cursor(-1);
        assert_eq!((app.cursor, app.scroll_offset(height)), (9, 2));
        assert!(
            frame_text_narrow(&app, 160, 12).contains(&title(&app, 10)),
            "the row below the cursor stopped being visible"
        );

        // Only when the cursor reaches the top edge does it move again.
        for _ in 0..7 {
            app.move_cursor(-1);
        }
        assert_eq!((app.cursor, app.scroll_offset(height)), (2, 2));
        app.move_cursor(-1);
        assert_eq!((app.cursor, app.scroll_offset(height)), (1, 1));
    }

    #[test]
    fn a_shrinking_list_pulls_a_stale_window_back_into_range() {
        // The property the derived offset had for free: whatever is stored, a
        // refresh that removed rows must never leave the table showing a window
        // past the end of the list.
        let mut app = App::new(fixture_tasks());
        app.set_page_size(4);
        app.cursor_to_last();
        assert_eq!(app.scroll_offset(4), 10);

        app.apply_event(crate::event::AppEvent::Tasks(
            fixture_tasks().into_iter().take(5).collect(),
        ));
        assert_eq!(
            app.scroll_offset(4),
            1,
            "the window must follow the list in"
        );
        let text = frame_text_narrow(&app, 160, 7);
        assert!(
            text.contains(
                &app.cursor_task()
                    .expect("a row under the cursor")
                    .title
                    .clone()
            ),
            "the cursor row is off screen after the list shrank:\n{text}"
        );
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
        assert_eq!(app.scroll_offset(40), total - 40);

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
        let empty = frame_words(&loaded_empty(), 90, 8);
        assert!(empty.contains("No Download Station tasks"), "{empty}");
        assert!(!empty.contains("No tasks match"), "{empty}");
        assert!(empty.contains("r refresh"), "{empty}");
    }

    #[test]
    fn zero_tasks_beats_a_filter_as_the_explanation() {
        // Both are true with an empty list and a filter set, and only one of
        // them is the user's actual problem: pressing `f` will not conjure a
        // download that does not exist.
        let mut app = loaded_empty();
        app.view.filter = StatusFilter::Seeding;
        app.view.search = "anything".to_string();

        let text = frame_words(&app, 90, 8);
        assert!(text.contains("No Download Station tasks"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn a_list_that_has_never_arrived_does_not_claim_the_nas_is_idle() {
        // Startup, and — the case that matters — a NAS that every poll has
        // failed to reach. "nothing is queued on the NAS" is an assertion the
        // program cannot make there, and directly under the red banner saying
        // the NAS is unreachable it contradicts itself.
        let mut app = App::default();
        assert!(!app.loaded);
        let text = frame_words(&app, 90, 8);
        assert!(text.contains("Waiting for the task list"), "{text}");
        assert!(!text.contains("nothing is queued"), "{text}");

        app.set_error("refresh failed: connection refused");
        let text = frame_words(&app, 90, 8);
        assert!(!text.contains("nothing is queued"), "{text}");

        // One successful poll — even an empty one — settles the question.
        app.apply_tasks(Vec::new());
        assert!(app.loaded);
        let text = frame_words(&app, 90, 8);
        assert!(text.contains("No Download Station tasks"), "{text}");
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
    fn a_connected_session_still_shows_the_key_hints() {
        // The regression this pins: the connection used to be seeded into
        // `status_message` at startup, and nothing ever clears that — so the
        // footer showed "https://… as user · logs: …" for the whole session and
        // the keymap, the only thing on screen that teaches the program, was
        // never visible once. It belongs in the title bar, where it does not
        // compete.
        let app = App::new(fixture_tasks()).with_connection("Chekushkin@192.168.1.170:5001");
        let lines = frame_lines(&app, 120, 10);

        assert!(
            lines[9].contains(NORMAL_HINTS),
            "the hints must survive a real startup: {:?}",
            lines[9]
        );
        assert!(
            lines[0].contains("Chekushkin@192.168.1.170:5001"),
            "the connection belongs in the title bar: {:?}",
            lines[0]
        );
        assert!(
            !lines[9].contains("Chekushkin"),
            "and not in the footer: {:?}",
            lines[9]
        );
    }

    #[test]
    fn the_title_bar_drops_the_connection_rather_than_shearing_it() {
        // A `user@host` cut to a few cells costs the space and cannot be read,
        // and it must never push the task counts off the right-hand end.
        let app = App::new(fixture_tasks())
            .with_connection("a-very-long-account-name@nas.example.internal:5001");

        for width in [40, 50, 60, 80, 120, 200] {
            let lines = frame_lines(&app, width, 10);
            let title = &lines[0];
            assert!(
                title.contains("syno-clean"),
                "name survives at {width}: {title:?}"
            );
            assert!(
                title.contains("tasks"),
                "the counts are never displaced at {width}: {title:?}"
            );
        }
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

    /// [`frame_words`] with the modal's vertical borders dropped, so a sentence
    /// that wraps across rows reads as one string. The whole point of wrapping
    /// is that the tail of a long reason is on screen, and nothing else can
    /// assert that.
    fn unboxed(app: &App, width: u16, height: u16) -> String {
        frame_words(app, width, height)
            .split_whitespace()
            .filter(|word| *word != "│")
            .collect::<Vec<_>>()
            .join(" ")
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
        let app = confirming(&["dbid_001", "dbid_010"]);
        let text = frame_text(&app, 120, 30);

        assert!(text.contains(dialog::SKIP_MARKER), "{text}");
        assert!(text.contains("Hosted.Archive.Part1of3"), "{text}");
        assert!(text.contains("no destination"), "{text}");
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
            let app = confirming(&["dbid_001", "dbid_010"]);
            for line in frame_lines(&app, width, height) {
                assert_eq!(line.chars().count(), width as usize, "{width}x{height}");
            }
        }
    }

    #[test]
    fn a_refusal_reason_is_wrapped_rather_than_cut_off_at_the_border() {
        // The modal is capped at `MAX_MODAL_WIDTH` however wide the terminal
        // is, so truncating a reason put the remedy it names beyond reach at
        // *every* terminal size. A torrent that arrives with no file list is
        // the refusal whose sentence *ends* in that remedy: it has to arrive
        // whole, at 80 columns and at 150.
        let torrent = Task {
            files: Vec::new(),
            ..crate::testutil::fixture_task("dbid_001")
        };
        let mut app = App::new(vec![torrent]);
        app.begin_delete();

        for width in [80_u16, 120, 150] {
            let text = unboxed(&app, width, 30);
            assert!(
                text.contains(
                    "refusing to aim a recursive delete at it (use --no-delete-files to \
                     remove the task without touching the volume)"
                ),
                "{width} columns: the reason is cut off:\n{text}"
            );
        }
    }

    #[test]
    fn a_plan_with_a_skipped_row_carries_the_standing_remedy_line() {
        // The reasons scroll; this line does not, so a user looking at a
        // SKIPPED row always has the way out in front of them.
        let app = confirming(&["dbid_001", "dbid_010"]);
        let text = frame_words(&app, 120, 30);
        assert!(text.contains("--no-delete-files"), "{text}");

        // Nothing skipped, nothing to say.
        let clean = frame_words(&confirming(&["dbid_001"]), 120, 30);
        assert!(!clean.contains("--no-delete-files"), "{clean}");
    }

    // ---- the results modal ---------------------------------------------------

    /// An app that has just finished a delete batch with one failure and one
    /// skip, both with reasons far too long for the footer.
    fn after_a_bad_batch() -> App {
        let mut app = App::new(fixture_tasks());
        app.apply_event(crate::event::AppEvent::OpProgress {
            op: crate::event::OpKind::Delete,
            done: 1,
            total: 2,
            item: crate::event::ItemReport {
                title: "Broken.Release.2019.720p".to_string(),
                outcome: crate::event::ItemOutcome::Failed(
                    "nothing at /downloads/Broken.Release.2019.720p, but this task has finished \
                     downloading, so its data should be there (use --no-delete-files to remove \
                     the task anyway)"
                        .to_string(),
                ),
            },
        });
        app.apply_event(crate::event::AppEvent::OpProgress {
            op: crate::event::OpKind::Delete,
            done: 2,
            total: 2,
            item: crate::event::ItemReport {
                title: "Mixed.Root.Release".to_string(),
                outcome: crate::event::ItemOutcome::Skipped(
                    "the task's 3 files share no single top-level directory".to_string(),
                ),
            },
        });
        app.apply_event(crate::event::AppEvent::OpDone {
            op: crate::event::OpKind::Delete,
            succeeded: 0,
            skipped: 1,
            failed: 1,
        });
        assert_eq!(app.mode, Mode::Results, "the modal must have opened");
        app
    }

    #[test]
    fn the_results_modal_names_every_item_and_its_whole_reason() {
        let app = after_a_bad_batch();
        let text = frame_words(&app, 120, 30);

        assert!(text.contains("1 skipped, 1 failed"), "{text}");
        assert!(text.contains("Broken.Release.2019.720p"), "{text}");
        assert!(text.contains("Mixed.Root.Release"), "{text}");
        assert!(
            text.contains("share no single top-level directory"),
            "{text}"
        );
        assert!(text.contains("--no-delete-files"), "{text}");
        assert!(text.contains(dialog::FAILED_MARKER), "{text}");
        assert!(text.contains(dialog::SKIP_MARKER), "{text}");
        assert!(text.contains(dialog::RESULTS_DISMISS), "{text}");
    }

    #[test]
    fn the_results_modal_is_gone_once_dismissed_and_never_drawn_without_a_report() {
        let mut app = after_a_bad_batch();
        app.close_results();
        assert!(!frame_words(&app, 120, 30).contains(dialog::RESULTS_DISMISS));

        let mut bare = App::new(fixture_tasks());
        bare.mode = Mode::Results;
        assert!(!frame_words(&bare, 120, 30).contains(dialog::RESULTS_DISMISS));
    }

    #[test]
    fn the_results_modal_never_overflows_the_terminal() {
        let app = after_a_bad_batch();
        for (width, height) in [(1_u16, 1_u16), (10, 4), (40, 8), (120, 24), (200, 60)] {
            let lines = frame_lines(&app, width, height);
            assert_eq!(lines.len(), usize::from(height), "{width}x{height}");
            for line in &lines {
                assert_eq!(line.chars().count(), usize::from(width), "{width}x{height}");
            }
        }
    }

    // ---- the footer ----------------------------------------------------------

    #[test]
    fn a_footer_too_long_for_the_terminal_drops_context_before_the_message() {
        let sort = "sort added↓".to_string();
        let selection = "2 selected · 1.0 GiB".to_string();
        let segments = vec![selection.clone(), sort.clone()];
        let tail = "⚠ delete finished: 1 succeeded, 1 failed · v for the reasons";

        // Wide enough for everything.
        let wide = fit_footer(&segments, tail, 120);
        assert!(wide.contains(&selection) && wide.contains(&sort), "{wide}");
        assert!(wide.contains(tail), "{wide}");

        // The sort goes first, then the selection — the message is the part the
        // user has not read yet.
        let narrow = fit_footer(&segments, tail, 70);
        assert!(!narrow.contains(&sort), "{narrow}");
        assert!(narrow.contains(tail), "{narrow}");

        let narrower = fit_footer(&segments, tail, 64);
        assert!(!narrower.contains(&selection), "{narrower}");
        assert!(narrower.contains(tail), "{narrower}");
    }

    #[test]
    fn a_message_that_cannot_fit_at_all_is_elided_not_sheared() {
        let tail = "⚠ delete finished: 1 succeeded, 1 failed · v for the reasons";
        let line = fit_footer(&[], tail, 30);
        assert!(format::display_width(&line) <= 30, "{line:?}");
        assert!(line.contains('…'), "{line:?}");
        assert!(line.starts_with(" ⚠ delete finished"), "{line:?}");

        // Degenerate widths must not panic.
        for width in [0_u16, 1, 2, 3] {
            let line = fit_footer(&[], tail, width);
            assert!(
                format::display_width(&line) <= usize::from(width).max(2),
                "{line:?}"
            );
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
