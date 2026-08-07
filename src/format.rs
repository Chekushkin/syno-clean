//! Human-readable rendering of the numbers `model::Task` carries, plus the
//! display-width-correct truncation the task table depends on.
//!
//! Everything here is pure and total: no allocation-free promises, no panics,
//! no `unwrap`. These functions are called once per visible row per frame, so
//! they take the values [`crate::model::Task`] already derives
//! ([`progress`](crate::model::Task::progress),
//! [`ratio`](crate::model::Task::ratio), [`eta`](crate::model::Task::eta))
//! rather than re-deriving them.
//!
//! Two conventions worth knowing before reading the code:
//!
//! * **Sizes are binary (1 KiB = 1024 B)**, because that is what DSM reports
//!   and what Download Station's own UI shows. Byte counts render as integers
//!   at `B` and `KiB` and gain one decimal from `MiB` upward, where the extra
//!   digit is the difference between "4 GiB" and "4.7 GiB".
//! * **"Nothing here" and "unknown" are distinct.** A zero speed is [`DASH`]
//!   (there is a task, it is simply not moving) while an unknowable ETA is
//!   [`INFINITY`]. Rendering both as `0` would tell the user a paused task is
//!   about to finish.
//!
//! Truncation uses [`unicode_width`], **not** character counts. Torrent titles
//! are frequently CJK and increasingly contain emoji, both of which occupy two
//! terminal cells; truncating by `char` would overflow the Name column and
//! shear every column to its right out of alignment.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Shown where a value exists but is zero — an idle transfer rate, mostly.
/// One cell wide.
pub const DASH: &str = "—";

/// Shown where a value cannot be known: an ETA with no download speed to
/// divide by, or a ratio that is not a finite number.
pub const INFINITY: &str = "∞";

/// The truncation marker. One cell wide, which [`truncate_ellipsis`] relies on
/// only by measuring it rather than assuming it.
pub const ELLIPSIS: &str = "…";

/// The occupied run of a [`gauge`] bar. One cell wide — see [`gauge`] for why
/// substituting a wider glyph breaks the layout to the bar's right.
pub const GAUGE_FILLED: char = '█';

/// The free run of a [`gauge`] bar. One cell wide, for the same reason as
/// [`GAUGE_FILLED`].
pub const GAUGE_EMPTY: char = '░';

/// Binary size units, smallest first. `TiB` is the ceiling: a Download Station
/// task larger than 1024 TiB is not a case worth a unit for.
const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// Decimal places used at a given index into [`UNITS`].
///
/// `B` and `KiB` are whole numbers — a tenth of a KiB is 102 bytes, which is
/// noise. From `MiB` up the decimal carries real information.
fn decimals(unit: usize) -> usize {
    if unit >= 2 { 1 } else { 0 }
}

/// `value` as it will look once formatted with `decimals` places, used to
/// decide whether rounding has pushed it into the next unit.
fn rounded(value: f64, decimals: usize) -> f64 {
    let scale = if decimals == 0 { 1.0 } else { 10.0 };
    (value * scale).round() / scale
}

/// A byte count as `0 B`, `640 KiB`, `5.8 GiB`.
///
/// The unit is chosen *after* rounding, so a value that would render as
/// `1024.0 KiB` is promoted to `1.0 MiB` instead. Without that check the
/// largest displayable number in a unit is one the unit is not supposed to
/// reach, which looks like a bug every time anyone notices it.
pub fn bytes(n: u64) -> String {
    let mut value = n as f64;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && rounded(value, decimals(unit)) >= 1024.0 {
        value /= 1024.0;
        unit += 1;
    }
    let name = UNITS[unit];
    if decimals(unit) == 0 {
        format!("{value:.0} {name}")
    } else {
        format!("{value:.1} {name}")
    }
}

