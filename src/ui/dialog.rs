//! Modal overlays: the delete confirmation and the `?` help.
//!
//! The help overlay ([`HELP_SECTIONS`], [`render_help`]) is **the** reference
//! for what the keyboard does, so it is data rather than a formatted blob — a
//! test walks the entries and asserts every key `App` binds appears in it, which
//! is the only thing that stops a new binding from being invisible.
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
//! * **Refused items are shown, never dropped.** While files are being deleted,
//!   a task the path resolver would not touch (`Target::Refused`) is listed as
//!   `SKIPPED` with the reason, and its bytes are left out of the total.
//!   Silently omitting it would let a user believe a torrent was cleaned up when
//!   it was not. Under `--no-delete-files` the same task is an ordinary
//!   deletable row — no path is used, so there is nothing to refuse — and the
//!   dialog says exactly that. Which it is comes from `delete::will_act`, the
//!   same rule the executor runs on, so the two cannot disagree.
//! * **The modal says what will actually happen.** With `delete_files = false`
//!   only the DSM task goes and the finished files stay; under `--dry-run`
//!   nothing goes at all. Both are stated in the title *and* in the effect line,
//!   because these are the cases where the user's mental model is most likely
//!   wrong. "Finished" is load-bearing: DSM deletes an *unfinished* task's
//!   partial data along with the task (`force_complete=false`), so the effect
//!   line, the row and the totals all say so rather than promising files that
//!   are about to go — see [`delete::payload_survives_task_delete`].
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
use crate::delete::{self, DeleteItem, DeleteOptions, DeletePlan};
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
    /// How many will be left entirely alone — refused items, and only while
    /// their refusal means something (see [`build_confirmation`]).
    pub skipped_count: usize,
    /// Bytes the acted-on items add up to; items nothing happens to are
    /// excluded.
    ///
    /// The *whole* of what is acted on. Under `--no-delete-files` the totals
    /// line reports a smaller "left on disk" figure, because DSM discards the
    /// partial data of the unfinished rows — see [`totals`].
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

/// The rows whose on-disk data does **not** survive what is about to happen,
/// even though no file delete is being issued for them.
///
/// Only ever non-zero under `--no-delete-files`: that mode removes the DSM task
/// with `force_complete=false`, and DSM discards the partial data of a task that
/// has not finished. With `delete_files = true` the files are going anyway and
/// the dialog already says so, so there is nothing extra to qualify.
#[derive(Debug, Clone, Copy, Default)]
struct Discarded {
    /// How many acted-on rows lose their data this way.
    count: usize,
    /// Their reported bytes, which must be kept out of the "left on disk"
    /// figure.
    size: u64,
}

impl Discarded {
    fn of(plan: &DeletePlan, options: DeleteOptions) -> Self {
        if options.delete_files {
            return Self::default();
        }
        plan.items
            .iter()
            .filter(|item| delete::will_act(item, options))
            .filter(|item| !delete::payload_survives_task_delete(&item.payload_state()))
            .fold(Self::default(), |acc, item| Self {
                count: acc.count + 1,
                size: acc.size + item.size,
            })
    }
}

