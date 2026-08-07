//! Application state and key handling.
//!
//! [`App`] holds *everything* the program knows; rendering (`crate::ui`) is a
//! pure function of `&App`, and every key press is a `&mut App` transition. No
//! widget reads the network, and no networking code touches a widget — the
//! poller and the op tasks in [`crate::event`] hand their results to `App` as
//! plain data through [`App::apply_event`], which is a `&mut App` transition
//! exactly like a key press and just as testable without a runtime.
//!
//! Two conventions set here and relied on by later tasks:
//!
//! * **The task list is never reordered or cloned for display.** What is
//!   visible, and in what order, comes from [`crate::view::visible_indices`] as
//!   a `Vec<usize>` into [`App::tasks`].
//! * **Selection is keyed by task ID, not row index**, so a refresh that
//!   reorders or removes rows can never silently reassign what is selected.
//!   [`App::cursor`] is a position in the *visible* list and is reconciled by
//!   ID in [`App::apply_tasks`].

use std::collections::HashSet;
use std::path::Path;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::api::client::parse_envelope;
use crate::api::download_station::DS_TASK_API;
use crate::delete::{DeleteOptions, DeletePlan};
use crate::error::Result;
use crate::event::{AppEvent, ItemReport, OpKind, TaskOp, TaskRef};
use crate::model::{Task, TaskList};
use crate::ui::{dialog, table};
use crate::view::{self, View};

/// Rows a `PageUp`/`PageDown` moves before the first frame has been drawn.
///
/// The real page is the height of the table body and is pushed in by the event
/// loop after each draw ([`App::set_page_size`]); this is only what the very
/// first key press uses.
pub const DEFAULT_PAGE_SIZE: usize = 20;

/// What the UI is currently doing, and therefore which keys mean what.
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
    /// The `?` help overlay is open. It binds nothing: **any** key closes it.
    Help,
    /// The results of the last finished batch are on screen.
    ///
    /// Unlike [`Mode::Help`] this one *is* scrollable, so it does not close on
    /// any key — a `j` has to be able to mean "next line". It changes nothing
    /// and blocks nothing: refreshes keep arriving underneath it, because the
    /// report is a record of what already happened rather than a snapshot the
    /// list could invalidate.
    Results,
}

/// Which button of the confirmation modal `Enter` will press.
///
/// **The default is [`ConfirmFocus::Cancel`], and that is the whole point.** A
/// dialog that opens with the destructive button primed turns a reflexive
/// `Enter` into a recursive delete. `y` remains the one-key confirm for a user
/// who means it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ConfirmFocus {
    #[default]
    Cancel,
    Delete,
}

impl ConfirmFocus {
    /// The other button.
    fn other(self) -> Self {
        match self {
            ConfirmFocus::Cancel => ConfirmFocus::Delete,
            ConfirmFocus::Delete => ConfirmFocus::Cancel,
        }
    }
}

/// A pause or resume the user asked for, waiting for the event loop to run it.
///
/// Carries **owned task ids and titles**, resolved from the selection (or the
/// cursor row) at the moment the key was pressed — the same reason the
/// selection set itself holds IDs: a refresh that reorders the table between
/// the key press and the call must not move the operation onto a different
/// torrent. The title rides along so the progress line can name the torrent
/// instead of `dbid_042`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOpRequest {
    /// A delete goes through the confirmation dialog instead, which is why
    /// [`TaskOp`] has no variant for one.
    pub op: TaskOp,
    pub tasks: Vec<TaskRef>,
}

/// What one finished batch did, kept so the user can still read it afterwards.
///
/// **The counts alone are not a report.** `⚠ delete finished: 3 succeeded, 2
/// failed` names neither of the two, and the per-item lines that did name them
/// went to the footer, where each overwrote the last and the summary overwrote
/// them all. The reasons are also the longest strings the program produces — the
/// refusals that name `--no-delete-files` run past 200 characters — so a
/// one-line footer could not have shown one whole even if it had survived.
///
/// Successes are **not** kept: a batch of twenty that worked has nothing to
/// report, and listing them would bury the two that did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpReport {
    pub op: OpKind,
    pub succeeded: usize,
    pub skipped: usize,
    pub failed: usize,
    /// Every item that failed or was skipped, in batch order — which is the
    /// order the confirmation dialog listed them in.
    pub problems: Vec<ItemReport>,
}

impl OpReport {
    /// Whether there is anything worth opening the modal for.
    pub fn has_problems(&self) -> bool {
        !self.problems.is_empty()
    }

    /// Body lines the modal renders: a heading and a reason per problem.
    pub fn line_count(&self) -> usize {
        self.problems.len() * 2
    }
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
    /// First visible table row.
    ///
    /// Stored, because scrolling is edge-triggered: the window follows the
    /// cursor only when the cursor would leave it, and a window derived from
    /// the cursor alone can do nothing but pin the cursor to one row of the
    /// viewport. It is never trusted raw — [`App::scroll_offset`] re-clamps it
    /// against the live cursor, row count and viewport height on every read, so
    /// a value left over from a longer list or a moved cursor corrects itself.
    scroll: usize,
    /// Task IDs the user has selected. IDs rather than rows, deliberately.
    pub selected: HashSet<String>,
    pub mode: Mode,
    /// What a confirmed delete is allowed to do — the resolved `delete_files`
    /// and `dry_run` settings. The confirmation modal states both.
    pub delete_options: DeleteOptions,
    /// One line of feedback shown in the footer: the result of the last
    /// operation.
    ///
    /// **Transient by nature — do not seed it at startup.** The footer shows
    /// this *instead of* the key hints, and a message nothing ever clears hides
    /// the keymap for the whole session, which is how the one affordance that
    /// teaches the program disappears. Standing context belongs in
    /// [`Self::connection`], which the title bar renders alongside the hints
    /// rather than on top of them.
    pub status_message: Option<String>,
    /// Where this session is connected, as `user@host:port`.
    ///
    /// Rendered in the title bar: true for the whole run, so it earns permanent
    /// space, and it is the first thing to check when the numbers look wrong.
    /// `None` in `--fixture` mode, which is connected to nothing.
    pub connection: Option<String>,
    /// Whether a task list has **ever** arrived.
    ///
    /// `tasks.is_empty()` alone cannot tell "the NAS has nothing queued" from
    /// "the first poll has not come back yet" — or, worse, from "every poll so
    /// far has failed", where the empty state would assert nothing is queued
    /// directly underneath a red banner saying the NAS is unreachable.
    pub loaded: bool,
    /// A **non-fatal** failure banner — a poll that could not reach the NAS,
    /// most often. Kept apart from [`App::status_message`] so it can be styled
    /// as a warning and, crucially, cleared automatically by the next
    /// successful refresh: the UI recovers on its own.
    pub error: Option<String>,
    /// How far `PageUp`/`PageDown` jump: the height of the table body, as of
    /// the last frame. See [`DEFAULT_PAGE_SIZE`].
    page_size: usize,
    /// Set by `r`, cleared by the event loop when it forwards the request to
    /// the poller. A flag rather than a channel handle so `App` stays free of
    /// the runtime and every key press stays a pure state transition.
    refresh_requested: bool,
    /// The query [`View::search`] held when `/` was pressed, so `Esc` can put
    /// it back. `Some` exactly while [`Mode::Search`] is active: the search box
    /// edits the live query (the table narrows as the user types), so the only
    /// way to undo an abandoned edit is to have kept the original.
    search_backup: Option<String>,
    /// The snapshot the confirmation modal is showing. `Some` exactly while
    /// [`Mode::Confirm`] is active, and owned — see [`DeletePlan`]: what the
    /// user reads is what would be deleted, however the task list moves
    /// underneath.
    pending_delete: Option<DeletePlan>,
    /// Which modal button `Enter` presses. Reset to
    /// [`ConfirmFocus::Cancel`] every time the dialog opens.
    confirm_focus: ConfirmFocus,
    /// First body line the modal shows, for a plan longer than the modal is
    /// tall. Clamped against the line count here and against the *height* at
    /// render time, the same split the table uses.
    confirm_scroll: usize,
    /// A pause or resume the user asked for, waiting to be picked up.
    ///
    /// The same request/take shape as `r` and the confirmed delete: `p` and `u`
    /// record an intent and [`App::take_requested_op`] hands it to the event
    /// loop, so no key press touches the network.
    requested_op: Option<TaskOpRequest>,
    /// A plan the user confirmed, waiting to be picked up.
    ///
    /// **The dialog performs no I/O.** Confirming records the intent here and
    /// the event loop drains it with [`App::take_confirmed_delete`], which
    /// hands it to [`crate::event::spawn_delete`] — the owner of the actual
    /// three-phase execution. Keeping it a value
    /// means the whole confirmation flow stays testable without a runtime, a
    /// client or a NAS.
    confirmed_delete: Option<DeletePlan>,
    /// Problems reported by the batch **currently running**, accumulated from
    /// [`AppEvent::OpProgress`] and folded into an [`OpReport`] when the
    /// matching `OpDone` arrives. Cleared by the first item of the next batch.
    op_problems: Vec<ItemReport>,
    /// The last finished batch's report. Kept after the modal is dismissed so
    /// `v` can bring it back — the footer line naming the counts is gone by the
    /// next refresh, and the reasons were never in it.
    op_report: Option<OpReport>,
    /// First body line the results modal shows. Same split as the confirmation
    /// modal: clamped against the line count here, against the height at render
    /// time.
    results_scroll: usize,
    /// Whether an operation batch the event loop started is still running.
    ///
    /// Pushed in by [`App::set_op_in_flight`] before every draw — `App` owns no
    /// runtime handle and cannot ask. It exists so `d`, `p` and `u` can say no
    /// **before** the user commits: refusing after
    /// [`App::take_confirmed_delete`] had already taken the plan discarded it,
    /// and the only notice was a footer line the running batch's next progress
    /// event overwrote milliseconds later.
    op_in_flight: bool,
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
            scroll: 0,
            selected: HashSet::new(),
            mode: Mode::Normal,
            delete_options: DeleteOptions::default(),
            status_message: None,
            connection: None,
            loaded: false,
            error: None,
            page_size: DEFAULT_PAGE_SIZE,
            refresh_requested: false,
            search_backup: None,
            pending_delete: None,
            confirm_focus: ConfirmFocus::default(),
            confirm_scroll: 0,
            requested_op: None,
            confirmed_delete: None,
            op_problems: Vec::new(),
            op_report: None,
            results_scroll: 0,
            op_in_flight: false,
            quit: false,
        }
    }
}

impl App {
    /// An app over a task list. `Vec::new()` is the normal startup state — the
    /// poller fills it in on the first tick.
    pub fn new(tasks: Vec<Task>) -> Self {
        Self {
            loaded: !tasks.is_empty(),
            tasks,
            ..Self::default()
        }
    }

    /// An app over a captured DSM `list` response on disk — the hidden
    /// `--fixture` mode.
    ///
    /// Offline verification hangs off this: with no NAS in reach the table,
    /// multi-select and the sort/filter keys have no other way to be exercised,
    /// and a 500-task file is the cheapest possible render-performance check.
    /// Nothing polls in this mode — the list is whatever the file said.
    pub fn from_fixture(path: &Path) -> Result<Self> {
        let tasks = parse_fixture(&std::fs::read_to_string(path)?)?;
        tracing::info!(
            fixture = %path.display(),
            tasks = tasks.len(),
            "loaded an offline fixture"
        );
        // Whatever the file said *is* the list — there is no poller behind this
        // mode, so an empty fixture is a loaded empty list, not a pending one.
        Ok(Self {
            loaded: true,
            ..Self::new(tasks)
        })
    }

    /// Set what a confirmed delete may do (from the merged configuration).
    pub fn with_delete_options(mut self, options: DeleteOptions) -> Self {
        self.delete_options = options;
        self
    }

