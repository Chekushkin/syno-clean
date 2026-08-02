//! Modal overlays. Today that is the delete confirmation; the `?` help overlay
//! lands here too in Task 17.
//!
//! The confirmation is the last thing standing between a keystroke and an
//! irreversible recursive delete, so it is built in two halves that can be
//! reasoned about separately:
//!
//! * [`build_confirmation`] turns a [`DeletePlan`] plus the session's
//!   [`DeleteOptions`] into a [`ConfirmSummary`] — plain strings and counts,
//!   **no widgets**. That is what the tests assert on: the totals, the exclusion
//!   of refused items, and the wording that tells the user whether their files
//!   are about to go.
//! * [`render_confirm`] draws that summary and nothing else. It reads; it never
//!   decides.
//!
//! Three rules the dialog exists to enforce:
//!
//! * **Refused items are shown, never dropped.** A task the path resolver would
//!   not touch (`Target::Refused`) is listed as `SKIPPED` with the reason, and
//!   its bytes are left out of the total. Silently omitting it would let a user
//!   believe a torrent was cleaned up when it was not.
//! * **The modal says what will actually happen.** With `delete_files = false`
//!   only the DSM task goes and the files stay; under `--dry-run` nothing goes
//!   at all. Both are stated in the title *and* in the effect line, because
//!   these are the cases where the user's mental model is most likely wrong.
//! * **Cancel is the default.** [`ConfirmFocus::default`] is
//!   [`ConfirmFocus::Cancel`], the Cancel button is the one drawn focused, and
//!   `Enter` on an untouched dialog therefore cancels. `y` is the deliberate
//!   confirm.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph, Wrap};

use crate::app::ConfirmFocus;
use crate::delete::{DeleteItem, DeleteOptions, DeletePlan};
use crate::format::{self, display_width, truncate_ellipsis};

/// Bullet in front of a task that will be deleted.
pub const DELETE_MARKER: &str = "•";

/// Flag in front of a task the path resolver refused. Spelled out rather than
/// drawn as a glyph: this is the line the user must not skim past.
pub const SKIP_MARKER: &str = "SKIPPED";

/// Prefix on the title while `--dry-run` is active.
pub const DRY_RUN_MARKER: &str = "DRY RUN";

/// Label of the button that closes the dialog without deleting anything.
pub const CANCEL_LABEL: &str = "Cancel (Esc)";

/// Label of the button that starts the delete.
pub const DELETE_LABEL: &str = "Delete (y)";

/// Widest the modal is allowed to get, however wide the terminal is. Past this
/// the lines are too long to scan and the paths stop lining up under the titles.
const MAX_MODAL_WIDTH: u16 = 82;

/// Cells of terminal left either side of the modal, so the table underneath
/// still frames it.
const MODAL_MARGIN: u16 = 4;

/// What one line of the scrollable body is, so the renderer can style it
/// without re-parsing the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// The title of a task that will be deleted.
    Delete,
    /// The size and resolved path underneath it.
    Path,
    /// The title of a task that was refused.
    Skipped,
    /// Why it was refused.
    Reason,
}

/// One line of the confirmation body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryLine {
    pub text: String,
    pub kind: LineKind,
}

impl SummaryLine {
    fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

/// Everything the confirmation modal displays, as data.
///
/// Deliberately free of widgets and of `App`: this is the part worth testing,
/// and it is testable from a [`DeletePlan`] alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmSummary {
    /// The modal's border title, carrying the `DRY RUN` label when there is one.
    pub title: String,
    /// One sentence saying what confirming actually does — task only, task and
    /// files, or nothing at all.
    pub effect: String,
    /// The scrollable body: two lines per item, in snapshot order.
    pub lines: Vec<SummaryLine>,
    /// The count-and-bytes line under the list.
    pub totals: String,
    /// How many tasks will be deleted.
    pub delete_count: usize,
    /// How many were refused and will be left alone.
    pub skipped_count: usize,
    /// Bytes the deletable items add up to — refused items excluded.
    pub total_size: u64,
    pub dry_run: bool,
    pub delete_files: bool,
}

impl ConfirmSummary {
    /// How many body lines there are, for the scroll clamp.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }
}