/// Turn a snapshot into everything the modal needs to say.
///
/// The plan's items are listed in **snapshot order** rather than grouped, and
/// snapshot order is the **on-screen order** the plan was built from (see
/// `App::target_tasks`) — so the nth row of the dialog is the nth armed row of
/// the table, under any sort. That correspondence is the entire job of this
/// screen: a user checking that the right torrents are armed reads down the
/// list, and an order that did not match the table would make the check
/// worthless. Refused items keep their place in that order and are flagged
/// rather than hidden.
///
/// `options` is a parameter rather than a field of the plan because it is
/// session state, not per-task state; see [`DeleteOptions`].
pub fn build_confirmation(plan: &DeletePlan, options: DeleteOptions) -> ConfirmSummary {
    // What counts as skipped is `plan_delete_ops`' answer, never a second copy
    // of the rule: under `--no-delete-files` a task whose *path* could not be
    // resolved is deleted like any other, because no path is used, and a dialog
    // that still called it SKIPPED would be describing a different program.
    let acted_on = |item: &&DeleteItem| delete::will_act(item, options);
    let delete_count = plan.items.iter().filter(acted_on).count();
    let skipped_count = plan.len() - delete_count;
    let total_size = plan
        .items
        .iter()
        .filter(acted_on)
        .map(|item| item.size)
        .sum();

    // `--no-delete-files` deletes the DSM task with `force_complete=false`, so
    // DSM throws away the partial data of a task that has not finished. Those
    // rows cannot be told their files are left in place — see
    // `delete::payload_survives_task_delete`.
    let discarded = Discarded::of(plan, options);

    let mut lines = Vec::with_capacity(plan.len() * 2);
    for item in &plan.items {
        lines.extend(item_lines(item, options));
    }

    ConfirmSummary {
        title: title(delete_count, options),
        effect: effect(options, discarded.count > 0),
        lines,
        totals: totals(delete_count, skipped_count, total_size, discarded, options),
        delete_count,
        skipped_count,
        total_size,
        dry_run: options.dry_run,
        delete_files: options.delete_files,
    }
}

