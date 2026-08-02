//! The task table: the columns from the plan's Technical Details, laid out by
//! hand rather than by ratatui's `Table` widget.
//!
//! Doing the layout here is deliberate. The Name column has to truncate at the
//! correct **display width** — torrent titles are routinely CJK and emoji, both
//! of which occupy two terminal cells — and every other column has to be padded
//! to an exact cell count so the columns stay in line underneath it. Handing
//! that to a widget that measures differently would shear the whole table one
//! column to the right of the first wide character. So each cell is truncated
//! and padded with [`crate::format`] (which measures in cells, via
//! `unicode-width`) and the row is emitted as one pre-composed [`Line`].
//!
//! Three rules worth knowing:
//!
//! * **Name absorbs the slack.** Every other column has a fixed width; Name
//!   takes whatever is left over, down to [`MIN_NAME_WIDTH`]. Responsive column
//!   *dropping* is deliberately out of scope for v1 — on a terminal narrower
//!   than [`ideal_width`] the rightmost columns are simply clipped by the
//!   buffer, which is a cosmetic loss, not a broken frame.
//! * **The scroll offset is derived, never stored.** [`scroll_offset`] is a
//!   pure function of the cursor, the row count and the viewport height, so
//!   there is no second piece of state that can disagree with the cursor after
//!   a refresh reorders the list.
//! * **Rendering stays a pure function of `&App`.** Everything here reads;
//!   nothing writes back.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::format::{self, DASH, display_width, truncate_ellipsis};
use crate::model::{Task, TaskStatus};
use crate::view::View;

/// Which edge a cell's text is pushed against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// One table column: its header, its fixed width in cells, and its alignment.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    /// Header text. Matches [`crate::view::SortKey::label`] exactly where the
    /// column is sortable, so the sort marker can be placed by comparing
    /// labels rather than by a second mapping that could drift out of step.
    pub header: &'static str,
    /// Width in terminal cells. For [`NAME`] this is the *minimum*.
    pub width: usize,
    pub align: Align,
}

/// The columns, left to right, exactly as the plan specifies them:
/// `[sel] │ Name │ Status │ Size │ Progress │ ↓ Speed │ ↑ Speed │ Ratio │
/// Seeds/Peers │ ETA │ Destination`.
///
/// Fixed widths are sized to hold their own header *plus a sort marker* and the
/// widest value the formatter can produce, so nothing in a normal frame is
/// truncated: `Size` holds `1023.0 GiB`, `Progress` holds `Progress▲`, the
/// speed columns hold `1023.0 MiB/s`.
pub const COLUMNS: [Column; 11] = [
    // The selection marker. Headerless: a one-cell heading would be noise.
    Column {
        header: "",
        width: 1,
        align: Align::Left,
    },
    Column {
        header: "Name",
        width: MIN_NAME_WIDTH,
        align: Align::Left,
    },
    Column {
        header: "Status",
        width: 11,
        align: Align::Left,
    },
    Column {
        header: "Size",
        width: 10,
        align: Align::Right,
    },
    Column {
        header: "Progress",
        width: 9,
        align: Align::Right,
    },
    Column {
        header: "↓ Speed",
        width: 12,
        align: Align::Right,
    },
    Column {
        header: "↑ Speed",
        width: 12,
        align: Align::Right,
    },
    Column {
        header: "Ratio",
        width: 6,
        align: Align::Right,
    },
    Column {
        header: "Seeds/Peers",
        width: 11,
        align: Align::Right,
    },
    Column {
        header: "ETA",
        width: 8,
        align: Align::Right,
    },
    Column {
        header: "Destination",
        width: 16,
        align: Align::Left,
    },
];

/// Index of the selection-marker column.
pub const SELECTION: usize = 0;
/// Index of the flexible column — the one that absorbs the leftover width.
pub const NAME: usize = 1;
/// Index of the column that carries the per-status colour.
pub const STATUS: usize = 2;

/// Floor for the Name column. Below this a title is all ellipsis and the table
/// stops being worth rendering; the columns to the right get clipped instead.
pub const MIN_NAME_WIDTH: usize = 12;

/// Blank cells between columns.
pub const COLUMN_GAP: usize = 1;