/// Turn a snapshot into everything the modal needs to say.
///
/// The plan's items are listed in **snapshot order** — the order the user had
/// them selected in — rather than grouped, so a row on screen maps to a row in
/// the dialog. Refused items keep their place in that order and are flagged
/// rather than hidden.
///
/// `options` is a parameter rather than a field of the plan because it is
/// session state, not per-task state; see [`DeleteOptions`].
pub fn build_confirmation(plan: &DeletePlan, options: DeleteOptions) -> ConfirmSummary {
    let delete_count = plan.deletable().count();
    let skipped_count = plan.refused().count();
    let total_size = plan.total_size();

    let mut lines = Vec::with_capacity(plan.len() * 2);
    for item in &plan.items {
        lines.extend(item_lines(item));
    }

    ConfirmSummary {
        title: title(delete_count, options),
        effect: effect(options),
        lines,
        totals: totals(delete_count, skipped_count, total_size, options),
        delete_count,
        skipped_count,
        total_size,
        dry_run: options.dry_run,
        delete_files: options.delete_files,
    }
}

/// The two lines one item contributes: what it is, then what happens to it.
fn item_lines(item: &DeleteItem) -> [SummaryLine; 2] {
    match (item.path(), item.refusal()) {
        (Some(path), _) => [
            SummaryLine::new(LineKind::Delete, format!("{DELETE_MARKER} {}", item.title)),
            SummaryLine::new(
                LineKind::Path,
                format!("    {}  {path}", format::bytes(item.size)),
            ),
        ],
        // A refused item shows no size: it is excluded from the total, and a
        // number next to it would read as bytes that are about to be freed.
        (None, reason) => [
            SummaryLine::new(LineKind::Skipped, format!("{SKIP_MARKER}  {}", item.title)),
            SummaryLine::new(
                LineKind::Reason,
                format!("    {}", reason.unwrap_or("no reason given")),
            ),
        ],
    }
}

/// `Delete 3 tasks`, prefixed with the dry-run label when one applies.
fn title(delete_count: usize, options: DeleteOptions) -> String {
    let subject = format!("Delete {delete_count} {}", tasks(delete_count));
    if options.dry_run {
        format!("{DRY_RUN_MARKER} · {subject}")
    } else {
        subject
    }
}

/// The sentence that has to be right: what confirming removes.
fn effect(options: DeleteOptions) -> String {
    let scope = if options.delete_files {
        "the Download Station task and its files on the NAS"
    } else {
        "the Download Station task only — the files on the NAS are left in place"
    };

    if options.dry_run {
        format!("Dry run: nothing is deleted. Would remove {scope}.")
    } else {
        format!("Removes {scope}. This cannot be undone.")
    }
}

/// The count-and-bytes line: how many tasks, how much space, how many skipped.
fn totals(
    delete_count: usize,
    skipped_count: usize,
    total_size: u64,
    options: DeleteOptions,
) -> String {
    let mut totals = if delete_count == 0 {
        "Nothing will be deleted".to_string()
    } else if options.delete_files {
        // The reason the tool exists, so it is the number that gets the words.
        format!(
            "{delete_count} {} · {} to free",
            tasks(delete_count),
            format::bytes(total_size)
        )
    } else {
        format!(
            "{delete_count} {} · {} left on disk",
            tasks(delete_count),
            format::bytes(total_size)
        )
    };

    if skipped_count > 0 {
        totals.push_str(&format!(" · {skipped_count} skipped"));
    }
    totals
}

/// `task` / `tasks`.
fn tasks(count: usize) -> &'static str {
    if count == 1 { "task" } else { "tasks" }
}

/// The rectangle a `width` x `height` modal occupies inside `area`, centred and
/// never larger than what it is centred in.
pub fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Rows a paragraph of `text` needs when wrapped to `width` cells.
fn wrapped_height(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let width = usize::from(width);
    let cells = display_width(text);
    u16::try_from(cells.div_ceil(width).max(1)).unwrap_or(u16::MAX)
}