/// A transfer rate as `5.8 MiB/s`, or [`DASH`] when nothing is moving.
///
/// Most rows in a real task list are idle; printing `0 B/s` on every one of
/// them buries the handful that are actually transferring.
pub fn speed(n: u64) -> String {
    if n == 0 {
        return DASH.to_string();
    }
    format!("{}/s", bytes(n))
}

/// A duration in seconds as at most two units — `45s`, `14m 5s`, `2h 14m`,
/// `3d 4h` — or [`INFINITY`] when it is unknown.
///
/// `None` is the unknown case and comes straight from
/// [`Task::eta`](crate::model::Task::eta): a stalled task has no meaningful
/// completion time and must not be rendered as though it does. Two units is
/// deliberate; seconds of precision on a four-hour download is false detail
/// and costs column width.
pub fn duration(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return INFINITY.to_string();
    };
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m {}s", secs / MINUTE, secs % MINUTE)
    } else if secs < DAY {
        format!("{}h {}m", secs / HOUR, (secs % HOUR) / MINUTE)
    } else {
        format!("{}d {}h", secs / DAY, (secs % DAY) / HOUR)
    }
}

/// A completion **fraction** in `0.0..=1.0` as `38.9%`.
///
/// The input is a fraction, not an already-multiplied percentage, because that
/// is what [`Task::progress`](crate::model::Task::progress) returns. Values
/// outside the range are clamped and a non-finite value reads as `0.0%`: a
/// progress column is not the place to surface a NaN.
pub fn percent(fraction: f64) -> String {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    format!("{:.1}%", fraction * 100.0)
}

/// A fixed-width `████░░░░` bar body — exactly `width` cells, no brackets.
///
/// **Both glyphs are single-cell, and that is load-bearing.** The caller lays
/// out the rest of its line assuming the bar occupies precisely `width` cells,
/// so a two-cell replacement glyph would shear everything to its right — the
/// same property [`crate::ui::table::SELECTED_MARKER`] is asserted to have,
/// for the same reason. Keep [`GAUGE_FILLED`] and [`GAUGE_EMPTY`] single-cell.
///
/// `fraction` is a fraction in `0.0..=1.0`, matching [`percent`] rather than an
/// already-multiplied percentage; out-of-range values clamp and a non-finite
/// one reads as empty, because a `NaN` must not panic mid-frame. `width == 0`
/// is an empty string rather than a panic: a terminal too narrow for a bar is
/// a layout to degrade, not a reason to bring the program down.
///
/// The filled count is *rounded*, so a bar can read full a hair before 100%.
/// That errs toward "almost out of space", which is the direction this bar
/// exists to warn in, and the exact figure is [`percent`]'s job beside it.
pub fn gauge(fraction: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((fraction * width as f64).round() as usize).min(width);
    let mut bar = String::with_capacity(width * GAUGE_FILLED.len_utf8());
    for cell in 0..width {
        bar.push(if cell < filled {
            GAUGE_FILLED
        } else {
            GAUGE_EMPTY
        });
    }
    bar
}

/// A share ratio as `2.14`, or [`INFINITY`] when it is not a finite number.
///
/// [`Task::ratio`](crate::model::Task::ratio) already avoids dividing by zero,
/// so the non-finite branch guards callers that compute a ratio some other
/// way rather than a case the model can produce.
pub fn ratio(value: f64) -> String {
    if !value.is_finite() {
        return INFINITY.to_string();
    }
    format!("{:.2}", value.max(0.0))
}