/// The two lines one item contributes: what it is, then what happens to it.
fn item_lines(item: &DeleteItem, options: DeleteOptions) -> [SummaryLine; 2] {
    let deleted = SummaryLine::new(LineKind::Delete, format!("{DELETE_MARKER} {}", item.title));

    match (item.path(), item.refusal()) {
        (Some(path), _) => [
            deleted,
            SummaryLine::new(
                LineKind::Path,
                format!("    {}  {path}", format::bytes(item.size)),
            ),
        ],
        // Refused, but nothing about to be deleted needs the path: the row goes
        // and — if the task finished — the files stay wherever they are. Said in
        // place of the path, because that is the line the user reads to check
        // the aim. An *unfinished* task gets the opposite sentence: DSM discards
        // its partial data along with the task, and promising otherwise here is
        // promising the user data that is about to go.
        (None, _) if !options.delete_files => [
            deleted,
            SummaryLine::new(
                LineKind::Path,
                format!(
                    "    {}  DSM task only — its on-disk location is unknown, and {}",
                    format::bytes(item.size),
                    if delete::payload_survives_task_delete(&item.payload_state()) {
                        "no file is touched"
                    } else {
                        "DSM discards its partial data"
                    }
                ),
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
///
/// `any_discarded` is the one qualification the task-only wording needs: DSM
/// deletes an unfinished task's partial data with the task
/// (`force_complete=false`), so "the files are left in place" is true of
/// finished tasks and false of the rest.
fn effect(options: DeleteOptions, any_discarded: bool) -> String {
    let scope = match (options.delete_files, any_discarded) {
        (true, _) => "the Download Station task and its files on the NAS",
        (false, false) => "the Download Station task only — the files on the NAS are left in place",
        (false, true) => {
            "the Download Station task only — finished files are left in place, but DSM \
             discards the partial data of any task that has not finished"
        }
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
    discarded: Discarded,
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
        // Only what DSM actually leaves behind is "left on disk": the partial
        // data of an unfinished task goes with the task, so its bytes are
        // reported as discarded rather than counted as kept.
        let mut line = format!(
            "{delete_count} {} · {} left on disk",
            tasks(delete_count),
            format::bytes(total_size.saturating_sub(discarded.size))
        );
        if discarded.count > 0 {
            // No byte figure: what DSM throws away is the *downloaded* part,
            // and the snapshot carries the task's total size, so any number
            // here would be the wrong one.
            line.push_str(&format!(
                " · {} unfinished, partial data discarded",
                discarded.count
            ));
        }
        line
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

// ---- the help overlay ------------------------------------------------------

/// Border title of the `?` overlay.
pub const HELP_TITLE: &str = "Keybindings";

/// Footer of the `?` overlay. The overlay binds nothing itself — **any** key
/// closes it — so this is the only instruction it needs.
pub const HELP_DISMISS: &str = "any key closes this help";

/// Cells between the two columns when both fit.
const HELP_COLUMN_GAP: u16 = 2;

/// Cells the border and its padding cost on either side.
const HELP_CHROME: u16 = 4;

/// Cells between a key and what it does.
const HELP_KEY_GAP: usize = 2;

/// One binding in the help overlay.
///
/// `keys` is a space-separated list of the keys that do the same thing (`↑ k`),
/// which is also how the cross-check test tokenizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpEntry {
    pub keys: &'static str,
    pub action: &'static str,
}

/// A titled group of bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpSection {
    pub title: &'static str,
    pub entries: &'static [HelpEntry],
}

impl HelpSection {
    /// Rows this section occupies: its title plus one row per binding.
    pub fn height(&self) -> usize {
        self.entries.len() + 1
    }
}

/// Every keybinding the program has, grouped as a user would look for them.
///
/// **The implementation is the source of truth**, so this table follows
/// `App::handle_normal_key`, `handle_search_key` and `handle_confirm_key`
/// rather than any prose — including the two places where the shipped
/// behaviour is deliberately not what the original plan sketched: `Enter` in
/// the confirmation presses the *focused* button (which starts on Cancel), and
/// `Enter` in the search box commits a query that has been matching live since
/// the first keystroke.
pub const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "Navigation",
        entries: &[
            HelpEntry {
                keys: "↑ k",
                action: "move up",
            },
            HelpEntry {
                keys: "↓ j",
                action: "move down",
            },
            HelpEntry {
                keys: "PgUp PgDn",
                action: "move a screenful",
            },
            HelpEntry {
                keys: "Home g",
                action: "first row",
            },
            HelpEntry {
                keys: "End G",
                action: "last row",
            },
        ],
    },
    HelpSection {
        title: "Selection",
        entries: &[
            HelpEntry {
                keys: "Space",
                action: "toggle this row",
            },
            HelpEntry {
                keys: "a",
                action: "(de)select visible rows",
            },
            HelpEntry {
                keys: "Esc",
                action: "clear the selection",
            },
        ],
    },
    HelpSection {
        title: "Actions",
        entries: &[
            HelpEntry {
                keys: "d",
                action: "delete (asks first)",
            },
            HelpEntry {
                keys: "p",
                action: "pause selection or row",
            },
            HelpEntry {
                keys: "u",
                action: "resume selection or row",
            },
            HelpEntry {
                keys: "r",
                action: "refresh now",
            },
        ],
    },
    HelpSection {
        title: "Sort, filter, search",
        entries: &[
            HelpEntry {
                keys: "s",
                action: "next sort column",
            },
            HelpEntry {
                keys: "S",
                action: "reverse the direction",
            },
            HelpEntry {
                keys: "f",
                action: "next status filter",
            },
            HelpEntry {
                keys: "/",
                action: "search titles",
            },
        ],
    },
    HelpSection {
        title: "Search box",
        entries: &[
            HelpEntry {
                keys: "Enter",
                action: "commit and close the box",
            },
            HelpEntry {
                keys: "Esc",
                action: "cancel, restore query",
            },
            HelpEntry {
                keys: "Backspace",
                action: "delete a character",
            },
        ],
    },
    HelpSection {
        title: "Confirmation dialog",
        entries: &[
            HelpEntry {
                keys: "y",
                action: "delete",
            },
            HelpEntry {
                keys: "n Esc q",
                action: "cancel",
            },
            HelpEntry {
                keys: "Enter",
                action: "press focused button",
            },
            HelpEntry {
                keys: "Tab ← → h l",
                action: "switch button",
            },
            HelpEntry {
                keys: "↑ ↓ k j",
                action: "scroll a line",
            },
            HelpEntry {
                keys: "PgUp PgDn",
                action: "scroll a page",
            },
            HelpEntry {
                keys: "Home End",
                action: "first / last line",
            },
        ],
    },
    HelpSection {
        title: "General",
        entries: &[
            HelpEntry {
                keys: "?",
                action: "this help",
            },
            HelpEntry {
                keys: "q",
                action: "quit",
            },
            HelpEntry {
                keys: "Ctrl-C",
                action: "quit from anywhere",
            },
        ],
    },
];

/// Split the sections into two columns of as near the same height as possible.
///
/// Pure, so the balance is testable without a frame: the whole table is far too
/// tall for one column on an ordinary terminal, and a lopsided split wastes the
/// height it was supposed to save.
pub fn split_columns(sections: &[HelpSection]) -> (&[HelpSection], &[HelpSection]) {
    if sections.len() < 2 {
        return (sections, &[]);
    }

    let mut best = (usize::MAX, 1);
    for split in 1..sections.len() {
        // The overlay is as tall as its *taller* column, so that — not the
        // difference between them — is what a split has to minimize.
        let tallest =
            column_rows(&sections[..split], true).max(column_rows(&sections[split..], true));
        if tallest < best.0 {
            best = (tallest, split);
        }
    }
    sections.split_at(best.1)
}

/// Rows a column of `sections` needs, with or without a blank line between
/// them.
///
/// The blank lines are the first thing dropped on a terminal too short for the
/// whole card: losing the last two bindings off the bottom is a worse trade
/// than a denser list, and 24 rows is an extremely ordinary terminal.
fn column_rows(sections: &[HelpSection], spaced: bool) -> usize {
    let content: usize = sections.iter().map(HelpSection::height).sum();
    if spaced {
        content + sections.len().saturating_sub(1)
    } else {
        content
    }
}

/// Cells the key column of `sections` occupies — the widest key in it.
///
/// One definition for both readers: [`column_width`] sizes the overlay from it
/// and [`column_lines`] pads to it, and two copies that disagreed would print
/// the actions out of alignment with the width that was reserved for them.
fn key_width(sections: &[HelpSection]) -> usize {
    sections
        .iter()
        .flat_map(|section| section.entries)
        .map(|entry| display_width(entry.keys))
        .max()
        .unwrap_or(0)
}

/// Cells a column of `sections` needs: the widest key, the gap, and the widest
/// action — measured at **display width**, since the key column holds arrows.
fn column_width(sections: &[HelpSection]) -> u16 {
    let keys = key_width(sections);
    let body = sections
        .iter()
        .flat_map(|section| {
            section
                .entries
                .iter()
                .map(move |entry| keys + HELP_KEY_GAP + display_width(entry.action))
        })
        .chain(sections.iter().map(|section| display_width(section.title)))
        .max()
        .unwrap_or(0);
    u16::try_from(body).unwrap_or(u16::MAX)
}

/// The rendered lines of one column: a styled title, then `keys  action` rows,
/// with a blank line between sections.
fn column_lines(sections: &[HelpSection], spaced: bool) -> Vec<Line<'static>> {
    let key_width = key_width(sections);

    let mut lines = Vec::with_capacity(column_rows(sections, spaced));
    for (index, section) in sections.iter().enumerate() {
        if spaced && index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for entry in section.entries {
            lines.push(Line::from(vec![
                Span::styled(
                    pad(entry.keys, key_width),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ".repeat(HELP_KEY_GAP)),
                Span::raw(entry.action),
            ]));
        }
    }
    lines
}

/// Pad `text` to `width` terminal cells. `str`'s own `{:width$}` counts
/// characters, which is not the same thing the moment an arrow shows up.
fn pad(text: &str, width: usize) -> String {
    format!(
        "{text}{}",
        " ".repeat(width.saturating_sub(display_width(text)))
    )
}

/// Draw the `?` overlay over whatever is on screen.
///
/// Two columns when the terminal is wide enough for both, one otherwise. Either
/// way the content is **clipped, never scrolled**: the overlay is dismissed by
/// any key, so there is no key left over to scroll it with, and a two-column
/// layout is what keeps it inside an ordinary terminal in the first place.
pub fn render_help(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let (left, right) = split_columns(HELP_SECTIONS);
    let two_up = !right.is_empty()
        && area.width >= column_width(left) + HELP_COLUMN_GAP + column_width(right) + HELP_CHROME;

    let content_width = if two_up {
        column_width(left) + HELP_COLUMN_GAP + column_width(right)
    } else {
        column_width(HELP_SECTIONS)
    };
    // Rows the body needs, with the blank lines between sections and without.
    let rows = |spaced: bool| -> u16 {
        let rows = if two_up {
            column_rows(left, spaced).max(column_rows(right, spaced))
        } else {
            column_rows(HELP_SECTIONS, spaced)
        };
        u16::try_from(rows).unwrap_or(u16::MAX)
    };
    // The body, the dismissal footer and the two border rows. A card that would
    // be clipped is tightened up first — every binding on screen beats an airy
    // layout with the last two scrolled into nowhere.
    let spaced = rows(true).saturating_add(3) <= area.height;
    let content_rows = rows(spaced);

    let width = content_width.saturating_add(HELP_CHROME).min(area.width);
    let height = content_rows.saturating_add(3).min(area.height);

    let modal = centred(area, width, height);
    frame.render_widget(Clear, modal);

    let block = Block::bordered()
        .title(format!(" {HELP_TITLE} "))
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::horizontal(1));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    if two_up {
        let [left_area, _, right_area] = Layout::horizontal([
            Constraint::Length(column_width(left)),
            Constraint::Length(HELP_COLUMN_GAP),
            Constraint::Min(0),
        ])
        .areas(body);
        frame.render_widget(Paragraph::new(column_lines(left, spaced)), left_area);
        frame.render_widget(Paragraph::new(column_lines(right, spaced)), right_area);
    } else {
        frame.render_widget(Paragraph::new(column_lines(HELP_SECTIONS, spaced)), body);
    }

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            HELP_DISMISS,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .centered(),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Task;
    use crate::testutil::fixture_task as task;

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
        // dbid_003 has finished, so its payload really is left in place.
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        let summary = build_confirmation(&plan(&["dbid_003"]), options);

        assert!(summary.effect.contains("task only"), "{:?}", summary.effect);
        assert!(
            summary.effect.contains("left in place"),
            "{:?}",
            summary.effect
        );
        assert!(
            !summary.effect.contains("partial data"),
            "a finished task loses nothing: {:?}",
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
        assert!(
            summary
                .totals
                .contains(&format::bytes(task("dbid_003").size)),
            "a finished task's bytes stay on disk: {:?}",
            summary.totals
        );
        assert!(
            !summary.totals.contains("discarded"),
            "{:?}",
            summary.totals
        );
    }

    #[test]
    fn no_delete_files_does_not_promise_an_unfinished_task_its_partial_data() {
        // The lie this guards against: `--no-delete-files` issues only
        // Op::DeleteTask, and `build_delete_params` sends force_complete=false,
        // which is DSM's "do NOT keep the uncompleted download files". dbid_001
        // is still downloading, so confirming throws its partial data away.
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        let downloading = task("dbid_001");
        assert!(!delete::payload_survives_task_delete(
            &delete::PayloadState::of_task(&downloading)
        ));

        let summary = build_confirmation(&plan(&["dbid_001"]), options);
        assert!(
            summary.effect.contains("partial data"),
            "the effect line must not promise the files stay: {:?}",
            summary.effect
        );
        assert!(
            summary
                .totals
                .contains("1 unfinished, partial data discarded"),
            "{:?}",
            summary.totals
        );
        // Its bytes are not "left on disk" — nothing of it is.
        assert!(
            summary.totals.contains(&format::bytes(0)),
            "{:?}",
            summary.totals
        );
        assert!(
            !summary.totals.contains(&format::bytes(downloading.size)),
            "an unfinished task's bytes are not left on disk: {:?}",
            summary.totals
        );
    }

    #[test]
    fn a_refused_unfinished_row_says_its_partial_data_goes_rather_than_stays() {
        // The per-row half. dbid_010 reports no destination, so the resolver
        // refuses it; under this flag it is still deleted as a DSM row — and it
        // has not finished, so its partial data goes with it. The row must not
        // describe that as "no file is touched".
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        let waiting = task("dbid_010");
        assert!(!delete::payload_survives_task_delete(
            &delete::PayloadState::of_task(&waiting)
        ));
        assert!(
            DeletePlan::snapshot([&waiting].into_iter()).items[0].is_refused(),
            "the fixture row must be one the resolver refuses"
        );

        let summary = build_confirmation(&plan(&["dbid_010"]), options);
        let body = body(&summary);
        assert!(body.contains("DSM discards its partial data"), "{body}");
        assert!(!body.contains("no file is touched"), "{body}");
    }

    #[test]
    fn a_refused_finished_row_still_says_no_file_is_touched() {
        // The other half of the same rule, so the qualification cannot creep
        // onto rows whose payload really does survive. dbid_013 is seeding —
        // complete — and its file list has no single root, so it is refused.
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        assert!(delete::payload_survives_task_delete(
            &delete::PayloadState::of_task(&task("dbid_013"))
        ));

        let summary = build_confirmation(&plan(&["dbid_013"]), options);
        assert!(
            body(&summary).contains("no file is touched"),
            "{}",
            body(&summary)
        );
        assert!(
            !summary.effect.contains("partial data"),
            "{:?}",
            summary.effect
        );
    }

    #[test]
    fn deleting_the_files_never_mentions_partial_data() {
        // With the files in scope the dialog already says they go; the
        // unfinished-task qualification is a `--no-delete-files` concern only.
        let summary = summary(&["dbid_001"]);
        assert!(
            !summary.effect.contains("partial data"),
            "{:?}",
            summary.effect
        );
        assert!(
            !summary.totals.contains("discarded"),
            "{:?}",
            summary.totals
        );
    }

    #[test]
    fn no_delete_files_lists_a_refused_item_as_deletable_because_no_path_is_used() {
        // The dialog and the executor must tell the same story. Under this flag
        // `plan_delete_ops` removes the DSM row of a task whose path could not
        // be resolved — those are the tasks the flag exists for — so calling it
        // SKIPPED here would be describing a different program.
        let options = DeleteOptions {
            delete_files: false,
            dry_run: false,
        };
        let ids = ["dbid_001", "dbid_013"];
        let summary = build_confirmation(&plan(&ids), options);

        assert_eq!(summary.delete_count, 2);
        assert_eq!(summary.skipped_count, 0);
        assert!(!summary.totals.contains("skipped"), "{:?}", summary.totals);
        assert!(
            !body(&summary).contains(SKIP_MARKER),
            "no row is skipped: {}",
            body(&summary)
        );
        // Its bytes stay on disk with everything else's, so they belong in the
        // "left on disk" figure.
        let expected: u64 = ids.iter().map(|id| task(id).size).sum();
        assert_eq!(summary.total_size, expected);
        // ...and the row still says its location is unknown, so nobody reads it
        // as "the files were dealt with".
        assert!(
            body(&summary).contains("on-disk location is unknown"),
            "{}",
            body(&summary)
        );
    }

    #[test]
    fn deleting_files_still_reports_a_refused_item_as_skipped() {
        // The other half of the same rule: with files in scope the refusal is
        // load-bearing again.
        let summary = summary(&["dbid_001", "dbid_013"]);
        assert_eq!((summary.delete_count, summary.skipped_count), (1, 1));
        assert!(body(&summary).contains(SKIP_MARKER));
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

    // ---- the help overlay --------------------------------------------------

    /// Every key token the overlay documents, `keys` split on whitespace.
    fn documented_keys() -> Vec<&'static str> {
        HELP_SECTIONS
            .iter()
            .flat_map(|section| section.entries)
            .flat_map(|entry| entry.keys.split_whitespace())
            .collect()
    }

    #[test]
    fn the_overlay_documents_every_key_the_app_binds() {
        // The overlay is the only place a user can learn the keymap, so a
        // binding missing from it is a binding that does not exist as far as
        // they are concerned. This list mirrors `App::handle_normal_key`,
        // `handle_search_key` and `handle_confirm_key` — add a key there,
        // add it here, and this test tells you which one you forgot.
        let documented = documented_keys();
        for key in [
            // Normal mode.
            "↑",
            "↓",
            "k",
            "j",
            "PgUp",
            "PgDn",
            "Home",
            "End",
            "g",
            "G",
            "Space",
            "a",
            "Esc",
            "d",
            "p",
            "u",
            "r",
            "s",
            "S",
            "f",
            "/",
            "?",
            "q",
            "Ctrl-C",
            // The search box.
            "Enter",
            "Backspace", // The confirmation.
            "y",
            "n",
            "Tab",
            "←",
            "→",
            "h",
            "l",
        ] {
            assert!(
                documented.contains(&key),
                "the help overlay never mentions {key:?}"
            );
        }
    }

    #[test]
    fn every_entry_is_populated_and_narrow_enough_to_lay_out() {
        // Two columns are what keep the overlay inside an ordinary terminal;
        // an over-long action silently forces the single-column fallback.
        for section in HELP_SECTIONS {
            assert!(!section.title.is_empty());
            assert!(!section.entries.is_empty(), "{}", section.title);
            for entry in section.entries {
                assert!(!entry.keys.is_empty(), "{}", section.title);
                assert!(!entry.action.is_empty(), "{}", entry.keys);
                assert!(
                    display_width(entry.action) <= 25,
                    "{:?} is too wide for a column",
                    entry.action
                );
            }
        }

        let (left, right) = split_columns(HELP_SECTIONS);
        let total = column_width(left) + HELP_COLUMN_GAP + column_width(right) + HELP_CHROME;
        assert!(total <= 80, "two columns need {total} cells");
    }

    #[test]
    fn the_columns_are_split_to_keep_the_card_short() {
        let (left, right) = split_columns(HELP_SECTIONS);
        assert!(!left.is_empty() && !right.is_empty());
        assert_eq!(left.len() + right.len(), HELP_SECTIONS.len());

        // No other contiguous split produces a shorter overlay.
        let tallest = column_rows(left, true).max(column_rows(right, true));
        for split in 1..HELP_SECTIONS.len() {
            let other = column_rows(&HELP_SECTIONS[..split], true)
                .max(column_rows(&HELP_SECTIONS[split..], true));
            assert!(tallest <= other, "split at {split} is shorter");
        }

        // Dropping the blank lines is what makes it fit a 24-row terminal.
        let tight = column_rows(left, false).max(column_rows(right, false));
        assert!(tight + 3 <= 24, "{tight} rows plus chrome");

        // Degenerate inputs must not panic or lose a section.
        assert_eq!(split_columns(&[]).0.len(), 0);
        assert_eq!(split_columns(&HELP_SECTIONS[..1]).0.len(), 1);
        assert!(split_columns(&HELP_SECTIONS[..1]).1.is_empty());
    }

    #[test]
    fn a_column_renders_one_row_per_binding_plus_its_title() {
        let (left, _) = split_columns(HELP_SECTIONS);
        for spaced in [true, false] {
            assert_eq!(
                column_lines(left, spaced).len(),
                column_rows(left, spaced),
                "spaced: {spaced}"
            );
        }

        // Keys are padded to a common width so the actions line up, and the
        // padding is measured in cells rather than characters.
        let lines = column_lines(&HELP_SECTIONS[..1], true);
        let widths: Vec<usize> = lines[1..]
            .iter()
            .map(|line| display_width(&line.spans[0].content))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}"
        );
    }

    #[test]
    fn padding_counts_cells_not_characters() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad("↑ k", 5), "↑ k  ");
        assert_eq!(pad("too long", 3), "too long");
    }
}