/// Draw the confirmation over whatever is already on screen.
///
/// `scroll` is the first body line to show. It is **clamped here** rather than
/// stored clamped, the same way the table's scroll offset is derived: the
/// modal's height is a property of the frame, not of the application state.
pub fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    summary: &ConfirmSummary,
    scroll: usize,
    focus: ConfirmFocus,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let width = area
        .width
        .saturating_sub(MODAL_MARGIN)
        .clamp(1, MAX_MODAL_WIDTH);
    // Borders and the one-cell padding either side.
    let inner_width = width.saturating_sub(4).max(1);
    let effect_rows = wrapped_height(&summary.effect, inner_width);
    let body_rows = u16::try_from(summary.line_count()).unwrap_or(u16::MAX);
    // effect + blank + body + blank + totals + buttons, inside the border.
    let height = effect_rows
        .saturating_add(body_rows)
        .saturating_add(4)
        .saturating_add(2)
        .min(area.height)
        .max(3);

    let modal = centred(area, width, height);
    // Nothing of the table may show through: a half-visible row behind a delete
    // confirmation is exactly the wrong kind of ambiguity.
    frame.render_widget(Clear, modal);

    let block = Block::bordered()
        .title(format!(" {} ", summary.title))
        .border_style(border_style(summary))
        .padding(Padding::horizontal(1));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [effect_area, body_area, totals_area, buttons_area] = Layout::vertical([
        Constraint::Length(effect_rows + 1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(summary.effect.clone())
            .wrap(Wrap { trim: true })
            .style(effect_style(summary)),
        effect_area,
    );

    let visible = usize::from(body_area.height);
    let offset = scroll.min(summary.line_count().saturating_sub(visible));
    let body: Vec<Line> = summary
        .lines
        .iter()
        .skip(offset)
        .take(visible)
        .map(|line| body_line(line, body_area.width))
        .collect();
    frame.render_widget(Paragraph::new(body), body_area);

    let more = summary.line_count() > offset + visible || offset > 0;
    frame.render_widget(Paragraph::new(totals_line(summary, more)), totals_area);
    frame.render_widget(
        Paragraph::new(buttons_line(focus, summary)).centered(),
        buttons_area,
    );
}

/// Red for a real delete, yellow for a dry run — the border is the fastest
/// thing to read, so it carries the "this is not armed" signal.
fn border_style(summary: &ConfirmSummary) -> Style {
    if summary.dry_run {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Red)
    }
}