/// What a selected row shows in the [`SELECTION`] column.
///
/// Exactly one cell wide (asserted by a test), because the column is one cell
/// wide and a two-cell glyph here would shear every column to its right.
pub const SELECTED_MARKER: &str = "✓";

/// The cursor row: reversed, so it reads as a selection in any colour scheme.
fn cursor_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// A selected row. Deliberately a *colour* change where the cursor is a
/// *reversal*, so the two read differently when they land on the same row —
/// "where I am" and "what is armed" are different questions.
fn selection_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Base style for a row, given whether it is selected and whether the cursor is
/// on it. The cursor is patched on last, so it always wins where they overlap.
pub fn row_style(selected: bool, cursor: bool) -> Style {
    let mut style = Style::default();
    if selected {
        style = style.patch(selection_style());
    }
    if cursor {
        style = style.patch(cursor_style());
    }
    style
}

/// The header row.
fn header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// Terminal width at which no column has to be clipped.
pub fn ideal_width() -> usize {
    COLUMNS.iter().map(|column| column.width).sum::<usize>() + gaps()
}

/// Total width taken by the gaps between columns.
fn gaps() -> usize {
    COLUMNS.len().saturating_sub(1) * COLUMN_GAP
}

/// Width of every column for a table `total` cells wide.
///
/// Every column but [`NAME`] keeps its declared width; Name takes the
/// remainder, never dropping below [`MIN_NAME_WIDTH`]. When even that does not
/// fit, the returned widths add up to more than `total` and the rightmost
/// columns fall off the edge of the buffer — see the module docs.
pub fn column_widths(total: usize) -> [usize; COLUMNS.len()] {
    let mut widths = [0usize; COLUMNS.len()];
    for (index, column) in COLUMNS.iter().enumerate() {
        widths[index] = column.width;
    }

    let fixed: usize = widths
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != NAME)
        .map(|(_, width)| *width)
        .sum();
    widths[NAME] = total.saturating_sub(fixed + gaps()).max(MIN_NAME_WIDTH);

    widths
}

/// The first visible row for a given cursor position.
///
/// Derived rather than stored: the window is the smallest one that contains the
/// cursor, and it never scrolls past the end of the list, so it cannot get out
/// of step with a cursor that a refresh moved. A cursor already inside the
/// first page leaves the offset at zero.
pub fn scroll_offset(cursor: usize, rows: usize, height: usize) -> usize {
    if rows == 0 || height == 0 {
        return 0;
    }
    // The offset that would put the cursor on the bottom row, capped so the
    // final page is full rather than trailing off into blank rows.
    cursor
        .saturating_sub(height - 1)
        .min(rows.saturating_sub(height))
}

/// The compact status word shown in the Status column.
///
/// Shorter than the DSM spelling for the two longest states —
/// `hash_checking` and `filehosting_waiting` would each cost the Name column
/// several cells to display in full — and verbatim for an unrecognized status,
/// which must never be silently renamed.
pub fn status_label(status: &TaskStatus) -> &str {
    match status {
        TaskStatus::Waiting => "waiting",
        TaskStatus::Downloading => "downloading",
        TaskStatus::Paused => "paused",
        TaskStatus::Finishing => "finishing",
        TaskStatus::Finished => "finished",
        TaskStatus::HashChecking => "checking",
        TaskStatus::Seeding => "seeding",
        TaskStatus::FilehostingWaiting => "hosting",
        TaskStatus::Extracting => "extracting",
        TaskStatus::Error => "error",
        TaskStatus::Unknown(raw) => raw,
    }
}

/// Colour for a status, applied to the Status cell only — colouring whole rows
/// would fight the cursor highlight and make a long list hard to read.
pub fn status_style(status: &TaskStatus) -> Style {
    let colour = match status {
        TaskStatus::Downloading => Color::Cyan,
        TaskStatus::Seeding => Color::Green,
        TaskStatus::Finished => Color::Blue,
        TaskStatus::Paused => Color::DarkGray,
        TaskStatus::Error => Color::Red,
        // Everything transient: waiting to start, finishing, checking,
        // extracting, queued at a file host.
        TaskStatus::Waiting
        | TaskStatus::Finishing
        | TaskStatus::HashChecking
        | TaskStatus::FilehostingWaiting
        | TaskStatus::Extracting => Color::Yellow,
        // A status this client does not know. Visibly odd on purpose.
        TaskStatus::Unknown(_) => Color::Magenta,
    };
    Style::default().fg(colour)
}