/// Width of `text` in terminal cells.
///
/// This is the only correct way to size or pad a column in this program:
/// `str::len` counts bytes and `chars().count()` counts code points, and both
/// disagree with the terminal for every CJK character and most emoji.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// `text` shortened to at most `max_width` **terminal cells**, with [`ELLIPSIS`]
/// marking the cut.
///
/// The result is guaranteed to fit: it is never wider than `max_width`, though
/// it may come up one cell short when the cut lands on a double-width
/// character that cannot be half-printed. Text that already fits is returned
/// unchanged, and a `max_width` too small for the ellipsis itself yields an
/// empty string — there is no honest way to abbreviate into zero cells.
///
/// Truncation is per `char`, so a combining mark or ZWJ emoji sequence can in
/// principle be split. Handling that properly means a grapheme-segmentation
/// dependency; the failure mode is one cosmetically odd character at the cut
/// in a title that was being elided anyway.
pub fn truncate_ellipsis(text: &str, max_width: usize) -> String {
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let marker_width = display_width(ELLIPSIS);
    if max_width < marker_width {
        return String::new();
    }

    let budget = max_width - marker_width;
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > budget {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push_str(ELLIPSIS);
    out
}

/// Break `text` into lines no wider than `max_width` **terminal cells**,
/// preferring word boundaries.
///
/// Written because the alternative for a long line is truncation, and the one
/// string in this program that must never be truncated is a refusal reason: the
/// remedy it names (`--no-delete-files`) sits at the end of the sentence, so
/// clipping it removes exactly the part the user needs. ratatui's own `Wrap`
/// cannot be used where the wrapping has to be *counted* as well as drawn — the
/// modals scroll, and a scroll offset over rows nobody counted is a scroll that
/// stops in the wrong place.
///
/// A word longer than the whole width — a path with no spaces in it — is broken
/// mid-word rather than allowed to overflow. Continuation lines repeat the first
/// line's leading spaces, so a wrapped reason stays visually attached to the
/// item it belongs to.
///
/// Always returns at least one line, so a caller can count rows without a
/// special case for the empty string.
pub fn wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![String::new()];
    }
    if display_width(text) <= max_width {
        return vec![text.to_string()];
    }

    // The indent every row gets, dropped when it would leave less than half the
    // line for words. `split_whitespace` below discards the original leading
    // spaces, so the first row is re-indented exactly like the rest.
    let indent: String = text.chars().take_while(|c| *c == ' ').collect();
    let indent = if display_width(&indent) * 2 < max_width {
        indent
    } else {
        String::new()
    };
    // Positive: an empty indent leaves the whole width, and a kept one is by
    // construction narrower than half of it.
    let budget = max_width - display_width(&indent);

    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        for chunk in hard_break(word, budget) {
            let chunk_width = display_width(&chunk);
            if !current.is_empty() && used + 1 + chunk_width > budget {
                rows.push(std::mem::take(&mut current));
                used = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                used += 1;
            }
            current.push_str(&chunk);
            used += chunk_width;
        }
    }
    rows.push(current);

    rows.into_iter()
        .map(|row| format!("{indent}{row}"))
        .collect()
}