fn effect_style(summary: &ConfirmSummary) -> Style {
    if summary.dry_run {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

/// One body line, truncated to the modal's width at display width so a CJK
/// title cannot spill past the border.
fn body_line(line: &SummaryLine, width: u16) -> Line<'static> {
    let style = match line.kind {
        LineKind::Delete => Style::default(),
        LineKind::Path => Style::default().add_modifier(Modifier::DIM),
        LineKind::Skipped => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        LineKind::Reason => Style::default().fg(Color::Yellow),
    };
    Line::from(Span::styled(
        truncate_ellipsis(&line.text, usize::from(width)),
        style,
    ))
}

/// The totals, plus a scroll hint when the list does not fit.
fn totals_line(summary: &ConfirmSummary, more: bool) -> Line<'static> {
    let mut spans = vec![Span::styled(
        summary.totals.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if more {
        spans.push(Span::styled(
            " · ↑/↓ scroll",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The button bar. Cancel is on the left and focused by default; the focused
/// button is reversed, so which key `Enter` will press is unambiguous.
fn buttons_line(focus: ConfirmFocus, summary: &ConfirmSummary) -> Line<'static> {
    let delete_label = if summary.dry_run {
        "Dry run (y)"
    } else {
        DELETE_LABEL
    };
    Line::from(vec![
        button(CANCEL_LABEL, focus == ConfirmFocus::Cancel, Color::Green),
        Span::raw("   "),
        button(
            delete_label,
            focus == ConfirmFocus::Delete,
            if summary.dry_run {
                Color::Yellow
            } else {
                Color::Red
            },
        ),
    ])
}

fn button(label: &str, focused: bool, colour: Color) -> Span<'static> {
    let mut style = Style::default().fg(colour);
    if focused {
        style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    Span::styled(format!("[ {label} ]"), style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;
    use crate::model::{Task, TaskList};

    const FIXTURE: &str = include_str!("../../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        parse_envelope::<TaskList>(FIXTURE, "SYNO.DownloadStation.Task")
            .expect("the fixture must parse")
            .tasks
    }

    fn task(id: &str) -> Task {
        fixture_tasks()
            .into_iter()
            .find(|task| task.id == id)
            .unwrap_or_else(|| panic!("fixture has no task {id}"))
    }

    /// A plan over the named fixture tasks, in the order given.
    fn plan(ids: &[&str]) -> DeletePlan {
        let tasks: Vec<Task> = ids.iter().map(|id| task(id)).collect();
        DeletePlan::snapshot(tasks.iter())
    }

    fn summary(ids: &[&str]) -> ConfirmSummary {
        build_confirmation(&plan(ids), DeleteOptions::default())
    }

    /// The whole body as one string, for "does it mention…" assertions.
    fn body(summary: &ConfirmSummary) -> String {
        summary
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ---- what the summary says ---------------------------------------------

    #[test]
    fn every_deletable_task_contributes_its_title_size_and_resolved_path() {
        let summary = summary(&["dbid_001"]);
        let task = task("dbid_001");

        assert_eq!(summary.delete_count, 1);
        assert_eq!(summary.skipped_count, 0);
        assert_eq!(summary.lines.len(), 2);

        assert_eq!(summary.lines[0].kind, LineKind::Delete);
        assert!(summary.lines[0].text.contains(&task.title));

        assert_eq!(summary.lines[1].kind, LineKind::Path);
        assert!(
            summary.lines[1].text.contains(&format::bytes(task.size)),
            "{:?}",
            summary.lines[1].text
        );
        assert!(
            summary.lines[1]
                .text
                .contains("/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64"),
            "{:?}",
            summary.lines[1].text
        );
    }

    #[test]
    fn the_total_is_the_sum_of_the_deletable_sizes() {
        let ids = ["dbid_001", "dbid_002", "dbid_003"];
        let expected: u64 = ids.iter().map(|id| task(id).size).sum();

        let summary = summary(&ids);
        assert_eq!(summary.delete_count, 3);
        assert_eq!(summary.total_size, expected);
        assert!(expected > 0, "the fixture must have sizes to sum");
        assert!(
            summary.totals.contains(&format::bytes(expected)),
            "{:?}",
            summary.totals
        );
        assert!(summary.totals.contains("3 tasks"), "{:?}", summary.totals);
    }

    #[test]
    fn a_refused_item_is_reported_as_skipped_with_its_reason() {
        // dbid_013's files share no common root, so `delete.rs` refuses it. The
        // user must be able to see that, not have it quietly dropped.
        let summary = summary(&["dbid_001", "dbid_013"]);

        assert_eq!(summary.delete_count, 1);
        assert_eq!(summary.skipped_count, 1);
        assert!(summary.totals.contains("1 skipped"), "{:?}", summary.totals);

        let skipped = summary
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Skipped)
            .expect("a skipped line");
        assert!(skipped.text.contains(SKIP_MARKER), "{:?}", skipped.text);
        assert!(skipped.text.contains("Mixed.Root.Release"));

        let reason = summary
            .lines
            .iter()
            .find(|line| line.kind == LineKind::Reason)
            .expect("a reason line");
        assert!(
            reason.text.contains("no single top-level"),
            "{:?}",
            reason.text
        );
    }

    #[test]
    fn a_refused_items_bytes_are_excluded_from_the_total() {
        let refused = task("dbid_013");
        assert!(refused.size > 0, "the refused task must have a size");

        let summary = summary(&["dbid_001", "dbid_013"]);
        assert_eq!(summary.total_size, task("dbid_001").size);
        assert!(
            !summary.totals.contains(&format::bytes(refused.size)),
            "the refused size leaked into the totals: {:?}",
            summary.totals
        );
    }

    #[test]
    fn a_plan_of_nothing_but_refusals_promises_no_deletion() {
        let summary = summary(&["dbid_010", "dbid_011", "dbid_013"]);
        assert_eq!(summary.delete_count, 0);
        assert_eq!(summary.skipped_count, 3);
        assert_eq!(summary.total_size, 0);
        assert!(summary.totals.contains("Nothing"), "{:?}", summary.totals);
        assert!(summary.totals.contains("3 skipped"), "{:?}", summary.totals);
        assert!(summary.title.contains("0 tasks"), "{:?}", summary.title);
    }

    #[test]
    fn items_are_listed_in_snapshot_order_not_grouped_by_outcome() {
        // The dialog's rows have to map onto the rows the user selected.
        let summary = summary(&["dbid_013", "dbid_001"]);
        assert_eq!(summary.lines[0].kind, LineKind::Skipped);
        assert_eq!(summary.lines[2].kind, LineKind::Delete);
    }

    #[test]
    fn an_empty_plan_summarizes_to_nothing() {
        let summary = build_confirmation(&DeletePlan::default(), DeleteOptions::default());
        assert_eq!(summary.line_count(), 0);
        assert_eq!(summary.delete_count, 0);
        assert_eq!(summary.total_size, 0);
    }

    // ---- what the summary promises -----------------------------------------

    #[test]
    fn the_default_wording_says_the_files_go_too() {
        let summary = summary(&["dbid_001"]);
        assert!(summary.delete_files);
        assert!(!summary.dry_run);
        assert!(summary.effect.contains("files"), "{:?}", summary.effect);
        assert!(
            summary.effect.contains("cannot be undone"),
            "{:?}",
            summary.effect
        );
        assert!(summary.totals.contains("to free"), "{:?}", summary.totals);
        assert!(!summary.title.contains(DRY_RUN_MARKER));
    }

    #[test]
    fn no_delete_files_says_the_files_stay_and_promises_no_space_back() {
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        let summary = build_confirmation(&plan(&["dbid_001"]), options);

        assert!(summary.effect.contains("task only"), "{:?}", summary.effect);
        assert!(
            summary.effect.contains("left in place"),
            "{:?}",
            summary.effect
        );
        assert!(
            !summary.totals.contains("to free"),
            "nothing is freed when the files stay: {:?}",
            summary.totals
        );
        assert!(
            summary.totals.contains("left on disk"),
            "{:?}",
            summary.totals
        );
    }

    #[test]
    fn a_dry_run_is_labelled_in_the_title_and_in_the_effect() {
        let summary = build_confirmation(&plan(&["dbid_001"]), DeleteOptions::dry_run());
        assert!(summary.dry_run);
        assert!(
            summary.title.starts_with(DRY_RUN_MARKER),
            "{:?}",
            summary.title
        );
        assert!(
            summary.effect.starts_with("Dry run"),
            "{:?}",
            summary.effect
        );
        assert!(
            summary.effect.contains("nothing is deleted"),
            "{:?}",
            summary.effect
        );
        // ...and it still lists exactly what it *would* remove.
        assert!(body(&summary).contains("/downloads/Ubuntu.24.04.3.LTS.Desktop.amd64"));
    }

    #[test]
    fn one_task_reads_as_one_task() {
        assert!(summary(&["dbid_001"]).title.contains("Delete 1 task"));
        assert!(summary(&["dbid_001"]).totals.starts_with("1 task ·"));
        assert!(summary(&["dbid_001", "dbid_002"]).title.contains("2 tasks"));
    }

    // ---- geometry ----------------------------------------------------------

    #[test]
    fn a_modal_is_centred_and_never_larger_than_what_it_sits_in() {
        let area = Rect::new(0, 0, 100, 40);
        let modal = centred(area, 60, 20);
        assert_eq!(
            (modal.x, modal.y, modal.width, modal.height),
            (20, 10, 60, 20)
        );

        // A modal bigger than the terminal is clipped, not overflowed.
        let modal = centred(Rect::new(0, 0, 10, 4), 60, 20);
        assert_eq!((modal.width, modal.height), (10, 4));
        assert_eq!((modal.x, modal.y), (0, 0));
    }

    #[test]
    fn a_wrapped_effect_reports_at_least_one_row() {
        assert_eq!(wrapped_height("", 40), 1);
        assert_eq!(wrapped_height("short", 40), 1);
        assert_eq!(wrapped_height(&"x".repeat(41), 40), 2);
        assert_eq!(wrapped_height("anything", 0), 1);
    }
}