/// The text of every cell of one row, in column order.
///
/// Separated from rendering so the contents can be asserted without a backend:
/// this is where a wrong formatter or a swapped column shows up. `selected` is
/// passed in rather than read from the app because a `Task` does not know
/// whether it is selected — the selection set is keyed by ID and lives on
/// [`App`].
pub fn row_cells(task: &Task, selected: bool) -> [String; COLUMNS.len()] {
    let peers = if task.seeders == 0 && task.leechers == 0 {
        DASH.to_string()
    } else {
        format!("{}/{}", task.seeders, task.leechers)
    };
    let destination = if task.destination.is_empty() {
        DASH.to_string()
    } else {
        task.destination.clone()
    };

    [
        if selected {
            SELECTED_MARKER.to_string()
        } else {
            String::new()
        },
        task.title.clone(),
        status_label(&task.status).to_string(),
        format::bytes(task.size),
        format::percent(task.progress()),
        format::speed(task.download_speed),
        format::speed(task.upload_speed),
        format::ratio(task.ratio()),
        peers,
        format::duration(task.eta()),
        destination,
    ]
}

/// The header cells, with the sort marker on the active column.
///
/// The marker is appended without a space so the widest header plus its arrow
/// still fits the declared column width. [`crate::view::SortKey::Added`] has no
/// column of its own, so sorting by it simply shows no marker — the status bar
/// (Task 12) is where that state is spelled out.
pub fn header_cells(view: &View) -> [String; COLUMNS.len()] {
    let active = view.sort_key.label();
    let arrow = view.sort_dir.arrow();
    std::array::from_fn(|index| {
        let header = COLUMNS[index].header;
        if !header.is_empty() && header == active {
            format!("{header}{arrow}")
        } else {
            header.to_string()
        }
    })
}

/// `text` truncated and padded to exactly `width` cells.
fn cell(text: &str, width: usize, align: Align) -> String {
    let text = truncate_ellipsis(text, width);
    let padding = " ".repeat(width.saturating_sub(display_width(&text)));
    match align {
        Align::Left => format!("{text}{padding}"),
        Align::Right => format!("{padding}{text}"),
    }
}

/// Compose one row from its cells, styling `styled` cells individually.
fn compose(
    cells: &[String; COLUMNS.len()],
    widths: &[usize],
    style: impl Fn(usize) -> Style,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(COLUMNS.len() * 2);
    for (index, column) in COLUMNS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
        }
        spans.push(Span::styled(
            cell(&cells[index], widths[index], column.align),
            style(index),
        ));
    }
    Line::from(spans)
}

/// One task as a table row.
pub fn row_line(task: &Task, selected: bool, widths: &[usize]) -> Line<'static> {
    compose(&row_cells(task, selected), widths, |index| {
        if index == STATUS {
            status_style(&task.status)
        } else {
            Style::default()
        }
    })
}

/// The header row, marked with the active sort column and direction.
pub fn header_line(view: &View, widths: &[usize]) -> Line<'static> {
    compose(&header_cells(view), widths, |_| Style::default()).style(header_style())
}