    /// Record where this session is connected, for the title bar.
    pub fn with_connection(mut self, connection: impl Into<String>) -> Self {
        self.connection = Some(connection.into());
        self
    }

    /// Replace the footer message.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    /// Raise the non-fatal error banner.
    ///
    /// Nothing here ends the program: a NAS that is briefly unreachable is an
    /// ordinary event, and the next successful refresh takes the banner down
    /// again (see [`App::apply_tasks`]).
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    /// Take the error banner down.
    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Tell the app whether an operation batch is still running on the NAS.
    ///
    /// The event loop is what knows (it holds the [`JoinHandle`]s) and pushes
    /// the answer in before every draw. **One batch at a time** is the rule
    /// this enforces — two overlapping delete runs would interleave their
    /// pause/delete phases against the same NAS — and enforcing it *here*,
    /// rather than where the plan is drained, is what makes the refusal
    /// something the user is told before they press `y`.
    ///
    /// [`JoinHandle`]: tokio::task::JoinHandle
    pub fn set_op_in_flight(&mut self, running: bool) {
        self.op_in_flight = running;
    }

    /// Whether the loop reported an operation still running.
    pub fn op_in_flight(&self) -> bool {
        self.op_in_flight
    }

    /// Say no to a second batch, and report why. Returns whether it said no.
    ///
    /// `what` names the operation that was refused, for the log; the message on
    /// screen deliberately does not, because the footer it lands in is also
    /// where the *running* batch reports itself and two similar lines about two
    /// different operations read as one.
    fn refuse_while_busy(&mut self, what: &str) -> bool {
        if !self.op_in_flight {
            return false;
        }

        tracing::warn!(refused = what, "an operation is already in flight");
        self.set_status(
            "an operation is still running — wait for it to finish before starting another",
        );
        true
    }

    // ---- background events -------------------------------------------------