/// Split a single word that is wider than the line into pieces that fit.
///
/// Returns the word untouched when it already does, which is the common case.
fn hard_break(word: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || display_width(word) <= max_width {
        return vec![word.to_string()];
    }

    let mut pieces = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    for ch in word.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > max_width && !piece.is_empty() {
            pieces.push(std::mem::take(&mut piece));
            used = 0;
        }
        piece.push(ch);
        used += width;
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- bytes ------------------------------------------------------------

    #[test]
    fn bytes_renders_whole_numbers_below_mib_and_one_decimal_above() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1), "1 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1024), "1 KiB");
        assert_eq!(bytes(1536), "2 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.0 GiB");
        assert_eq!(bytes(1024 * 1024 * 1024 * 1024), "1.0 TiB");
    }

    #[test]
    fn bytes_rounds_to_the_displayed_precision() {
        // 1.5 KiB rounds to a whole 2 KiB; 6231819257 B is 5.8 GiB.
        assert_eq!(bytes(1024 + 512), "2 KiB");
        assert_eq!(bytes(1024 + 511), "1 KiB");
        assert_eq!(bytes(6_231_819_257), "5.8 GiB");
        assert_eq!(bytes(8_912_896), "8.5 MiB");
    }

    #[test]
    fn a_value_that_would_round_up_to_1024_is_promoted_to_the_next_unit() {
        // The point of the promotion check: never print "1024 KiB".
        assert_eq!(bytes(1024 * 1024 - 1), "1.0 MiB");
        assert_eq!(bytes(1024 * 1024 - 1024), "1023 KiB");
        // 1023.97 MiB formats as 1024.0 at one decimal, so it becomes 1.0 GiB.
        assert_eq!(bytes(1024 * 1024 * 1024 - 1), "1.0 GiB");
        assert_eq!(bytes(1024 * 1024 * 1024 - 40 * 1024 * 1024), "984.0 MiB");
    }

    #[test]
    fn bytes_saturates_at_tib_without_panicking() {
        // u64::MAX is 16 EiB; TiB is the last unit, so it simply grows.
        assert!(bytes(u64::MAX).ends_with(" TiB"));
        assert_eq!(bytes(2 * 1024 * 1024 * 1024 * 1024), "2.0 TiB");
    }

    // ---- speed ------------------------------------------------------------

    #[test]
    fn speed_marks_an_idle_transfer_with_a_dash_rather_than_zero() {
        assert_eq!(speed(0), DASH);
        assert_eq!(speed(1), "1 B/s");
        assert_eq!(speed(524_288), "512 KiB/s");
        assert_eq!(speed(8_912_896), "8.5 MiB/s");
    }

    // ---- duration ---------------------------------------------------------

    #[test]
    fn an_unknown_duration_is_infinity_not_zero() {
        assert_eq!(duration(None), INFINITY);
        // Zero seconds is a *known* duration and must not read as unknown.
        assert_eq!(duration(Some(0)), "0s");
    }

    #[test]
    fn duration_renders_at_most_two_units() {
        assert_eq!(duration(Some(1)), "1s");
        assert_eq!(duration(Some(59)), "59s");
        assert_eq!(duration(Some(60)), "1m 0s");
        assert_eq!(duration(Some(65)), "1m 5s");
        assert_eq!(duration(Some(3_599)), "59m 59s");
        assert_eq!(duration(Some(3_600)), "1h 0m");
        assert_eq!(duration(Some(8_040)), "2h 14m");
        assert_eq!(duration(Some(86_399)), "23h 59m");
        assert_eq!(duration(Some(86_400)), "1d 0h");
        assert_eq!(duration(Some(90_000)), "1d 1h");
        assert_eq!(duration(Some(1_000_000)), "11d 13h");
    }

    // ---- percent ----------------------------------------------------------

    #[test]
    fn percent_takes_a_fraction_and_renders_one_decimal() {
        assert_eq!(percent(0.0), "0.0%");
        assert_eq!(percent(0.389), "38.9%");
        assert_eq!(percent(0.5), "50.0%");
        assert_eq!(percent(1.0), "100.0%");
    }

    #[test]
    fn percent_clamps_out_of_range_and_non_finite_values() {
        assert_eq!(percent(1.5), "100.0%");
        assert_eq!(percent(-0.5), "0.0%");
        assert_eq!(percent(f64::NAN), "0.0%");
        assert_eq!(percent(f64::INFINITY), "0.0%");
    }

    // ---- ratio ------------------------------------------------------------

    #[test]
    fn ratio_renders_two_decimals() {
        assert_eq!(ratio(0.0), "0.00");
        assert_eq!(ratio(0.005), "0.01");
        assert_eq!(ratio(1.0), "1.00");
        assert_eq!(ratio(2.1408), "2.14");
        assert_eq!(ratio(12.3456), "12.35");
    }

    #[test]
    fn a_non_finite_ratio_is_infinity() {
        assert_eq!(ratio(f64::INFINITY), INFINITY);
        assert_eq!(ratio(f64::NAN), INFINITY);
        // Negative ratios are impossible from the model but must not print a
        // minus sign if one ever arrives.
        assert_eq!(ratio(-1.0), "0.00");
    }

    // ---- display width ----------------------------------------------------

    #[test]
    fn display_width_counts_cells_not_bytes_or_chars() {
        assert_eq!(display_width("abc"), 3);
        // Three chars, nine bytes, six cells.
        assert_eq!(display_width("千と千"), 6);
        assert_eq!("千と千".chars().count(), 3);
        assert_eq!("千と千".len(), 9);
        // The ellipsis and the sentinels are all single-cell.
        assert_eq!(display_width(ELLIPSIS), 1);
        assert_eq!(display_width(DASH), 1);
        assert_eq!(display_width(INFINITY), 1);
    }

    // ---- truncation -------------------------------------------------------

    /// The contract every truncation must satisfy.
    fn assert_fits(text: &str, max_width: usize) -> String {
        let out = truncate_ellipsis(text, max_width);
        assert!(
            display_width(&out) <= max_width,
            "{out:?} is {} cells, over the {max_width} allowed",
            display_width(&out)
        );
        out
    }

    #[test]
    fn text_that_already_fits_is_returned_unchanged() {
        assert_eq!(truncate_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_ellipsis("abc", 10), "abc");
        assert_eq!(truncate_ellipsis("", 0), "");
        assert_eq!(truncate_ellipsis("千と千", 6), "千と千");
    }

    #[test]
    fn ascii_truncation_leaves_room_for_the_ellipsis() {
        assert_eq!(assert_fits("abcdef", 5), "abcd…");
        assert_eq!(assert_fits("abcdef", 2), "a…");
        assert_eq!(assert_fits("abcdef", 1), "…");
    }

    #[test]
    fn a_width_too_small_for_the_ellipsis_yields_nothing() {
        assert_eq!(truncate_ellipsis("abcdef", 0), "");
        assert_eq!(truncate_ellipsis("千と千", 0), "");
    }

    #[test]
    fn cjk_truncation_counts_two_cells_per_character() {
        // Six cells of text into five: two chars (4 cells) plus the ellipsis.
        assert_eq!(assert_fits("千と千", 5), "千と…");
        // Four cells available: one char plus the ellipsis is 3, and the next
        // char would overflow, so the result is deliberately one cell short.
        assert_eq!(assert_fits("千と千", 4), "千…");
        assert_eq!(assert_fits("千と千", 3), "千…");
        assert_eq!(assert_fits("千と千", 2), "…");
    }

    #[test]
    fn an_emoji_is_two_cells_wide_and_never_half_printed() {
        assert_eq!(assert_fits("🐰🐰🐰", 5), "🐰🐰…");
        assert_eq!(assert_fits("🐰🐰🐰", 4), "🐰…");
        // Three cells cannot hold a two-cell emoji plus the marker.
        assert_eq!(assert_fits("🐰🐰🐰", 3), "🐰…");
        assert_eq!(assert_fits("ab🐰cd", 4), "ab…");
    }

    #[test]
    fn mixed_width_text_truncates_on_the_cell_boundary() {
        // "a千b千c" is 7 cells: 1 + 2 + 1 + 2 + 1.
        assert_eq!(display_width("a千b千c"), 7);
        assert_eq!(assert_fits("a千b千c", 4), "a千…");
        assert_eq!(assert_fits("a千b千c", 5), "a千b…");
        // At 6 the next character is double-width and will not fit in the one
        // remaining cell, so the result stops a cell short rather than
        // clipping it in half.
        assert_eq!(assert_fits("a千b千c", 6), "a千b…");
    }

    // ---- against the checked-in fixture -----------------------------------
    //
    // The fixture carries a CJK title and an emoji title precisely so the
    // width handling is exercised against data of the shape a real NAS sends,
    // not just against hand-picked strings.

    fn fixture_titles() -> Vec<String> {
        crate::testutil::fixture_tasks()
            .into_iter()
            .map(|t| t.title)
            .collect()
    }

    #[test]
    fn every_fixture_title_truncates_within_its_column_at_every_width() {
        let titles = fixture_titles();
        assert!(!titles.is_empty());
        for title in &titles {
            for width in 0..=48 {
                let out = truncate_ellipsis(title, width);
                assert!(
                    display_width(&out) <= width,
                    "{title:?} at width {width} produced {out:?} \
                     ({} cells)",
                    display_width(&out)
                );
            }
        }
    }

    #[test]
    fn the_fixtures_cjk_title_is_wider_than_its_character_count() {
        let title = fixture_titles()
            .into_iter()
            .find(|t| t.starts_with("千と千尋"))
            .expect("the fixture must keep a CJK title");
        assert!(
            display_width(&title) > title.chars().count(),
            "a char-count truncation would have looked safe here"
        );
        // Truncating to 20 cells keeps the recognizable head of the title.
        let out = truncate_ellipsis(&title, 20);
        assert_eq!(display_width(&out), 20);
        assert!(out.starts_with("千と千尋の神隠し"), "{out}");
        assert!(out.ends_with(ELLIPSIS), "{out}");
    }

    #[test]
    fn the_fixtures_emoji_title_truncates_without_splitting_the_emoji() {
        let title = fixture_titles()
            .into_iter()
            .find(|t| t.contains('🐰'))
            .expect("the fixture must keep an emoji title");
        // Cut exactly where the emoji would straddle the boundary.
        let head = "Big.Buck.Bunny.2008.1080p.";
        let upto_emoji = display_width(head);
        let out = truncate_ellipsis(&title, upto_emoji + 2);
        assert!(display_width(&out) <= upto_emoji + 2);
        assert!(
            !out.contains('🐰'),
            "{out} must not include a clipped emoji"
        );
        assert_eq!(
            truncate_ellipsis(&title, upto_emoji + 3),
            format!("{head}🐰…")
        );
    }

    // ---- wrap ---------------------------------------------------------------

    #[test]
    fn text_that_fits_is_returned_as_one_line_unchanged() {
        assert_eq!(wrap("short enough", 40), vec!["short enough".to_string()]);
        assert_eq!(wrap("", 40), vec![String::new()]);
        // A zero-width area still yields a countable row.
        assert_eq!(wrap("anything", 0), vec![String::new()]);
    }

    #[test]
    fn wrapping_breaks_on_words_and_never_exceeds_the_width() {
        let text = "nothing at /downloads/X, and that path was guessed from the task's title";
        for width in [10, 20, 31, 79] {
            let lines = wrap(text, width);
            for line in &lines {
                assert!(
                    display_width(line) <= width,
                    "{width}: {line:?} is {} cells",
                    display_width(line)
                );
            }
            // Nothing is lost and nothing is invented. Compared without any
            // whitespace, because a width narrow enough to break a word mid-way
            // legitimately turns one word into two.
            let squeeze =
                |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
            assert_eq!(squeeze(&lines.join("")), squeeze(text), "{width}");
        }
    }

    #[test]
    fn a_word_wider_than_the_line_is_broken_rather_than_overflowing() {
        // A path with no spaces in it is the realistic case, and it must not be
        // allowed to spill past the modal's border.
        let path = "/volume1/downloads/".to_string() + &"x".repeat(60);
        let lines = wrap(&path, 20);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(display_width(line) <= 20, "{line:?}");
        }
        assert_eq!(lines.concat(), path);
    }

    #[test]
    fn continuation_lines_keep_the_first_lines_indent() {
        let lines = wrap("    a reason that is much too long for this width", 20);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.starts_with("    "), "{line:?}");
            assert!(display_width(line) <= 20, "{line:?}");
        }

        // An indent that would swallow the line is dropped instead.
        let lines = wrap("          indented past the point of usefulness", 12);
        assert!(!lines[0].starts_with(' '), "{:?}", lines[0]);
    }

    #[test]
    fn wrapping_counts_cells_not_characters() {
        let lines = wrap("千と千尋の神隠し 2001", 8);
        for line in &lines {
            assert!(display_width(line) <= 8, "{line:?}");
        }
        assert!(lines.len() >= 3, "{lines:?}");
    }
}