/// Draw the table into `area`: one header row, then as many task rows as fit.
///
/// The caller ([`crate::ui::render`]) handles the empty case, so `area` is only
/// ever given a non-empty visible list here.
pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let widths = column_widths(area.width as usize);
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    frame.render_widget(Paragraph::new(header_line(&app.view, &widths)), header_area);

    let visible = app.visible();
    let height = body_area.height as usize;
    let offset = scroll_offset(app.cursor, visible.len(), height);
    let rows: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(row, &index)| {
            let task = &app.tasks[index];
            let selected = app.is_selected(&task.id);
            row_line(task, selected, &widths).style(row_style(selected, row == app.cursor))
        })
        .collect();

    frame.render_widget(Paragraph::new(Text::from(rows)), body_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::parse_envelope;
    use crate::model::TaskList;
    use crate::view::{SortDir, SortKey};

    const FIXTURE: &str = include_str!("../../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        parse_envelope::<TaskList>(FIXTURE, "SYNO.DownloadStation.Task")
            .expect("the fixture must parse")
            .tasks
    }

    fn task(id: &str) -> Task {
        fixture_tasks()
            .into_iter()
            .find(|t| t.id == id)
            .unwrap_or_else(|| panic!("fixture has no task {id}"))
    }

    // ---- column widths ----------------------------------------------------

    #[test]
    fn name_absorbs_every_spare_cell() {
        let widths = column_widths(200);
        let others: usize = widths
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != NAME)
            .map(|(_, width)| *width)
            .sum();
        assert_eq!(others + widths[NAME] + gaps(), 200);
        assert!(widths[NAME] > MIN_NAME_WIDTH);
    }

    #[test]
    fn every_fixed_column_keeps_its_width_at_any_terminal_size() {
        for total in [1, 20, 80, ideal_width(), 300] {
            let widths = column_widths(total);
            for (index, column) in COLUMNS.iter().enumerate() {
                if index != NAME {
                    assert_eq!(widths[index], column.width, "{} at {total}", column.header);
                }
            }
        }
    }

    #[test]
    fn name_never_shrinks_below_its_floor() {
        // Narrower than the table needs: the rightmost columns are clipped by
        // the buffer rather than the title being squeezed into nothing.
        for total in [0, 1, 40, 80] {
            assert_eq!(column_widths(total)[NAME], MIN_NAME_WIDTH, "{total}");
        }
    }

    #[test]
    fn the_ideal_width_is_the_narrowest_terminal_that_clips_nothing() {
        let widths = column_widths(ideal_width());
        assert_eq!(widths[NAME], MIN_NAME_WIDTH);
        assert_eq!(widths.iter().sum::<usize>() + gaps(), ideal_width());
    }

    // ---- scroll offset ----------------------------------------------------

    #[test]
    fn a_list_that_fits_is_never_scrolled() {
        for cursor in 0..10 {
            assert_eq!(scroll_offset(cursor, 10, 10), 0, "{cursor}");
            assert_eq!(scroll_offset(cursor, 10, 40), 0, "{cursor}");
        }
    }

    #[test]
    fn scrolling_keeps_the_cursor_on_screen() {
        let (rows, height) = (100, 10);
        for cursor in 0..rows {
            let offset = scroll_offset(cursor, rows, height);
            assert!(offset <= cursor, "cursor {cursor} scrolled off the top");
            assert!(
                cursor < offset + height,
                "cursor {cursor} scrolled off the bottom (offset {offset})"
            );
        }
    }

    #[test]
    fn the_scroll_offset_never_runs_past_the_end_of_the_list() {
        // The window puts the cursor on the bottom row...
        assert_eq!(scroll_offset(89, 100, 10), 80);
        assert_eq!(scroll_offset(95, 100, 10), 86);
        // ...but never scrolls past a full last page: 100 rows in a 10-row
        // window tops out at 90, so the frame is never padded with blanks.
        assert_eq!(scroll_offset(99, 100, 10), 90);
        assert_eq!(scroll_offset(100, 100, 10), 90);
    }

    #[test]
    fn a_degenerate_viewport_or_list_scrolls_to_zero() {
        assert_eq!(scroll_offset(0, 0, 10), 0);
        assert_eq!(scroll_offset(5, 0, 10), 0);
        assert_eq!(scroll_offset(5, 10, 0), 0);
        // A cursor somehow past the end still yields a valid offset.
        assert_eq!(scroll_offset(500, 10, 4), 6);
    }

    // ---- cell contents ----------------------------------------------------

    #[test]
    fn a_row_reads_the_formatted_values_of_its_task() {
        let cells = row_cells(&task("dbid_001"), false);
        assert_eq!(cells[SELECTION], "");
        assert_eq!(cells[NAME], "Ubuntu.24.04.3.LTS.Desktop.amd64");
        assert_eq!(cells[STATUS], "downloading");
        assert_eq!(cells[3], format::bytes(6_231_819_257));
        assert_eq!(cells[4], "39.0%");
        assert_eq!(cells[5], "8.5 MiB/s");
        assert_eq!(cells[6], "512 KiB/s");
        assert_eq!(cells[8], "12/4");
        assert_eq!(cells[10], "downloads");
    }

    #[test]
    fn an_idle_task_shows_sentinels_rather_than_zeroes() {
        let cells = row_cells(&task("dbid_004"), false);
        assert_eq!(cells[STATUS], "paused");
        assert_eq!(cells[5], DASH, "no download speed");
        assert_eq!(cells[6], DASH, "no upload speed");
        assert_eq!(cells[9], format::INFINITY, "a paused task has no ETA");
    }

    #[test]
    fn a_task_with_no_destination_shows_a_dash_not_an_empty_cell() {
        // dbid_010 has no `additional` block at all.
        let cells = row_cells(&task("dbid_010"), false);
        assert_eq!(cells[10], DASH);
        assert_eq!(cells[8], DASH, "no peers either");
    }

    #[test]
    fn the_long_dsm_statuses_are_shortened_but_an_unknown_one_is_verbatim() {
        assert_eq!(status_label(&TaskStatus::HashChecking), "checking");
        assert_eq!(status_label(&TaskStatus::FilehostingWaiting), "hosting");
        assert_eq!(
            status_label(&TaskStatus::Unknown("captcha_needed".into())),
            "captcha_needed"
        );
        // Every label fits its column, so no status is ever elided.
        for status in TaskStatus::KNOWN {
            assert!(
                display_width(status_label(&status)) <= COLUMNS[STATUS].width,
                "{status} does not fit"
            );
        }
    }

    #[test]
    fn every_status_gets_its_own_readable_colour() {
        // Not a palette assertion — just that the states a user scans for are
        // told apart at a glance.
        assert_ne!(
            status_style(&TaskStatus::Error),
            status_style(&TaskStatus::Finished)
        );
        assert_ne!(
            status_style(&TaskStatus::Downloading),
            status_style(&TaskStatus::Seeding)
        );
        assert_ne!(
            status_style(&TaskStatus::Paused),
            status_style(&TaskStatus::Downloading)
        );
    }

    // ---- selection ---------------------------------------------------------

    #[test]
    fn a_selected_row_carries_the_marker_and_an_unselected_one_does_not() {
        assert_eq!(
            row_cells(&task("dbid_001"), true)[SELECTION],
            SELECTED_MARKER
        );
        assert_eq!(row_cells(&task("dbid_001"), false)[SELECTION], "");
    }

    #[test]
    fn the_selection_marker_is_exactly_one_cell_wide() {
        // A two-cell glyph here would push every column to its right out of
        // line on selected rows only — the nastiest possible layout bug.
        assert_eq!(
            display_width(SELECTED_MARKER),
            COLUMNS[SELECTION].width,
            "{SELECTED_MARKER:?}"
        );
    }

    #[test]
    fn selection_and_the_cursor_are_told_apart_in_every_combination() {
        let plain = row_style(false, false);
        let selected = row_style(true, false);
        let cursor = row_style(false, true);
        let both = row_style(true, true);
        for (a, b) in [
            (plain, selected),
            (plain, cursor),
            (plain, both),
            (selected, cursor),
            (selected, both),
            (cursor, both),
        ] {
            assert_ne!(a, b, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn the_cursor_highlight_survives_landing_on_a_selected_row() {
        // Patched on last, so the reversal is never lost to the selection
        // colour: the user must always be able to see where they are.
        assert!(
            row_style(true, true)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    // ---- padding and truncation -------------------------------------------

    #[test]
    fn every_cell_is_padded_to_exactly_its_column_width() {
        let widths = column_widths(120);
        for task in fixture_tasks() {
            // Selected and not: the marker must not widen its column either.
            for selected in [false, true] {
                let cells = row_cells(&task, selected);
                for (index, column) in COLUMNS.iter().enumerate() {
                    let rendered = cell(&cells[index], widths[index], column.align);
                    assert_eq!(
                        display_width(&rendered),
                        widths[index],
                        "column {} of {} (selected={selected}): {rendered:?}",
                        column.header,
                        task.id
                    );
                }
            }
        }
    }

    #[test]
    fn every_column_lines_up_at_every_terminal_width() {
        // The stronger form of the test above, and the one that pins the whole
        // point of measuring in display width: a *composed* row, at widths
        // narrow enough that the CJK and emoji titles are being truncated
        // mid-title. Every span — cell and gap alike — is exactly its declared
        // width, so column N begins in the same screen column on every row
        // whatever the title is made of. A char-count truncation passes the
        // 120-cell case above and fails here.
        for total in [60usize, 80, 100, 120, 160, 200] {
            let widths = column_widths(total);
            let expected: Vec<usize> = (0..COLUMNS.len())
                .flat_map(|index| {
                    if index == 0 {
                        vec![widths[0]]
                    } else {
                        vec![COLUMN_GAP, widths[index]]
                    }
                })
                .collect();
            for task in fixture_tasks() {
                for selected in [false, true] {
                    let spans: Vec<usize> = row_line(&task, selected, &widths)
                        .spans
                        .iter()
                        .map(|span| display_width(&span.content))
                        .collect();
                    assert_eq!(spans, expected, "{} at width {total}", task.id);
                }
                let spans: Vec<usize> = header_line(&View::default(), &widths)
                    .spans
                    .iter()
                    .map(|span| display_width(&span.content))
                    .collect();
                assert_eq!(spans, expected, "the header at width {total}");
            }
        }
    }

    #[test]
    fn a_cjk_title_is_truncated_by_cell_width_not_character_count() {
        let title = &task("dbid_006").title;
        assert!(display_width(title) > title.chars().count());
        let rendered = cell(title, 20, Align::Left);
        assert_eq!(display_width(&rendered), 20);
        assert!(rendered.ends_with(format::ELLIPSIS), "{rendered:?}");
    }

    #[test]
    fn numeric_cells_are_right_aligned_and_text_cells_left() {
        assert_eq!(cell("5.8 GiB", 10, Align::Right), "   5.8 GiB");
        assert_eq!(cell("downloads", 12, Align::Left), "downloads   ");
    }

    // ---- header -----------------------------------------------------------

    #[test]
    fn the_header_marks_the_active_sort_column_and_direction() {
        let mut view = View::default();
        assert_eq!(header_cells(&view)[NAME], "Name▲");
        view.toggle_dir();
        assert_eq!(header_cells(&view)[NAME], "Name▼");
        assert_eq!(header_cells(&view)[STATUS], "Status", "only one marker");
    }

    #[test]
    fn every_sortable_column_can_carry_the_marker_within_its_width() {
        let widths = column_widths(ideal_width());
        for key in SortKey::ALL {
            let view = View {
                sort_key: key,
                ..View::default()
            };
            let cells = header_cells(&view);
            for (index, column) in COLUMNS.iter().enumerate() {
                assert!(
                    display_width(&cells[index]) <= widths[index],
                    "{} does not fit its marker for {key:?}",
                    column.header
                );
            }
            let marked = cells.iter().filter(|c| c.ends_with('▲')).count();
            // `Added` is a sort key with no column of its own.
            let expected = usize::from(key != SortKey::Added);
            assert_eq!(marked, expected, "{key:?}");
        }
    }

    #[test]
    fn the_column_headers_match_the_sort_key_labels() {
        // The marker is placed by comparing labels, so a rename on either side
        // must show up as a compile-independent failure here.
        for key in SortKey::ALL {
            if key == SortKey::Added {
                continue;
            }
            assert!(
                COLUMNS.iter().any(|column| column.header == key.label()),
                "no column for {key:?}"
            );
        }
    }

    #[test]
    fn a_descending_sort_is_marked_differently_from_an_ascending_one() {
        let asc = View {
            sort_key: SortKey::Size,
            sort_dir: SortDir::Asc,
            ..View::default()
        };
        let desc = View {
            sort_dir: SortDir::Desc,
            ..asc.clone()
        };
        assert_ne!(header_cells(&asc)[3], header_cells(&desc)[3]);
    }
}