    /// Apply one [`AppEvent`] from the poller or an op task.
    ///
    /// The counterpart of [`App::handle_event`] for everything that is not a
    /// key press. Like key handling it is a pure `&mut self` transition, so the
    /// whole reconciliation is testable without a runtime or a NAS.
    pub fn apply_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Tasks(tasks) => self.apply_tasks(tasks),
            AppEvent::Error(message) => self.set_error(message),
            // Placeholder: `App` has no storage field yet, and the band that
            // reads it does not exist. Wired up in the next task; dropped here
            // rather than left to a catch-all arm so the compiler still names
            // every future variant that needs handling.
            AppEvent::Storage(_) => {}
            AppEvent::OpProgress {
                op,
                done,
                total,
                item,
            } => {
                // `done` counts from one, so the first item of a batch is where
                // the previous batch's problems stop being current.
                if done <= 1 {
                    self.op_problems.clear();
                }
                self.set_status(format!("{} {done}/{total} · {}", op.label(), item.detail()));
                if item.outcome.problem().is_some() {
                    self.op_problems.push(item);
                }
            }
            AppEvent::OpDone {
                op,
                succeeded,
                skipped,
                failed,
            } => {
                let report = OpReport {
                    op,
                    succeeded,
                    skipped,
                    failed,
                    problems: std::mem::take(&mut self.op_problems),
                };
                self.set_status(summary_with_hint(op, succeeded, skipped, failed, &report));
                let worth_showing = report.has_problems();
                self.op_report = Some(report);
                // Opened for the user rather than waited for: the counts land in
                // a footer that the refresh this batch just asked for is about
                // to talk over, and a reason nobody can reach is a reason
                // nobody has. Only from `Normal` — a modal must not be replaced
                // under a user who is reading it, least of all a delete
                // confirmation.
                if worth_showing && self.mode == Mode::Normal {
                    self.show_results();
                }
            }
        }
    }

    // ---- the results modal ---------------------------------------------------

    /// The last finished batch's report, if there has been one.
    pub fn last_op_report(&self) -> Option<&OpReport> {
        self.op_report.as_ref()
    }

    /// First body line the results modal should show.
    pub fn results_scroll(&self) -> usize {
        self.results_scroll
    }

    /// Open the results modal (`v`, and automatically after a batch with
    /// anything to report).
    ///
    /// Says so in the footer rather than opening an empty box when there is
    /// nothing to show: a modal that reports nothing teaches the user that the
    /// key does nothing.
    pub fn show_results(&mut self) {
        match &self.op_report {
            Some(report) if report.has_problems() => {
                self.results_scroll = 0;
                self.mode = Mode::Results;
            }
            Some(_) => self.set_status("the last operation finished with nothing to report"),
            None => self.set_status("no operation has finished yet"),
        }
    }

    /// Dismiss the results modal. The report itself is kept — `v` reopens it.
    pub fn close_results(&mut self) {
        self.results_scroll = 0;
        self.mode = Mode::Normal;
    }

    /// Scroll the results modal, clamped to the lines there are.
    fn scroll_results(&mut self, delta: isize) {
        let last = self.results_line_count().saturating_sub(1);
        self.results_scroll = shift(self.results_scroll, delta, last);
    }

    fn scroll_results_to(&mut self, line: usize) {
        self.results_scroll = line.min(self.results_line_count().saturating_sub(1));
    }

    fn results_line_count(&self) -> usize {
        self.op_report.as_ref().map_or(0, OpReport::line_count)
    }

    /// Reconcile a freshly fetched task list into the app.
    ///
    /// Three invariants, all of them about not moving things under the user:
    ///
    /// 1. **The cursor follows its task, by ID.** A list that came back sorted
    ///    differently, or with a row inserted above, must not leave the cursor
    ///    pointing at a different torrent than it did a moment ago — that is
    ///    how the wrong task gets deleted by a `d` that was already half typed.
    ///    When the task under the cursor is gone entirely, the *position* is
    ///    kept instead and clamped into the new list.
    /// 2. **Selections for tasks that no longer exist are dropped.** An ID that
    ///    names nothing is not a selection, and keeping it would let a task
    ///    that reappears later come back pre-armed for deletion.
    /// 3. **A refresh arriving while the confirmation modal is open is ignored
    ///    outright** — not merged, not queued. The `delete` plan on
    ///    screen is a snapshot, and the user is reading it; changing the list
    ///    underneath would make the dialog describe something other than what
    ///    is about to happen.
    pub fn apply_tasks(&mut self, tasks: Vec<Task>) {
        if self.mode == Mode::Confirm {
            tracing::debug!("dropping a refresh while the confirmation dialog is open");
            return;
        }

        self.preserving_cursor(|app| {
            app.tasks = tasks;
            let live: HashSet<&str> = app.tasks.iter().map(|task| task.id.as_str()).collect();
            app.selected.retain(|id| live.contains(id.as_str()));
        });

        // The first list to arrive is what separates "the NAS has no tasks"
        // from "nothing has come back yet"; see `ui::empty_state`.
        self.loaded = true;
        // A tick that got through is the proof the last failure has passed.
        self.clear_error();
    }

    /// Run `change`, then put the cursor back on the task it was on.
    ///
    /// The cursor is a row number in the *visible* list, so anything that
    /// reorders, filters or replaces that list would otherwise silently move it
    /// onto a different torrent — which is how the wrong task gets deleted by a
    /// `d` that was already half typed. Following the task by **ID** is the
    /// only reconciliation that cannot alias; when the change hides or removes
    /// that task altogether the *row number* is kept instead, so the cursor
    /// stays where the user's eye is, and clamped into whatever is left.
    ///
    /// Shared by [`App::apply_tasks`] and [`App::change_view`]: a refresh and a
    /// sort are the same hazard, and two copies of this could disagree.
    fn preserving_cursor(&mut self, change: impl FnOnce(&mut Self)) {
        let cursor_id = self.cursor_task().map(|task| task.id.clone());

        change(self);

        // One [`App::visible`] for the whole transition: it filters, searches
        // and sorts, and the reconciliation, the clamp and the scroll all want
        // the same answer.
        let visible = self.visible();
        if let Some(id) = cursor_id
            && let Some(row) = visible.iter().position(|&index| self.tasks[index].id == id)
        {
            self.cursor = row;
        }
        self.settle_cursor(visible.len());
    }

    /// Ask for an immediate refresh (`r`).
    pub fn request_refresh(&mut self) {
        self.refresh_requested = true;
    }

    /// Whether `r` was pressed since this was last asked, clearing the flag.
    ///
    /// The event loop calls this after every event and pokes the poller; in
    /// offline `--fixture` mode nothing is listening and the request is simply
    /// dropped.
    pub fn take_refresh_request(&mut self) -> bool {
        std::mem::take(&mut self.refresh_requested)
    }

    /// Indices into [`App::tasks`] of the rows to display, in display order.
    ///
    /// **Derived on every call, never stored.** `tasks` and `view` are public
    /// and mutated directly (by the poller, by every view key and by the
    /// tests), so a cached list would have to be invalidated from more places
    /// than can be checked — and a stale one puts the cursor, the selection
    /// and the confirmation dialog on different rows than the table.
    ///
    /// It is not free — a `Vec` plus an O(n log n) sort — so a caller that
    /// needs it more than once holds on to the answer: [`crate::ui::render`]
    /// takes it once per frame and passes it down, and everything that moves
    /// the cursor reuses the row count it already has (see
    /// [`App::scroll_offset_for`]).
    pub fn visible(&self) -> Vec<usize> {
        view::visible_indices(&self.tasks, &self.view)
    }

    /// How many rows the current sort/filter/search leaves on screen.
    ///
    /// Costs a whole [`App::visible`]; prefer `visible().len()` where the list
    /// itself is wanted too.
    pub fn visible_count(&self) -> usize {
        self.visible().len()
    }

    /// The task under the cursor, if any row is visible at all.
    pub fn cursor_task(&self) -> Option<&Task> {
        self.visible()
            .get(self.cursor)
            .map(|&index| &self.tasks[index])
    }

    // ---- sort, filter and search -------------------------------------------
    //
    // Every one of these changes *which rows are on screen and in what order*,
    // and none of them may change **what is armed for deletion**. So they all
    // go through [`App::change_view`], which puts the cursor back on the task
    // it was on and leaves [`App::selected`] strictly alone.

    /// Advance to the next sort column (`s`).
    pub fn cycle_sort(&mut self) {
        self.change_view(View::cycle_sort);
    }

    /// Reverse the sort direction (`S`).
    pub fn toggle_sort_dir(&mut self) {
        self.change_view(View::toggle_dir);
    }

    /// Advance to the next status filter (`f`).
    pub fn cycle_filter(&mut self) {
        self.change_view(View::cycle_filter);
    }

    /// Apply a change to the view and put the cursor back where it belongs.
    ///
    /// The cursor is a row number in the *visible* list, so re-sorting or
    /// re-filtering underneath it would otherwise silently move it onto a
    /// different task — the same hazard [`App::apply_tasks`] guards against for
    /// a refresh, and it is resolved the same way: follow the task by **ID**,
    /// and when the change hides that task altogether keep the row number and
    /// clamp it into whatever is left.
    ///
    /// The selection is deliberately untouched. A filter is a question about
    /// what to *look* at; it is never an instruction to disarm rows that
    /// scrolled out of sight.
    fn change_view(&mut self, change: impl FnOnce(&mut View)) {
        self.preserving_cursor(|app| change(&mut app.view));
    }

    /// Start editing the search query (`/`).
    ///
    /// The current query is kept — `/` refines a search rather than throwing it
    /// away — and stashed, so `Esc` can restore it.
    pub fn begin_search(&mut self) {
        self.search_backup = Some(self.view.search.clone());
        self.mode = Mode::Search;
    }

    /// Accept the query as typed and leave search mode (`Enter`).
    ///
    /// The table is already showing the result: matching happens on every
    /// keystroke, so `Enter` commits rather than applies. Dropping the backup
    /// is the commit.
    pub fn commit_search(&mut self) {
        self.search_backup = None;
        self.mode = Mode::Normal;
    }

    /// Abandon the edit and restore the query `/` was pressed on (`Esc`).
    ///
    /// Note this is `Esc` in [`Mode::Search`] only — in [`Mode::Normal`] the
    /// same key clears the selection.
    pub fn cancel_search(&mut self) {
        if let Some(previous) = self.search_backup.take() {
            self.change_view(|view| view.search = previous);
        }
        self.mode = Mode::Normal;
    }

    /// Append one typed character to the query.
    pub fn search_push(&mut self, c: char) {
        self.change_view(|view| view.search.push(c));
    }

    /// Delete the last character of the query (`Backspace`).
    ///
    /// Backspacing past the start is a no-op rather than an exit: leaving the
    /// mode on an empty query would make the key that widens a search
    /// occasionally cancel it instead.
    pub fn search_pop(&mut self) {
        self.change_view(|view| {
            view.search.pop();
        });
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
    /// Selections are dropped when a refresh removes their task
    /// ([`App::apply_tasks`]), but
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

    // ---- the delete confirmation -------------------------------------------
    //
    // `d` never deletes. It takes a **snapshot** ([`DeletePlan`]) and opens a
    // modal describing it; only `y`, or `Enter` on a deliberately re-focused
    // Delete button, records the intent. Even then nothing here touches the
    // network — [`App::take_confirmed_delete`] hands the plan to the event loop,
    // and [`crate::event::spawn_delete`] owns the three-phase execution.

    /// Open the confirmation modal for the current target (`d`).
    ///
    /// The target is **the selection when there is one, and the row under the
    /// cursor otherwise** — a `d` aimed at a row the user is looking at must
    /// work without arming it first. A plan with no items (an empty table)
    /// opens no dialog at all: there is nothing to confirm.
    pub fn begin_delete(&mut self) {
        if self.refuse_while_busy("a delete") {
            return;
        }

        let plan = DeletePlan::snapshot(self.target_tasks());
        if plan.is_empty() {
            self.set_status("nothing to delete");
            return;
        }

        tracing::info!(items = plan.len(), "opening the delete confirmation");
        self.pending_delete = Some(plan);
        // Every open starts on Cancel. Focus must never carry over from a
        // previous dialog the user left on Delete.
        self.confirm_focus = ConfirmFocus::Cancel;
        self.confirm_scroll = 0;
        self.mode = Mode::Confirm;
    }

    /// The tasks an operation acts on: **the selection when there is one, the
    /// row under the cursor otherwise, and nothing at all when the table is
    /// empty.**
    ///
    /// One definition shared by `d`, `p` and `u`, deliberately: three keys that
    /// disagreed about what "the current target" means is how a user who armed
    /// a selection ends up pausing the row their cursor happened to be resting
    /// on. A selected task that a filter is currently hiding **is** included —
    /// the selection is what is armed, not what is on screen.
    ///
    /// **Order is on-screen order.** The confirmation dialog lists these rows
    /// back to the user for checking, and a dialog whose order does not match
    /// the table's defeats the one job that screen has — under any non-default
    /// sort, `self.tasks` order is not what the user is looking at. Selected
    /// rows a filter is currently hiding have no on-screen position, so they
    /// follow, in DSM order.
    fn target_tasks(&self) -> Vec<&Task> {
        if self.selected_count() == 0 {
            return self.cursor_task().into_iter().collect();
        }

        let mut shown: Vec<&Task> = self
            .visible()
            .into_iter()
            .map(|index| &self.tasks[index])
            .filter(|task| self.selected.contains(&task.id))
            .collect();

        let on_screen: HashSet<&str> = shown.iter().map(|task| task.id.as_str()).collect();
        shown.extend(
            self.selected_tasks()
                .filter(|task| !on_screen.contains(task.id.as_str())),
        );
        shown
    }

    // ---- pause and resume ---------------------------------------------------
    //
    // Unlike `d` these need no confirmation: both are reversible by the other
    // key, and a modal in front of a reversible operation only teaches the user
    // to dismiss modals. They still perform **no I/O here** — the request is
    // parked for the event loop exactly as a confirmed delete is.

    /// Pause the current target (`p`).
    pub fn pause_target(&mut self) {
        self.request_task_op(TaskOp::Pause);
    }

    /// Resume the current target (`u`).
    pub fn resume_target(&mut self) {
        self.request_task_op(TaskOp::Resume);
    }

    /// Record a pause/resume for the event loop to run.
    ///
    /// An empty target — an empty table, or a filter that hides everything — is
    /// a **no-op with a message**, never an empty batch: a round trip that can
    /// only report "nothing to do" is not worth making.
    fn request_task_op(&mut self, op: TaskOp) {
        if self.refuse_while_busy(op.label()) {
            return;
        }

        let tasks: Vec<TaskRef> = self
            .target_tasks()
            .into_iter()
            .map(|task| TaskRef {
                id: task.id.clone(),
                title: task.title.clone(),
            })
            .collect();
        if tasks.is_empty() {
            self.set_status(format!("nothing to {}", op.label()));
            return;
        }

        tracing::info!(
            op = op.label(),
            tasks = tasks.len(),
            "requesting an operation"
        );
        let plural = if tasks.len() == 1 { "task" } else { "tasks" };
        self.set_status(format!(
            "{} requested for {} {plural}",
            op.label(),
            tasks.len()
        ));
        self.requested_op = Some(TaskOpRequest { op, tasks });
    }

    /// Take the pause/resume the user asked for, if there is one.
    ///
    /// The counterpart of [`App::take_confirmed_delete`], drained by the event
    /// loop on every pass.
    pub fn take_requested_op(&mut self) -> Option<TaskOpRequest> {
        self.requested_op.take()
    }

    /// The plan the modal is displaying, if it is open.
    pub fn pending_delete(&self) -> Option<&DeletePlan> {
        self.pending_delete.as_ref()
    }

    /// Which modal button `Enter` would press.
    pub fn confirm_focus(&self) -> ConfirmFocus {
        self.confirm_focus
    }

    /// First body line the modal should show.
    pub fn confirm_scroll(&self) -> usize {
        self.confirm_scroll
    }

    /// Move the focus between Cancel and Delete (`Tab`, `←`/`→`, `h`/`l`).
    pub fn toggle_confirm_focus(&mut self) {
        self.confirm_focus = self.confirm_focus.other();
    }

    /// Scroll the modal body, clamped to the lines there are.
    ///
    /// The *height* is not known here — the renderer clamps again against it,
    /// exactly as `ui::table` derives its scroll offset — but clamping to the
    /// line count stops a held `j` from running the offset up to a number the
    /// user then has to hold `k` through.
    pub fn scroll_confirm(&mut self, delta: isize) {
        let last = self.confirm_line_count().saturating_sub(1);
        self.confirm_scroll = shift(self.confirm_scroll, delta, last);
    }

    /// Jump the modal body to the top or the bottom.
    pub fn scroll_confirm_to(&mut self, line: usize) {
        self.confirm_scroll = line.min(self.confirm_line_count().saturating_sub(1));
    }

    /// How many body lines the open plan renders to.
    ///
    /// Goes through [`dialog::build_confirmation`] rather than counting items
    /// so there is exactly one definition of what the dialog shows; the summary
    /// is a handful of strings and rebuilding it per keystroke costs nothing.
    fn confirm_line_count(&self) -> usize {
        self.pending_delete.as_ref().map_or(0, |plan| {
            dialog::build_confirmation(plan, self.delete_options).line_count()
        })
    }

    /// Close the modal without deleting anything (`n`, `Esc`, `q`, or `Enter`
    /// on the default focus).
    pub fn cancel_delete(&mut self) {
        if self.pending_delete.take().is_some() {
            tracing::info!("delete cancelled");
        }
        self.confirm_scroll = 0;
        self.confirm_focus = ConfirmFocus::Cancel;
        self.mode = Mode::Normal;
    }

    /// Accept the plan on screen (`y`, or `Enter` with the Delete button
    /// focused).
    ///
    /// **This performs no I/O.** The snapshot is parked in `confirmed_delete`
    /// for [`App::take_confirmed_delete`]; the execution — pause, files, task —
    /// belongs to [`crate::event::spawn_delete`].
    pub fn confirm_delete(&mut self) {
        if let Some(plan) = self.pending_delete.take() {
            tracing::info!(
                items = plan.len(),
                // "resolved", not "deletable": with `--no-delete-files` a
                // refused item is deleted too — see `delete::will_act`.
                resolved = plan.deletable().count(),
                refused = plan.refused().count(),
                dry_run = self.delete_options.dry_run,
                delete_files = self.delete_options.delete_files,
                "delete confirmed"
            );
            self.confirmed_delete = Some(plan);
        }
        self.confirm_scroll = 0;
        self.confirm_focus = ConfirmFocus::Cancel;
        self.mode = Mode::Normal;
    }

    /// Take the confirmed plan, if the user confirmed one since the last call.
    ///
    /// The counterpart of [`App::take_refresh_request`], and the seam the event
    /// loop plugs [`crate::event::spawn_delete`] into.
    pub fn take_confirmed_delete(&mut self) -> Option<DeletePlan> {
        self.confirmed_delete.take()
    }

    /// Tell the app how tall the table body is, so `PageUp`/`PageDown` move by
    /// a screenful. Clamped to at least one row — a zero-row page would make
    /// the key silently dead.
    pub fn set_page_size(&mut self, rows: usize) {
        self.page_size = rows.max(1);
        self.follow_cursor();
    }

    /// The first table row to draw in a viewport `height` rows tall.
    ///
    /// The stored offset, re-clamped: see [`App::scroll`] and
    /// [`crate::ui::table::scroll_offset`]. `render` is the caller, and it
    /// knows the real height of the frame it is drawing, which the app may not
    /// have been told about yet.
    pub fn scroll_offset(&self, height: usize) -> usize {
        self.scroll_offset_for(self.visible_count(), height)
    }

    /// As [`App::scroll_offset`], for a caller that already knows how many rows
    /// are visible — which spares it a second filter-and-sort. The row count
    /// must be `self.visible().len()`; passing anything else scrolls the table
    /// to a row that is not there.
    pub fn scroll_offset_for(&self, rows: usize, height: usize) -> usize {
        table::scroll_offset(self.scroll, self.cursor, rows, height)
    }

    /// Re-seat the window around the cursor, in the height the last frame had.
    ///
    /// Called by everything that moves the cursor or resizes the table. It is
    /// only ever an *approximation* of the next frame — the clamp in
    /// [`App::scroll_offset`] is what makes the drawn window correct — but
    /// keeping the stored value in step here is what makes the scrolling
    /// edge-triggered rather than one-row-at-a-time.
    fn follow_cursor(&mut self) {
        self.follow_cursor_in(self.visible_count());
    }

    /// [`App::follow_cursor`] for a caller holding the row count already.
    fn follow_cursor_in(&mut self, rows: usize) {
        self.scroll = self.scroll_offset_for(rows, self.page_size);
    }

    /// Pull the cursor back inside the visible list.
    ///
    /// Called after anything that can shrink the list — a filter change
    /// ([`App::cycle_filter`]), a refresh that removed rows
    /// ([`App::apply_tasks`]) — and by every movement, so
    /// [`App::cursor`] is never a position that does not exist.
    pub fn clamp_cursor(&mut self) {
        self.settle_cursor(self.visible_count());
    }

    /// [`App::clamp_cursor`] for a caller holding the row count already.
    fn settle_cursor(&mut self, rows: usize) {
        self.cursor = self.cursor.min(rows.saturating_sub(1));
        self.follow_cursor_in(rows);
    }

    /// Move the cursor by `delta` rows, clamped to the ends of the list.
    ///
    /// Deliberately does **not** wrap: holding `j` at the bottom of a long list
    /// jumping back to the top is how the wrong row gets deleted.
    pub fn move_cursor(&mut self, delta: isize) {
        let rows = self.visible_count();
        if rows == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        self.cursor = shift(self.cursor, delta, rows.saturating_sub(1));
        self.follow_cursor_in(rows);
    }

    /// Jump to the first visible row (`Home`, `g`).
    pub fn cursor_to_first(&mut self) {
        self.cursor = 0;
        self.follow_cursor();
    }

    /// Jump to the last visible row (`End`, `G`).
    pub fn cursor_to_last(&mut self) {
        let rows = self.visible_count();
        self.cursor = rows.saturating_sub(1);
        self.follow_cursor_in(rows);
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
            Mode::Search => self.handle_search_key(key),
            Mode::Confirm => self.handle_confirm_key(key),
            // The overlay is a reference card, not a mode with bindings: every
            // key closes it, including the one the user reached for next.
            // Nothing else happens on that key — a `d` that dismissed the help
            // *and* opened a delete confirmation would be a surprise on the
            // screen that exists to remove surprises.
            Mode::Help => self.close_help(),
            Mode::Results => self.handle_results_key(key),
        }
    }

    /// Open the `?` overlay.
    pub fn show_help(&mut self) {
        self.mode = Mode::Help;
    }

    /// Close the `?` overlay (any key).
    pub fn close_help(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Keys while browsing the table.
    ///
    /// No key here touches the network: `d` opens the confirmation modal, `p`
    /// and `u` record a [`TaskOpRequest`], and the event loop hands both to
    /// [`crate::event::spawn_delete`] / [`crate::event::spawn_task_op`].
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
            KeyCode::Char('r') => self.request_refresh(),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => self.toggle_sort_dir(),
            KeyCode::Char('f') => self.cycle_filter(),
            KeyCode::Char('d') => self.begin_delete(),
            // Both are reversible by the other key, so neither is confirmed.
            KeyCode::Char('p') => self.pause_target(),
            KeyCode::Char('u') => self.resume_target(),
            KeyCode::Char('/') => self.begin_search(),
            // The reasons the footer could not hold. Never destructive, so it
            // needs no confirmation and is safe to reach for at any time.
            KeyCode::Char('v') => self.show_results(),
            KeyCode::Char('?') => self.show_help(),
            // `Esc` is mode-specific: here it is the panic button for a
            // selection, in `Mode::Search` it cancels the edit.
            KeyCode::Esc => self.clear_selection(),
            _ => {}
        }
    }

    /// Keys while the confirmation modal is open.
    ///
    /// The safety rules, in order of how easily they are got wrong:
    ///
    /// * **`Enter` presses the focused button**, and the focus starts on
    ///   Cancel. A modal that opens with `Enter` wired straight to a recursive
    ///   delete is one reflex away from data loss.
    /// * **`q` closes the dialog rather than the program.** Quitting out of a
    ///   half-read confirmation is a perfectly reasonable thing to want, and
    ///   `Ctrl-C` (handled above, before the mode dispatch) still does it.
    /// * **Every unrecognized key does nothing at all** — never "the safe
    ///   default of confirming".
    fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_delete(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                self.cancel_delete();
            }
            KeyCode::Enter => match self.confirm_focus {
                ConfirmFocus::Cancel => self.cancel_delete(),
                ConfirmFocus::Delete => self.confirm_delete(),
            },
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l') => self.toggle_confirm_focus(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_confirm(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_confirm(1),
            KeyCode::PageUp => self.scroll_confirm(-page_delta(self.page_size)),
            KeyCode::PageDown => self.scroll_confirm(page_delta(self.page_size)),
            KeyCode::Home => self.scroll_confirm_to(0),
            KeyCode::End => self.scroll_confirm_to(usize::MAX),
            _ => {}
        }
    }

    /// Keys while the results modal is open.
    ///
    /// **Not "any key closes it"**, the way the help overlay works: this one
    /// scrolls, so `j` and `k` have to stay available, and an unrecognized key
    /// does nothing rather than dismissing the only place the reasons are
    /// legible. The modal changes nothing, so there is no destructive key here
    /// to guard against either.
    fn handle_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc
            | KeyCode::Enter
            | KeyCode::Char('q')
            | KeyCode::Char('v')
            | KeyCode::Char(' ') => self.close_results(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_results(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_results(1),
            KeyCode::PageUp => self.scroll_results(-page_delta(self.page_size)),
            KeyCode::PageDown => self.scroll_results(page_delta(self.page_size)),
            KeyCode::Home => self.scroll_results_to(0),
            KeyCode::End => self.scroll_results_to(usize::MAX),
            _ => {}
        }
    }

    /// Keys while the search box has focus.
    ///
    /// Everything printable is **text**, not a command: `q` types a `q` rather
    /// than quitting, and the only way out is `Enter`, `Esc` or the global
    /// `Ctrl-C`. A search box where half the alphabet still triggers bindings
    /// is a search box that cannot search for those letters.
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.commit_search(),
            KeyCode::Esc => self.cancel_search(),
            KeyCode::Backspace => self.search_pop(),
            // A modified character is a command someone aimed elsewhere, not
            // something to type. `Shift` is excluded from that: it is how a
            // capital letter arrives.
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.search_push(c);
            }
            _ => {}
        }
    }
}

/// The one-line footer report of a finished background operation.
///
/// Three deliberate choices:
///
/// * **Only non-zero categories are named.** "delete finished: 3 succeeded" is
///   readable; "3 succeeded, 0 skipped, 0 failed" is a form to be decoded.
/// * **Any failure is marked.** A count of failures sitting quietly beside a
///   count of successes is exactly how a failed delete goes unnoticed.
/// * **It goes in the status message, not the error banner.** An operation asks
///   for an immediate refresh when it finishes, and the banner is cleared by the
///   next successful tick — the report would vanish a moment after appearing.
pub fn op_summary(op: OpKind, succeeded: usize, skipped: usize, failed: usize) -> String {
    let mut parts = Vec::new();
    if succeeded > 0 {
        parts.push(format!("{succeeded} succeeded"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if parts.is_empty() {
        parts.push("nothing to do".to_string());
    }

    let marker = if failed > 0 { "⚠ " } else { "" };
    format!("{marker}{} finished: {}", op.label(), parts.join(", "))
}

/// [`op_summary`] plus, when there is something to read, the key that reaches
/// it.
///
/// The counts on their own are a dead end — "2 failed" names neither task nor
/// reason, and the log file whose path scrolled past at startup is not an answer
/// a user inside a TUI can act on. The modal opens by itself, but it is also
/// dismissable, so the way back has to be on screen.
fn summary_with_hint(
    op: OpKind,
    succeeded: usize,
    skipped: usize,
    failed: usize,
    report: &OpReport,
) -> String {
    let summary = op_summary(op, succeeded, skipped, failed);
    if report.has_problems() {
        format!("{summary} · v for the reasons")
    } else {
        summary
    }
}

/// A page jump as a signed row count, saturating rather than wrapping on the
/// (impossible in practice) terminal taller than `isize::MAX` rows.
fn page_delta(page_size: usize) -> isize {
    isize::try_from(page_size).unwrap_or(isize::MAX)
}

/// Move `value` by a signed `delta`, saturating at `0` and at `max`.
///
/// Deliberately **does not wrap**: holding `j` at the bottom of a long list and
/// jumping back to the top is how the wrong row gets deleted. Shared by the
/// table cursor and the confirmation-modal scroll, which had the same three
/// lines twice.
fn shift(value: usize, delta: isize, max: usize) -> usize {
    if delta < 0 {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta.unsigned_abs())
    }
    .min(max)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskStatus;
    use crate::view::StatusFilter;

    use crate::testutil::{fixture_task, fixture_tasks};

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

    // ---- the help overlay --------------------------------------------------

    #[test]
    fn question_mark_opens_the_help_overlay() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
    }

    #[test]
    fn any_key_closes_the_help_overlay() {
        // A reference card must never be a mode the user has to work out how
        // to leave, so every key is the way out — including `?` itself.
        for key in [
            KeyCode::Char('x'),
            KeyCode::Char('?'),
            KeyCode::Char('d'),
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Down,
        ] {
            let mut app = App::new(fixture_tasks());
            app.handle_key(press(KeyCode::Char('?')));
            assert_eq!(app.mode, Mode::Help, "{key:?}");
            app.handle_key(press(key));
            assert_eq!(app.mode, Mode::Normal, "{key:?}");
        }
    }

    #[test]
    fn the_key_that_closes_the_help_does_nothing_else() {
        // Dismissing the help with `d` must not also open a delete
        // confirmation: the overlay exists to remove surprises.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('?')));
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_delete().is_none());

        // ...and nor does it move the cursor or touch the selection.
        let mut app = App::new(fixture_tasks());
        app.cursor = 3;
        app.handle_key(press(KeyCode::Char('?')));
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.cursor, 3);
        assert!(app.selected.is_empty());
    }

    #[test]
    fn the_help_key_is_normal_mode_only() {
        // In the search box `?` is a character to search for; in the
        // confirmation it is an unbound key and must change nothing.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        app.handle_key(press(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.view.search, "?");

        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Confirm);
        app.handle_key(press(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Confirm);
    }

    #[test]
    fn the_status_message_is_settable() {
        let mut app = App::default();
        app.set_status("hello");
        assert_eq!(app.status_message.as_deref(), Some("hello"));
    }

    // ---- cursor movement ---------------------------------------------------

    /// One synthetic task. Everything the reconciliation cares about is the ID;
    /// the title and size are what the sort and the footer read.
    fn task(id: &str, title: &str, size: u64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status: TaskStatus::Paused,
            task_type: crate::model::TaskType::BitTorrent,
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

    /// An app over `count` synthetic tasks, all visible.
    fn app_with(count: usize) -> App {
        let tasks = (0..count)
            .map(|n| task(&format!("id_{n:03}"), &format!("task {n:03}"), 0))
            .collect();
        App::new(tasks)
    }

    /// A per-item progress report for a task that succeeded.
    fn done_item(title: &str) -> ItemReport {
        ItemReport {
            title: title.to_string(),
            outcome: crate::event::ItemOutcome::Done("deleted".to_string()),
        }
    }

    /// A per-item progress report for a task that failed, with its reason.
    fn failed_item(title: &str, why: &str) -> ItemReport {
        ItemReport {
            title: title.to_string(),
            outcome: crate::event::ItemOutcome::Failed(why.to_string()),
        }
    }

    /// A per-item progress report for a task that was deliberately skipped.
    fn skipped_item(title: &str, why: &str) -> ItemReport {
        ItemReport {
            title: title.to_string(),
            outcome: crate::event::ItemOutcome::Skipped(why.to_string()),
        }
    }

    /// The ID of the task the cursor is on, for the reconciliation assertions.
    fn cursor_id(app: &App) -> Option<&str> {
        app.cursor_task().map(|task| task.id.as_str())
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
        assert_eq!(app.page_size, DEFAULT_PAGE_SIZE);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, DEFAULT_PAGE_SIZE);

        // A terminal too short for even one table row still has to page.
        app.set_page_size(0);
        assert_eq!(app.page_size, 1);
        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.cursor, DEFAULT_PAGE_SIZE + 1);
    }

    #[test]
    fn the_row_count_shortcut_answers_exactly_as_the_full_derivation_does() {
        // `scroll_offset_for` exists only to spare the caller a second
        // filter-and-sort per frame; the moment it disagrees with
        // `scroll_offset` the table scrolls to a row the cursor is not on.
        let mut app = App::new(fixture_tasks());
        for filter in [
            StatusFilter::All,
            StatusFilter::Seeding,
            StatusFilter::Error,
        ] {
            app.view.filter = filter;
            app.clamp_cursor();
            let rows = app.visible_count();
            for cursor in 0..=rows {
                app.cursor = cursor;
                for height in [0, 1, 3, 40] {
                    assert_eq!(
                        app.scroll_offset(height),
                        app.scroll_offset_for(rows, height),
                        "{filter:?} cursor {cursor} height {height}"
                    );
                }
            }
        }
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
        // `cycle_filter` changes the filter under the cursor and `apply_tasks`
        // refreshes the list under it; both rely on this.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.cursor, 13);
        app.view.filter = StatusFilter::Paused;
        app.clamp_cursor();
        assert_eq!(app.cursor, 0);
        assert_eq!(app.cursor_task().map(|t| t.id.as_str()), Some("dbid_004"));
    }

    // ---- sort, filter and search keys --------------------------------------

    #[test]
    fn s_cycles_the_sort_column_and_leaves_the_direction_alone() {
        let mut app = App::new(fixture_tasks());
        assert_eq!(app.view.sort_key, view::SortKey::Name);

        app.handle_key(press(KeyCode::Char('s')));
        assert_eq!(app.view.sort_key, view::SortKey::Status);
        assert_eq!(app.view.sort_dir, view::SortDir::Asc);

        // All the way round and back to where it started.
        for _ in 1..view::SortKey::ALL.len() {
            app.handle_key(press(KeyCode::Char('s')));
        }
        assert_eq!(app.view.sort_key, view::SortKey::Name);
    }

    #[test]
    fn capital_s_reverses_the_sort_without_changing_the_column() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('s')));
        let key = app.view.sort_key;

        app.handle_key(press(KeyCode::Char('S')));
        assert_eq!(app.view.sort_dir, view::SortDir::Desc);
        assert_eq!(app.view.sort_key, key);

        app.handle_key(press(KeyCode::Char('S')));
        assert_eq!(app.view.sort_dir, view::SortDir::Asc);
    }

    #[test]
    fn f_cycles_the_status_filter_and_wraps() {
        let mut app = App::new(fixture_tasks());
        let mut seen = vec![app.view.filter];
        for _ in 1..StatusFilter::ALL.len() {
            app.handle_key(press(KeyCode::Char('f')));
            seen.push(app.view.filter);
        }
        assert_eq!(seen, StatusFilter::ALL.to_vec());

        app.handle_key(press(KeyCode::Char('f')));
        assert_eq!(app.view.filter, StatusFilter::All);
        assert_eq!(app.visible_count(), 14);
    }

    #[test]
    fn a_sort_change_keeps_the_cursor_on_the_same_task() {
        // Pressing `s` must not hand the cursor — and therefore the next `d` —
        // to whatever task happens to land on that row number.
        let mut app = App::new(fixture_tasks());
        app.cursor = 3;
        let before = cursor_id(&app).expect("a row under the cursor").to_string();

        app.handle_key(press(KeyCode::Char('s')));
        assert_eq!(cursor_id(&app), Some(before.as_str()));
        app.handle_key(press(KeyCode::Char('S')));
        assert_eq!(cursor_id(&app), Some(before.as_str()));
    }

    #[test]
    fn a_filter_change_clamps_the_cursor_into_the_new_visible_set() {
        let mut app = App::new(fixture_tasks());
        app.cursor_to_last();
        assert_eq!(app.cursor, 13);

        // All -> Downloading -> Seeding -> Finished -> Paused, which leaves
        // exactly one row: a cursor of 13 cannot survive it.
        for _ in 0..4 {
            app.handle_key(press(KeyCode::Char('f')));
        }
        assert_eq!(app.view.filter, StatusFilter::Paused);
        assert_eq!(app.visible_count(), 1);
        assert_eq!(app.cursor, 0);
        assert_eq!(cursor_id(&app), Some("dbid_004"));
    }

    #[test]
    fn a_filter_that_hides_everything_leaves_a_valid_cursor() {
        let mut app = App::new(fixture_tasks());
        app.cursor_to_last();
        app.search_push('z');
        app.search_push('z');
        app.search_push('z');
        assert_eq!(app.visible_count(), 0);
        assert_eq!(app.cursor, 0);
        assert!(app.cursor_task().is_none());
    }

    #[test]
    fn a_filter_change_leaves_the_selection_untouched() {
        // A filter is a question about what to look at, never an instruction to
        // disarm rows that scrolled out of sight.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 14);
        let armed = selected_ids(&app);

        for _ in 0..StatusFilter::ALL.len() {
            app.handle_key(press(KeyCode::Char('f')));
            assert_eq!(selected_ids(&app), armed, "{:?}", app.view.filter);
        }
        app.handle_key(press(KeyCode::Char('s')));
        app.handle_key(press(KeyCode::Char('S')));
        assert_eq!(selected_ids(&app), armed);
    }

    #[test]
    fn a_filter_change_keeps_the_cursor_on_a_task_that_survives_it() {
        // The cursor is a row number, and narrowing the list renumbers every
        // row. It has to follow the *task*.
        let mut app = App::new(fixture_tasks());
        app.cursor = app
            .visible()
            .iter()
            .position(|&index| app.tasks[index].id == "dbid_001")
            .expect("the downloading task");
        let row_before = app.cursor;

        // All -> Downloading, which dbid_001 is part of.
        app.handle_key(press(KeyCode::Char('f')));
        assert_eq!(app.view.filter, StatusFilter::Downloading);
        assert_ne!(app.cursor, row_before, "the rows were renumbered");
        assert_eq!(cursor_id(&app), Some("dbid_001"));
    }

    // ---- the search-mode state machine -------------------------------------

    /// Type a whole query, one key event at a time.
    fn type_query(app: &mut App, query: &str) {
        for c in query.chars() {
            app.handle_key(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn slash_enters_search_mode_without_disturbing_the_table() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Search);
        assert!(app.view.search.is_empty());
        assert_eq!(app.visible_count(), 14);
    }

    #[test]
    fn typing_narrows_the_table_as_the_query_grows() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        assert_eq!(app.view.search, "1080p");
        assert_eq!(app.visible_count(), 3, "matching is live, not on Enter");
        assert_eq!(app.mode, Mode::Search);
    }

    #[test]
    fn backspace_widens_the_search_again_and_stops_at_an_empty_query() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "ubuntu");
        assert_eq!(app.visible_count(), 1);

        for _ in 0..10 {
            app.handle_key(press(KeyCode::Backspace));
        }
        assert_eq!(app.view.search, "");
        assert_eq!(app.visible_count(), 14);
        assert_eq!(
            app.mode,
            Mode::Search,
            "backspacing past the start is inert"
        );
    }

    #[test]
    fn enter_commits_the_query_and_returns_to_normal_mode() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        app.handle_key(press(KeyCode::Enter));

        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.view.search, "1080p");
        assert_eq!(app.visible_count(), 3);

        // ...and a committed query cannot be un-done by a later Esc.
        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.view.search, "1080p");
    }

    #[test]
    fn esc_cancels_the_edit_and_restores_the_previous_query() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        app.handle_key(press(KeyCode::Enter));

        // A second search, abandoned half typed.
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "-nope");
        assert_eq!(app.view.search, "1080p-nope");
        assert_eq!(app.visible_count(), 0);

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.view.search, "1080p", "the prior query comes back");
        assert_eq!(app.visible_count(), 3);
    }

    #[test]
    fn esc_out_of_a_first_search_restores_the_empty_query() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "ubuntu");
        assert_eq!(app.visible_count(), 1);

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.view.search, "");
        assert_eq!(app.visible_count(), 14);
    }

    #[test]
    fn slash_refines_the_committed_query_rather_than_clearing_it() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "ubuntu");
        app.handle_key(press(KeyCode::Enter));

        app.handle_key(press(KeyCode::Char('/')));
        assert_eq!(app.view.search, "ubuntu", "reopening keeps what was typed");
        app.handle_key(press(KeyCode::Backspace));
        assert_eq!(app.view.search, "ubunt");
    }

    #[test]
    fn every_printable_key_is_text_in_search_mode() {
        // `q` must not quit, `a` must not select all, `g`/`G` must not jump and
        // `/` is just a slash — a search box that cannot type the alphabet is
        // not a search box.
        let mut app = App::new(fixture_tasks());
        app.cursor = 5;
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "qagGsSfr/ ");

        assert_eq!(app.view.search, "qagGsSfr/ ");
        assert!(!app.should_quit());
        assert!(app.selected.is_empty());
        assert_eq!(app.view.sort_key, view::SortKey::Name);
        assert_eq!(app.view.filter, StatusFilter::All);
        assert!(!app.take_refresh_request());
    }

    #[test]
    fn a_control_chord_is_not_typed_into_the_query() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(app.view.search.is_empty());
        assert_eq!(app.mode, Mode::Search);

        // ...but a shifted character is how a capital letter arrives.
        app.handle_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT));
        assert_eq!(app.view.search, "U");
    }

    #[test]
    fn ctrl_c_still_quits_out_of_the_search_box() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn a_search_leaves_the_selection_alone_and_moves_the_cursor_only_to_stay_valid() {
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('a')));
        let armed = selected_ids(&app);
        app.cursor_to_last();

        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        assert_eq!(
            selected_ids(&app),
            armed,
            "a search arms and disarms nothing"
        );
        assert_eq!(app.visible_count(), 3);
        assert!(
            app.cursor < 3 && app.cursor_task().is_some(),
            "the cursor was pulled into the matching rows: {}",
            app.cursor
        );

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(selected_ids(&app), armed);
    }

    #[test]
    fn a_search_keeps_the_cursor_on_a_task_that_still_matches() {
        let mut app = App::new(fixture_tasks());
        app.view.sort_key = view::SortKey::Size;
        app.cursor = 0;
        let kept = app
            .visible()
            .iter()
            .map(|&index| app.tasks[index].clone())
            .find(|task| task.title.contains("1080p"))
            .expect("a matching task");
        app.cursor = app
            .visible()
            .iter()
            .position(|&index| app.tasks[index].id == kept.id)
            .expect("its row");

        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        assert_eq!(cursor_id(&app), Some(kept.id.as_str()));
    }

    #[test]
    fn esc_in_normal_mode_still_clears_the_selection() {
        // The two jobs of `Esc` must stay mode-correct: cancelling a search in
        // `Mode::Search`, clearing the selection in `Mode::Normal`.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "1080p");
        app.handle_key(press(KeyCode::Enter));
        app.handle_key(press(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 3);

        app.handle_key(press(KeyCode::Esc));
        assert!(app.selected.is_empty(), "Esc cleared the selection");
        assert_eq!(app.view.search, "1080p", "...and not the search");
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn a_refresh_arriving_mid_search_does_not_close_the_box() {
        // The poller keeps running while the user types; the query, the mode
        // and the reconciled cursor all have to survive it.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        type_query(&mut app, "ubuntu");
        let kept = cursor_id(&app).expect("the one match").to_string();

        app.apply_event(AppEvent::Tasks(fixture_tasks()));

        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.view.search, "ubuntu");
        assert_eq!(cursor_id(&app), Some(kept.as_str()));
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
        // The heart of `toggle_select_all_visible`: with a filter on, `a` must
        // not arm a delete
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
        // `apply_tasks` prunes these on refresh; until it does, the footer must
        // not claim a task that is not there.
        let mut app = app_with(2);
        app.selected.insert("id_000".to_string());
        app.selected.insert("ghost".to_string());
        assert_eq!(app.selected_count(), 1);
        assert_eq!(app.selected_size(), 0);
    }

    // ---- refresh reconciliation --------------------------------------------
    //
    // The heart of `apply_tasks`. A refresh lands every few seconds, unannounced,
    // possibly while the user is reaching for `d` — so what it may *not* do is
    // move the cursor onto a different task or leave a selection armed against
    // one that is gone.

    #[test]
    fn a_refresh_that_reorders_the_list_keeps_the_cursor_on_the_same_task() {
        // A new task sorting above the cursor pushes every row down by one. The
        // row number must follow the task, not the other way round.
        let mut app = App::new(vec![
            task("id_b", "bravo", 0),
            task("id_c", "charlie", 0),
            task("id_d", "delta", 0),
        ]);
        app.cursor = 1;
        assert_eq!(cursor_id(&app), Some("id_c"));

        app.apply_tasks(vec![
            task("id_d", "delta", 0),
            task("id_a", "alpha", 0),
            task("id_c", "charlie", 0),
            task("id_b", "bravo", 0),
        ]);

        assert_eq!(app.cursor, 2, "charlie is now the third visible row");
        assert_eq!(cursor_id(&app), Some("id_c"));
    }

    #[test]
    fn a_refresh_that_re_sorts_the_list_keeps_the_cursor_on_the_same_task() {
        // Same invariant when it is the *data* that moves the rows: sorting by
        // size, a task that grew overtakes the one the cursor is on.
        let mut app = App::new(vec![
            task("id_a", "alpha", 100),
            task("id_b", "bravo", 200),
            task("id_c", "charlie", 300),
        ]);
        app.view.sort_key = view::SortKey::Size;
        app.cursor = 0;
        assert_eq!(cursor_id(&app), Some("id_a"));

        app.apply_tasks(vec![
            task("id_a", "alpha", 100),
            task("id_b", "bravo", 200),
            task("id_c", "charlie", 50),
        ]);

        assert_eq!(app.cursor, 1, "charlie shrank past alpha");
        assert_eq!(cursor_id(&app), Some("id_a"));
    }

    #[test]
    fn a_refresh_keeps_the_cursor_on_the_same_task_inside_a_filtered_view() {
        // The cursor is a position in the *visible* list, so the reconciliation
        // has to search the visible list, not `tasks`.
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        app.cursor_to_last();
        assert_eq!(app.cursor, 1);
        let kept = cursor_id(&app).expect("a seeding task").to_string();
        let dropped = app.tasks[app.visible()[0]].id.clone();

        // The *other* seeding task disappears, so the row number has to change
        // for the cursor to stay on the same torrent.
        let refreshed: Vec<Task> = fixture_tasks()
            .into_iter()
            .filter(|task| task.id != dropped)
            .collect();
        app.apply_tasks(refreshed);

        assert_eq!(app.cursor, 0);
        assert_eq!(cursor_id(&app), Some(kept.as_str()));
    }

    #[test]
    fn a_refresh_drops_selections_for_tasks_that_no_longer_exist() {
        let mut app = app_with(3);
        app.selected.insert("id_000".to_string());
        app.selected.insert("id_002".to_string());

        app.apply_tasks(vec![task("id_000", "task 000", 0), task("id_001", "x", 0)]);

        assert_eq!(selected_ids(&app), ["id_000"], "id_002 is gone");
        assert_eq!(app.selected_count(), 1);
    }

    #[test]
    fn a_refresh_that_removes_the_cursor_task_clamps_the_cursor_into_the_list() {
        // Cursor on the last row, and that task is what vanished.
        let mut app = app_with(4);
        app.cursor_to_last();
        assert_eq!(cursor_id(&app), Some("id_003"));

        app.apply_tasks(vec![
            task("id_000", "task 000", 0),
            task("id_001", "task 001", 0),
        ]);

        assert_eq!(app.cursor, 1, "clamped to the new last row");
        assert_eq!(cursor_id(&app), Some("id_001"));
    }

    #[test]
    fn a_refresh_that_removes_the_cursor_task_mid_list_holds_the_row_number() {
        // Nothing to follow, so the cursor stays where the user's eye is rather
        // than jumping to the top of the list.
        let mut app = app_with(5);
        app.cursor = 2;
        let mut refreshed: Vec<Task> = app.tasks.clone();
        refreshed.remove(2);

        app.apply_tasks(refreshed);

        assert_eq!(app.cursor, 2);
        assert_eq!(cursor_id(&app), Some("id_003"));
    }

    #[test]
    fn a_refresh_that_empties_the_list_leaves_a_valid_cursor() {
        let mut app = app_with(5);
        app.cursor_to_last();
        app.handle_key(press(KeyCode::Char('a')));

        app.apply_tasks(Vec::new());

        assert_eq!(app.cursor, 0);
        assert!(app.cursor_task().is_none());
        assert!(app.selected.is_empty(), "every ID is stale now");
    }

    #[test]
    fn a_refresh_is_ignored_entirely_while_the_confirmation_dialog_is_open() {
        // The delete plan on screen is a snapshot the user is reading. Merging
        // a refresh into it would make the dialog describe something other than
        // what is about to be deleted.
        let mut app = app_with(3);
        app.cursor = 2;
        app.selected.insert("id_002".to_string());
        app.set_error("nas unreachable");
        app.mode = Mode::Confirm;
        let before = format!("{app:?}");

        app.apply_event(AppEvent::Tasks(vec![task("id_009", "brand new", 0)]));

        assert_eq!(format!("{app:?}"), before, "nothing may change in Confirm");
        assert_eq!(app.tasks.len(), 3);
        assert_eq!(cursor_id(&app), Some("id_002"));
        assert_eq!(selected_ids(&app), ["id_002"]);
        assert_eq!(app.error.as_deref(), Some("nas unreachable"));
    }

    #[test]
    fn a_refresh_lands_again_as_soon_as_the_dialog_closes() {
        let mut app = app_with(3);
        app.mode = Mode::Confirm;
        app.apply_event(AppEvent::Tasks(vec![task("id_009", "brand new", 0)]));
        assert_eq!(app.tasks.len(), 3);

        app.mode = Mode::Normal;
        app.apply_event(AppEvent::Tasks(vec![task("id_009", "brand new", 0)]));
        assert_eq!(app.tasks.len(), 1);
        assert_eq!(cursor_id(&app), Some("id_009"));
    }

    // ---- the non-fatal error banner ----------------------------------------

    #[test]
    fn a_failed_poll_raises_a_banner_without_disturbing_anything_else() {
        let mut app = app_with(3);
        app.cursor = 1;
        app.selected.insert("id_001".to_string());

        app.apply_event(AppEvent::Error("refresh failed: connection refused".into()));

        assert_eq!(
            app.error.as_deref(),
            Some("refresh failed: connection refused")
        );
        assert!(
            !app.should_quit(),
            "a poll failure must not end the program"
        );
        assert_eq!(app.tasks.len(), 3, "the last good list is still on screen");
        assert_eq!(app.cursor, 1);
        assert_eq!(selected_ids(&app), ["id_001"]);
    }

    #[test]
    fn the_next_successful_refresh_takes_the_banner_down() {
        let mut app = app_with(1);
        app.apply_event(AppEvent::Error("refresh failed: timed out".into()));
        assert!(app.error.is_some());

        app.apply_event(AppEvent::Tasks(vec![task("id_000", "task 000", 0)]));
        assert!(app.error.is_none(), "the UI recovers on its own");
    }

    #[test]
    fn an_error_banner_does_not_replace_the_status_message() {
        // They are different things: one is what the program is doing, the
        // other is what went wrong. The footer decides which to show.
        let mut app = App::default();
        app.set_status("nas.local as eduard");
        app.set_error("refresh failed");
        assert_eq!(app.status_message.as_deref(), Some("nas.local as eduard"));
        app.clear_error();
        assert_eq!(app.status_message.as_deref(), Some("nas.local as eduard"));
    }

    #[test]
    fn op_progress_reports_the_item_and_how_far_through_the_batch_it_is() {
        let mut app = app_with(2);
        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 1,
            total: 3,
            item: done_item("task 000"),
        });
        let status = app.status_message.clone().expect("a progress line");
        assert!(status.contains("delete 1/3"), "{status}");
        assert!(status.contains("task 000: deleted"), "{status}");
    }

    #[test]
    fn op_progress_leaves_the_task_list_and_the_selection_alone() {
        // Reporting is reporting: an op event must not move the cursor or
        // disarm a row while a batch is running.
        let mut app = app_with(4);
        app.cursor = 2;
        app.toggle_selection();
        let (tasks, cursor, selected) = (app.tasks.clone(), app.cursor, app.selected.clone());

        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 1,
            total: 4,
            item: done_item("task 000"),
        });

        assert_eq!(app.tasks, tasks);
        assert_eq!(app.cursor, cursor);
        assert_eq!(app.selected, selected);
    }

    #[test]
    fn op_done_summarizes_only_the_categories_that_happened() {
        let mut app = app_with(1);
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 3,
            skipped: 0,
            failed: 0,
        });
        assert_eq!(
            app.status_message.as_deref(),
            Some("delete finished: 3 succeeded")
        );
    }

    // ---- the results modal ---------------------------------------------------
    //
    // The counts in the footer name neither the task nor the reason, and every
    // per-item line was overwritten by the next one. These are the tests that
    // the reasons survive the batch that produced them.

    /// Run a two-item batch through `apply_event`, the second item failing.
    fn batch_with_one_failure(app: &mut App) {
        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 1,
            total: 2,
            item: done_item("Good.Release"),
        });
        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 2,
            total: 2,
            item: failed_item("Bad.Release", "nothing at /downloads/Bad.Release"),
        });
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 1,
            skipped: 0,
            failed: 1,
        });
    }

    #[test]
    fn a_batch_with_a_failure_opens_the_results_modal_with_the_reason_in_it() {
        let mut app = app_with(2);
        batch_with_one_failure(&mut app);

        assert_eq!(app.mode, Mode::Results);
        let report = app.last_op_report().expect("a report");
        assert_eq!((report.succeeded, report.failed), (1, 1));
        assert_eq!(report.problems.len(), 1, "successes are not listed");
        assert_eq!(report.problems[0].title, "Bad.Release");
        assert_eq!(
            report.problems[0].outcome.problem(),
            Some("nothing at /downloads/Bad.Release")
        );

        // And the footer says how to get back to it once dismissed.
        let status = app.status_message.clone().expect("a summary");
        assert!(status.contains("1 failed"), "{status}");
        assert!(status.contains("v for the reasons"), "{status}");
    }

    #[test]
    fn a_skipped_item_is_reported_as_well_as_a_failed_one() {
        // A skip is not a success: the task is still on the NAS, and the reason
        // is the one that names `--no-delete-files`.
        let mut app = app_with(1);
        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 1,
            total: 1,
            item: skipped_item("Mixed.Root", "the task's files share no single top-level"),
        });
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 0,
            skipped: 1,
            failed: 0,
        });

        assert_eq!(app.mode, Mode::Results);
        let report = app.last_op_report().expect("a report");
        assert_eq!(report.problems.len(), 1);
        assert!(!report.problems[0].outcome.is_failure());
    }

    #[test]
    fn a_clean_batch_opens_nothing_and_offers_nothing_to_open() {
        let mut app = app_with(1);
        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Delete,
            done: 1,
            total: 1,
            item: done_item("Good.Release"),
        });
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 1,
            skipped: 0,
            failed: 0,
        });

        assert_eq!(app.mode, Mode::Normal);
        let status = app.status_message.clone().expect("a summary");
        assert!(!status.contains("v for"), "{status}");

        // `v` says so rather than opening an empty box.
        app.handle_key(press(KeyCode::Char('v')));
        assert_eq!(app.mode, Mode::Normal);
        let status = app.status_message.clone().expect("a message");
        assert!(status.contains("nothing to report"), "{status}");
    }

    #[test]
    fn v_reopens_the_last_report_after_it_was_dismissed() {
        let mut app = app_with(2);
        batch_with_one_failure(&mut app);

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.last_op_report().is_some(), "the report is kept");

        app.handle_key(press(KeyCode::Char('v')));
        assert_eq!(app.mode, Mode::Results);
    }

    #[test]
    fn v_with_no_batch_behind_it_says_so() {
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('v')));
        assert_eq!(app.mode, Mode::Normal);
        let status = app.status_message.clone().expect("a message");
        assert!(status.contains("no operation has finished"), "{status}");
    }

    #[test]
    fn the_results_modal_scrolls_and_does_not_close_on_an_unknown_key() {
        let mut app = app_with(2);
        batch_with_one_failure(&mut app);
        // One problem, two body lines.
        assert_eq!(app.results_line_count(), 2);

        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.results_scroll(), 1);
        app.handle_key(press(KeyCode::Down));
        assert_eq!(app.results_scroll(), 1, "clamped to the last line");
        app.handle_key(press(KeyCode::Home));
        assert_eq!(app.results_scroll(), 0);
        app.handle_key(press(KeyCode::End));
        assert_eq!(app.results_scroll(), 1);

        // Unlike the help overlay, a stray key must not take the reasons away.
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Results);
        assert!(app.pending_delete().is_none(), "and it does nothing else");
    }

    #[test]
    fn the_next_batch_starts_from_no_problems_at_all() {
        // `done == 1` is where a batch begins; without the reset the second
        // batch would report the first one's failures as its own.
        let mut app = app_with(2);
        batch_with_one_failure(&mut app);
        app.close_results();

        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Pause,
            done: 1,
            total: 1,
            item: done_item("Good.Release"),
        });
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Pause,
            succeeded: 1,
            skipped: 0,
            failed: 0,
        });

        let report = app.last_op_report().expect("a report");
        assert_eq!(report.op, OpKind::Pause);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn a_finished_batch_never_replaces_a_modal_the_user_is_reading() {
        // A confirmation dialog swapped out from under a `y` is the one thing
        // this program must never do.
        let mut app = app_with(2);
        app.begin_delete();
        assert_eq!(app.mode, Mode::Confirm);

        app.apply_event(AppEvent::OpProgress {
            op: OpKind::Pause,
            done: 1,
            total: 1,
            item: failed_item("Other.Release", "could not pause it"),
        });
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Pause,
            succeeded: 0,
            skipped: 0,
            failed: 1,
        });

        assert_eq!(app.mode, Mode::Confirm);
        // Kept all the same — `v` reaches it once the dialog is gone.
        assert!(app.last_op_report().expect("a report").has_problems());
    }

    #[test]
    fn a_failure_in_the_batch_is_marked_in_the_summary() {
        // A silent "1 failed" beside "3 succeeded" is how a failed delete goes
        // unnoticed.
        let summary = op_summary(OpKind::Delete, 3, 1, 1);
        assert_eq!(
            summary,
            "⚠ delete finished: 3 succeeded, 1 skipped, 1 failed"
        );
        assert!(!op_summary(OpKind::Delete, 3, 1, 0).starts_with('⚠'));
    }

    #[test]
    fn a_batch_that_did_nothing_says_so_rather_than_reporting_zeroes() {
        assert_eq!(
            op_summary(OpKind::Delete, 0, 0, 0),
            "delete finished: nothing to do"
        );
        // A dry run is entirely skips, and must not read as a delete.
        let dry = op_summary(OpKind::Delete, 0, 4, 0);
        assert_eq!(dry, "delete finished: 4 skipped");
        assert!(!dry.contains("succeeded"), "{dry}");
    }

    #[test]
    fn the_op_summary_names_the_operation() {
        for op in [OpKind::Delete, OpKind::Pause, OpKind::Resume] {
            assert!(
                op_summary(op, 1, 0, 0).starts_with(op.label()),
                "{}",
                op.label()
            );
        }
    }

    #[test]
    fn an_op_report_goes_to_the_status_message_not_the_error_banner() {
        // The banner is cleared by the next successful refresh, and an op asks
        // for one the moment it finishes — the report would vanish.
        let mut app = app_with(1);
        app.apply_event(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 0,
            skipped: 0,
            failed: 2,
        });
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|s| s.contains("2 failed")),
            "{:?}",
            app.status_message
        );

        // And it survives the refresh that follows.
        app.apply_event(AppEvent::Tasks(vec![task("id_000", "task 000", 0)]));
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|s| s.contains("2 failed")),
            "{:?}",
            app.status_message
        );
    }

    // ---- manual refresh ----------------------------------------------------

    #[test]
    fn r_asks_for_a_refresh_exactly_once_per_press() {
        let mut app = app_with(2);
        assert!(!app.take_refresh_request(), "nothing asked for yet");

        app.handle_key(press(KeyCode::Char('r')));
        assert!(app.take_refresh_request());
        assert!(
            !app.take_refresh_request(),
            "taking the request must clear it"
        );
    }

    #[test]
    fn repeated_r_presses_coalesce_into_one_request() {
        // Leaning on the key must not queue a round trip per keystroke.
        let mut app = app_with(2);
        for _ in 0..5 {
            app.handle_key(press(KeyCode::Char('r')));
        }
        assert!(app.take_refresh_request());
        assert!(!app.take_refresh_request());
    }

    #[test]
    fn r_does_not_move_the_cursor_or_the_selection() {
        let mut app = app_with(3);
        app.cursor = 1;
        app.handle_key(press(KeyCode::Char('r')));
        assert_eq!(app.cursor, 1);
        assert!(app.selected.is_empty());
    }

    // ---- the delete confirmation -------------------------------------------
    //
    // `d` is the key that loses data. Every test here is about one of two
    // questions: *what* did it snapshot, and *how hard is it to confirm*.

    /// The IDs a plan covers, in snapshot order.
    fn plan_ids(plan: &DeletePlan) -> Vec<&str> {
        plan.items.iter().map(|item| item.id.as_str()).collect()
    }

    #[test]
    fn d_with_nothing_selected_confirms_the_row_under_the_cursor() {
        let mut app = app_with(4);
        app.cursor = 2;

        app.handle_key(press(KeyCode::Char('d')));

        assert_eq!(app.mode, Mode::Confirm);
        let plan = app.pending_delete().expect("a dialog is open");
        assert_eq!(plan_ids(plan), ["id_002"]);
        assert!(
            app.selected.is_empty(),
            "the cursor fallback must not arm anything"
        );
    }

    #[test]
    fn d_with_a_selection_confirms_the_selection_and_ignores_the_cursor() {
        let mut app = app_with(4);
        app.selected.insert("id_000".to_string());
        app.selected.insert("id_003".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('d')));

        let plan = app.pending_delete().expect("a dialog is open");
        assert_eq!(plan_ids(plan), ["id_000", "id_003"]);
        assert!(
            !plan_ids(plan).contains(&"id_001"),
            "the cursor row must not be added to a selection"
        );
    }

    #[test]
    fn d_on_an_empty_table_opens_no_dialog() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_delete().is_none());
        assert!(app.take_confirmed_delete().is_none());
    }

    #[test]
    fn d_with_every_row_hidden_opens_no_dialog() {
        // A filter that hides everything leaves no cursor row to fall back to.
        let mut app = App::new(fixture_tasks());
        app.view.search = "no-such-task".to_string();
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_delete().is_none());
    }

    #[test]
    fn d_with_a_stale_selection_falls_back_to_the_cursor_row() {
        // Selected IDs that name nothing are not a selection.
        let mut app = app_with(2);
        app.selected.insert("ghost".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(
            plan_ids(app.pending_delete().expect("a dialog")),
            ["id_001"]
        );
    }

    #[test]
    fn the_plan_a_dialog_shows_is_a_snapshot_of_what_was_selected() {
        // Refusals are part of the snapshot, not an aborted batch: dbid_013's
        // files share no common root and must appear as a skip.
        let mut app = App::new(fixture_tasks());
        for id in ["dbid_001", "dbid_010"] {
            app.selected.insert(id.to_string());
        }
        app.handle_key(press(KeyCode::Char('d')));

        let plan = app.pending_delete().expect("a dialog is open");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.deletable().count(), 1);
        assert_eq!(plan.refused().count(), 1);
        assert_eq!(plan.total_size(), fixture_task("dbid_001").size);
    }

    // ---- one batch at a time ------------------------------------------------

    #[test]
    fn d_while_a_batch_is_running_never_opens_a_dialog_it_cannot_honour() {
        // The refusal has to happen *before* the user commits. Taking the plan
        // and then refusing dropped it silently: the footer said so for the few
        // milliseconds before the running batch's next progress event
        // overwrote the line, and the user walked away believing their second
        // delete had run.
        let mut app = app_with(2);
        app.set_op_in_flight(true);

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Normal, "no modal may open");
        assert!(app.pending_delete().is_none());
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|status| status.contains("still running")),
            "{:?}",
            app.status_message
        );
        assert!(app.take_confirmed_delete().is_none());
    }

    #[test]
    fn p_and_u_while_a_batch_is_running_are_refused_the_same_way() {
        for key in ['p', 'u'] {
            let mut app = app_with(2);
            app.set_op_in_flight(true);
            app.handle_key(press(KeyCode::Char(key)));
            assert!(
                app.take_requested_op().is_none(),
                "{key} must not queue an operation on top of a live batch"
            );
            assert!(
                app.status_message
                    .as_deref()
                    .is_some_and(|status| status.contains("still running")),
                "{key}: {:?}",
                app.status_message
            );
        }
    }

    #[test]
    fn the_refusal_lifts_when_the_batch_finishes() {
        // The flag is pushed in by the event loop before every draw, so a
        // finished batch has to re-enable the keys with no other state change.
        let mut app = app_with(2);
        app.set_op_in_flight(true);
        app.handle_key(press(KeyCode::Char('d')));
        assert!(app.pending_delete().is_none());

        app.set_op_in_flight(false);
        assert!(!app.op_in_flight());
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Confirm);
        assert!(app.pending_delete().is_some());
    }

    #[test]
    fn the_dialog_opens_with_cancel_focused() {
        // The single most important line in this file: an `Enter` reflex must
        // not delete anything.
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.confirm_focus(), ConfirmFocus::Cancel);

        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            app.take_confirmed_delete().is_none(),
            "Enter on the default focus must cancel"
        );
    }

    #[test]
    fn enter_confirms_only_after_the_focus_is_moved_to_delete() {
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Right));
        assert_eq!(app.confirm_focus(), ConfirmFocus::Delete);

        app.handle_key(press(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        let plan = app.take_confirmed_delete().expect("the plan was confirmed");
        assert_eq!(plan_ids(&plan), ["id_000"]);
    }

    #[test]
    fn every_focus_key_moves_between_the_two_buttons() {
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('d')));
        for key in [
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('h'),
            KeyCode::Char('l'),
        ] {
            let before = app.confirm_focus();
            app.handle_key(press(key));
            assert_ne!(app.confirm_focus(), before, "{key:?}");
        }
    }

    #[test]
    fn y_confirms_from_either_focus() {
        for focus_key in [None, Some(KeyCode::Tab)] {
            let mut app = app_with(1);
            app.handle_key(press(KeyCode::Char('d')));
            if let Some(key) = focus_key {
                app.handle_key(press(key));
            }
            app.handle_key(press(KeyCode::Char('y')));

            assert_eq!(app.mode, Mode::Normal);
            assert!(app.pending_delete().is_none());
            assert!(app.take_confirmed_delete().is_some(), "{focus_key:?}");
        }
    }

    #[test]
    fn n_esc_and_q_all_cancel_without_confirming_anything() {
        for key in [KeyCode::Char('n'), KeyCode::Esc, KeyCode::Char('q')] {
            let mut app = app_with(1);
            app.handle_key(press(KeyCode::Char('d')));
            app.handle_key(press(key));

            assert_eq!(app.mode, Mode::Normal, "{key:?}");
            assert!(app.pending_delete().is_none(), "{key:?}");
            assert!(app.take_confirmed_delete().is_none(), "{key:?}");
            assert!(
                !app.should_quit(),
                "{key:?} must close the dialog, not the program"
            );
        }
    }

    #[test]
    fn ctrl_c_still_leaves_the_program_from_inside_the_dialog() {
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn an_unbound_key_in_the_dialog_changes_nothing() {
        // Least of all does it count as a confirmation.
        let mut app = app_with(2);
        app.handle_key(press(KeyCode::Char('d')));
        let before = format!("{app:?}");
        for key in [
            KeyCode::Char('x'),
            KeyCode::Char('a'),
            KeyCode::Char(' '),
            KeyCode::Char('r'),
            KeyCode::Char('/'),
            KeyCode::Char('d'),
        ] {
            app.handle_key(press(key));
            assert_eq!(format!("{app:?}"), before, "{key:?}");
        }
        assert_eq!(app.mode, Mode::Confirm);
    }

    #[test]
    fn cancelling_leaves_the_table_the_selection_and_the_cursor_exactly_as_they_were() {
        let mut app = app_with(4);
        app.cursor = 2;
        app.selected.insert("id_001".to_string());
        let before = format!("{app:?}");

        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Esc));

        assert_eq!(format!("{app:?}"), before);
    }

    #[test]
    fn confirming_deletes_nothing_here_and_hands_the_plan_over_exactly_once() {
        // The dialog performs no I/O: the plan is parked for the event loop,
        // and the task list is untouched until a refresh says otherwise.
        let mut app = app_with(3);
        app.selected.insert("id_001".to_string());
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));

        assert_eq!(app.tasks.len(), 3, "nothing is removed locally");
        assert_eq!(selected_ids(&app), ["id_001"]);
        assert_eq!(
            plan_ids(&app.take_confirmed_delete().expect("one plan")),
            ["id_001"]
        );
        assert!(
            app.take_confirmed_delete().is_none(),
            "taking the plan must clear it"
        );
    }

    #[test]
    fn reopening_the_dialog_starts_from_cancel_and_the_top_of_the_list() {
        let mut app = app_with(3);
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Tab));
        app.handle_key(press(KeyCode::Esc));

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.confirm_focus(), ConfirmFocus::Cancel);
        assert_eq!(app.confirm_scroll(), 0);
    }

    #[test]
    fn the_dialog_body_scrolls_and_clamps_at_both_ends() {
        // Two body lines per item, so a twenty-task plan is well past any
        // modal height.
        let mut app = app_with(20);
        app.toggle_select_all_visible();
        app.handle_key(press(KeyCode::Char('d')));
        let lines = app.confirm_line_count();
        assert_eq!(lines, 40);

        app.handle_key(press(KeyCode::Down));
        app.handle_key(press(KeyCode::Char('j')));
        assert_eq!(app.confirm_scroll(), 2);

        app.handle_key(press(KeyCode::Up));
        app.handle_key(press(KeyCode::Char('k')));
        app.handle_key(press(KeyCode::Char('k')));
        assert_eq!(app.confirm_scroll(), 0, "scrolling up clamps at the top");

        app.handle_key(press(KeyCode::End));
        assert_eq!(app.confirm_scroll(), lines - 1);
        for _ in 0..5 {
            app.handle_key(press(KeyCode::PageDown));
        }
        assert_eq!(app.confirm_scroll(), lines - 1, "and at the bottom");

        app.handle_key(press(KeyCode::Home));
        assert_eq!(app.confirm_scroll(), 0);
    }

    #[test]
    fn scrolling_a_plan_that_fits_goes_nowhere() {
        let mut app = app_with(1);
        app.handle_key(press(KeyCode::Char('d')));
        for _ in 0..5 {
            app.handle_key(press(KeyCode::Down));
        }
        assert_eq!(app.confirm_scroll(), 1, "two lines, so one line of travel");
    }

    #[test]
    fn the_delete_options_ride_along_with_the_app() {
        let mut app = app_with(1).with_delete_options(DeleteOptions::dry_run());
        assert!(app.delete_options.dry_run);
        assert!(app.delete_options.delete_files);

        // ...and a dry run still opens a dialog and still confirms.
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('y')));
        assert!(app.take_confirmed_delete().is_some());
    }

    // ---- pause and resume --------------------------------------------------
    //
    // The one thing that can go wrong here without anyone noticing is *which*
    // tasks the key aimed at, so that is what these test.

    /// The request `p` / `u` parked, if any.
    fn requested(app: &mut App) -> Option<TaskOpRequest> {
        app.take_requested_op()
    }

    /// The ids a request names, in order — which task a key aimed at is the
    /// whole subject of these tests.
    fn ids(request: &TaskOpRequest) -> Vec<&str> {
        request.tasks.iter().map(|task| task.id.as_str()).collect()
    }

    #[test]
    fn p_with_nothing_selected_acts_on_the_row_under_the_cursor() {
        let mut app = app_with(4);
        app.cursor = 2;

        app.handle_key(press(KeyCode::Char('p')));

        let request = requested(&mut app).expect("a pause was requested");
        assert_eq!(request.op, TaskOp::Pause);
        assert_eq!(ids(&request), ["id_002"]);
        assert!(
            app.selected.is_empty(),
            "the cursor fallback must not arm anything"
        );
    }

    #[test]
    fn u_requests_a_resume_for_the_same_target() {
        let mut app = app_with(4);
        app.cursor = 3;

        app.handle_key(press(KeyCode::Char('u')));

        let request = requested(&mut app).expect("a resume was requested");
        assert_eq!(request.op, TaskOp::Resume);
        assert_eq!(ids(&request), ["id_003"]);
    }

    #[test]
    fn p_with_a_selection_acts_on_the_selection_and_ignores_the_cursor() {
        let mut app = app_with(4);
        app.selected.insert("id_000".to_string());
        app.selected.insert("id_003".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('p')));

        let request = requested(&mut app).expect("a pause was requested");
        assert_eq!(ids(&request), ["id_000", "id_003"]);
        assert!(
            !ids(&request).contains(&"id_001"),
            "the cursor row must not be added to a selection"
        );
    }

    #[test]
    fn p_on_an_empty_table_requests_nothing_at_all() {
        // An empty batch would be a round trip that can only report "nothing to
        // do".
        let mut app = App::default();
        app.handle_key(press(KeyCode::Char('p')));
        assert!(requested(&mut app).is_none());
        assert_eq!(app.status_message.as_deref(), Some("nothing to pause"));

        app.handle_key(press(KeyCode::Char('u')));
        assert!(requested(&mut app).is_none());
        assert_eq!(app.status_message.as_deref(), Some("nothing to resume"));
    }

    #[test]
    fn p_with_every_row_hidden_requests_nothing() {
        // A filter that hides everything leaves no cursor row to fall back to.
        let mut app = App::new(fixture_tasks());
        app.view.search = "no-such-task".to_string();
        app.handle_key(press(KeyCode::Char('p')));
        assert!(requested(&mut app).is_none());
    }

    #[test]
    fn a_selected_task_a_filter_is_hiding_is_still_paused() {
        // The selection is what is armed, not what is on screen — the same rule
        // `d` follows, and the reason both keys share `target_tasks`.
        let mut app = App::new(fixture_tasks());
        app.selected.insert("dbid_004".to_string()); // paused
        app.view.filter = StatusFilter::Seeding;
        assert!(
            !app.visible()
                .iter()
                .any(|&index| app.tasks[index].id == "dbid_004")
        );

        app.handle_key(press(KeyCode::Char('u')));
        assert_eq!(ids(&requested(&mut app).expect("a resume")), ["dbid_004"]);
    }

    #[test]
    fn the_target_is_listed_in_on_screen_order_not_dsm_order() {
        // The confirmation dialog lists these rows back for checking, so under
        // any non-default sort a dialog ordered by `self.tasks` would not match
        // the table the user is looking at — which defeats the one job that
        // screen has.
        let mut app = App::new(fixture_tasks());
        app.view.sort_key = crate::view::SortKey::Name;
        app.view.sort_dir = crate::view::SortDir::Desc;
        for task in &app.tasks {
            app.selected.insert(task.id.clone());
        }

        let on_screen: Vec<String> = app
            .visible()
            .into_iter()
            .map(|index| app.tasks[index].id.clone())
            .collect();
        let targeted: Vec<String> = app
            .target_tasks()
            .into_iter()
            .map(|task| task.id.clone())
            .collect();

        assert_eq!(targeted, on_screen);
        assert_ne!(
            targeted,
            app.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            "the fixture must actually re-order under this sort, or this proves nothing"
        );
        // And the plan the dialog is built from inherits that order.
        app.handle_key(press(KeyCode::Char('d')));
        let plan = app.pending_delete().expect("a plan");
        assert_eq!(
            plan.items.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
            on_screen
        );
    }

    #[test]
    fn a_selected_row_a_filter_is_hiding_follows_the_visible_ones() {
        // It has no on-screen position to sort into, but it is still armed and
        // must still be listed.
        let mut app = App::new(fixture_tasks());
        app.view.filter = StatusFilter::Seeding;
        let hidden = "dbid_004"; // paused
        let visible_ids: Vec<String> = app
            .visible()
            .into_iter()
            .map(|index| app.tasks[index].id.clone())
            .collect();
        assert!(!visible_ids.contains(&hidden.to_string()));
        assert!(!visible_ids.is_empty());

        app.selected.insert(hidden.to_string());
        for id in &visible_ids {
            app.selected.insert(id.clone());
        }

        let targeted: Vec<String> = app
            .target_tasks()
            .into_iter()
            .map(|task| task.id.clone())
            .collect();
        assert_eq!(&targeted[..visible_ids.len()], &visible_ids[..]);
        assert_eq!(targeted.last().map(String::as_str), Some(hidden));
    }

    #[test]
    fn a_stale_selection_falls_back_to_the_cursor_row() {
        let mut app = app_with(2);
        app.selected.insert("ghost".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('p')));
        assert_eq!(ids(&requested(&mut app).expect("a pause")), ["id_001"]);
    }

    #[test]
    fn the_request_is_handed_over_exactly_once() {
        let mut app = app_with(2);
        app.handle_key(press(KeyCode::Char('p')));
        assert!(requested(&mut app).is_some());
        assert!(
            requested(&mut app).is_none(),
            "taking the request must clear it"
        );
    }

    #[test]
    fn pausing_changes_nothing_about_the_tasks_or_the_selection_locally() {
        // `p` performs no I/O and predicts no outcome: the table only changes
        // when the refresh that follows the batch says so.
        let mut app = app_with(3);
        app.selected.insert("id_001".to_string());
        app.handle_key(press(KeyCode::Char('p')));

        assert_eq!(app.tasks.len(), 3);
        assert_eq!(selected_ids(&app), ["id_001"]);
        assert_eq!(
            app.tasks[1].status,
            TaskStatus::Paused,
            "no status is guessed at locally"
        );
    }

    #[test]
    fn p_and_u_are_normal_mode_keys_only() {
        // In the search box they are letters; in the confirmation modal they
        // are not bound at all.
        let mut app = App::new(fixture_tasks());
        app.handle_key(press(KeyCode::Char('/')));
        app.handle_key(press(KeyCode::Char('p')));
        app.handle_key(press(KeyCode::Char('u')));
        assert_eq!(app.view.search, "pu");
        assert!(requested(&mut app).is_none());

        let mut app = app_with(2);
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('p')));
        app.handle_key(press(KeyCode::Char('u')));
        assert_eq!(app.mode, Mode::Confirm);
        assert!(requested(&mut app).is_none());
    }

    // ---- fixture mode ------------------------------------------------------

    #[test]
    fn the_checked_in_fixture_loads_straight_off_disk() {
        // The same file the model tests parse, through the same envelope
        // reader — `--fixture` is not a second, laxer parser.
        let app = App::from_fixture(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/task_list.json"
        )))
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
