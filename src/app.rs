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
use crate::event::{AppEvent, OpKind};
use crate::model::{Task, TaskList};
use crate::ui::dialog;
use crate::view::{self, View};

/// Rows a `PageUp`/`PageDown` moves before the first frame has been drawn.
///
/// The real page is the height of the table body and is pushed in by the event
/// loop after each draw ([`App::set_page_size`]); this is only what the very
/// first key press uses.
pub const DEFAULT_PAGE_SIZE: usize = 20;

/// What the UI is currently doing, and therefore which keys mean what.
///
/// [`Mode::Normal`], [`Mode::Search`] and [`Mode::Confirm`] are reachable; the
/// help overlay lands in Task 17.
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
/// Carries **task IDs**, resolved from the selection (or the cursor row) at the
/// moment the key was pressed — the same reason the selection set itself holds
/// IDs: a refresh that reorders the table between the key press and the call
/// must not move the operation onto a different torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOpRequest {
    /// [`OpKind::Pause`] or [`OpKind::Resume`]; a delete goes through the
    /// confirmation dialog instead.
    pub op: OpKind,
    pub ids: Vec<String>,
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
    /// What a confirmed delete is allowed to do — the resolved `delete_files`
    /// and `dry_run` settings. The confirmation modal states both.
    pub delete_options: DeleteOptions,
    /// One line of feedback shown in the footer: the result of the last
    /// operation or the startup banner.
    pub status_message: Option<String>,
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
    /// the event loop drains it with [`App::take_confirmed_delete`]; Task 15
    /// hangs the actual three-phase delete off that hook. Keeping it a value
    /// means the whole confirmation flow stays testable without a runtime, a
    /// client or a NAS.
    confirmed_delete: Option<DeletePlan>,
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
            delete_options: DeleteOptions::default(),
            status_message: None,
            error: None,
            page_size: DEFAULT_PAGE_SIZE,
            refresh_requested: false,
            search_backup: None,
            pending_delete: None,
            confirm_focus: ConfirmFocus::default(),
            confirm_scroll: 0,
            requested_op: None,
            confirmed_delete: None,
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
    /// Offline verification hangs off this: with no NAS in reach the table,
    /// multi-select and the sort/filter keys have no other way to be exercised,
    /// and a 500-task file is the cheapest possible render-performance check.
    /// Nothing polls in this mode — the list is whatever the file said.
    pub fn from_fixture(path: &Path) -> Result<Self> {
        Ok(Self::new(read_fixture(path)?))
    }

    /// Set what a confirmed delete may do (from the merged configuration).
    pub fn with_delete_options(mut self, options: DeleteOptions) -> Self {
        self.delete_options = options;
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
            AppEvent::OpProgress {
                op,
                done,
                total,
                detail,
            } => self.set_status(format!("{} {done}/{total} · {detail}", op.label())),
            AppEvent::OpDone {
                op,
                succeeded,
                skipped,
                failed,
            } => self.set_status(op_summary(op, succeeded, skipped, failed)),
        }
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

        let cursor_id = self.cursor_task().map(|task| task.id.clone());

        self.tasks = tasks;
        let live: HashSet<&str> = self.tasks.iter().map(|task| task.id.as_str()).collect();
        self.selected.retain(|id| live.contains(id.as_str()));

        self.cursor = match cursor_id {
            Some(id) => self
                .visible()
                .iter()
                .position(|&index| self.tasks[index].id == id)
                // The task is gone: hold the row number rather than jumping to
                // the top, so the cursor stays where the user's eye is.
                .unwrap_or(self.cursor),
            None => self.cursor,
        };
        self.clamp_cursor();

        // A tick that got through is the proof the last failure has passed.
        self.clear_error();
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
        let cursor_id = self.cursor_task().map(|task| task.id.clone());

        change(&mut self.view);

        if let Some(id) = cursor_id
            && let Some(row) = self
                .visible()
                .iter()
                .position(|&index| self.tasks[index].id == id)
        {
            self.cursor = row;
        }
        self.clamp_cursor();
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

    // ---- the delete confirmation -------------------------------------------
    //
    // `d` never deletes. It takes a **snapshot** ([`DeletePlan`]) and opens a
    // modal describing it; only `y`, or `Enter` on a deliberately re-focused
    // Delete button, records the intent. Even then nothing here touches the
    // network — [`App::take_confirmed_delete`] hands the plan to the event loop,
    // and Task 15 owns the three-phase execution.

    /// Open the confirmation modal for the current target (`d`).
    ///
    /// The target is **the selection when there is one, and the row under the
    /// cursor otherwise** — a `d` aimed at a row the user is looking at must
    /// work without arming it first. A plan with no items (an empty table)
    /// opens no dialog at all: there is nothing to confirm.
    pub fn begin_delete(&mut self) {
        let plan = self.delete_target();
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

    /// Snapshot whatever `d` would act on right now.
    fn delete_target(&self) -> DeletePlan {
        DeletePlan::snapshot(self.target_tasks())
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
    fn target_tasks(&self) -> Vec<&Task> {
        if self.selected_count() > 0 {
            self.selected_tasks().collect()
        } else {
            self.cursor_task().into_iter().collect()
        }
    }

    /// The IDs [`App::target_tasks`] resolves to.
    pub fn op_target_ids(&self) -> Vec<String> {
        self.target_tasks()
            .into_iter()
            .map(|task| task.id.clone())
            .collect()
    }

    // ---- pause and resume ---------------------------------------------------
    //
    // Unlike `d` these need no confirmation: both are reversible by the other
    // key, and a modal in front of a reversible operation only teaches the user
    // to dismiss modals. They still perform **no I/O here** — the request is
    // parked for the event loop exactly as a confirmed delete is.

    /// Pause the current target (`p`).
    pub fn pause_target(&mut self) {
        self.request_task_op(OpKind::Pause);
    }

    /// Resume the current target (`u`).
    pub fn resume_target(&mut self) {
        self.request_task_op(OpKind::Resume);
    }

    /// Record a pause/resume for the event loop to run.
    ///
    /// An empty target — an empty table, or a filter that hides everything — is
    /// a **no-op with a message**, never an empty batch: a round trip that can
    /// only report "nothing to do" is not worth making.
    fn request_task_op(&mut self, op: OpKind) {
        let ids = self.op_target_ids();
        if ids.is_empty() {
            self.set_status(format!("nothing to {}", op.label()));
            return;
        }

        tracing::info!(
            op = op.label(),
            tasks = ids.len(),
            "requesting an operation"
        );
        let plural = if ids.len() == 1 { "task" } else { "tasks" };
        self.set_status(format!(
            "{} requested for {} {plural}",
            op.label(),
            ids.len()
        ));
        self.requested_op = Some(TaskOpRequest { op, ids });
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
        self.confirm_scroll = if delta < 0 {
            self.confirm_scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.confirm_scroll.saturating_add(delta.unsigned_abs())
        }
        .min(last);
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
    /// belongs to Task 15.
    pub fn confirm_delete(&mut self) {
        if let Some(plan) = self.pending_delete.take() {
            tracing::info!(
                items = plan.len(),
                deletable = plan.deletable().count(),
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
    /// The counterpart of [`App::take_refresh_request`], and the seam Task 15
    /// plugs the executor into.
    pub fn take_confirmed_delete(&mut self) -> Option<DeletePlan> {
        self.confirmed_delete.take()
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
            Mode::Search => self.handle_search_key(key),
            Mode::Confirm => self.handle_confirm_key(key),
            // The help overlay (Task 17) owns its keys. Until it lands nothing
            // can put the app into that mode; falling back to Normal means a
            // stray mode can never trap the user.
            Mode::Help => self.mode = Mode::Normal,
        }
    }

    /// Keys while browsing the table. The operations land in Tasks 14-16.
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

    /// One fixture task by id.
    fn fixture_task(id: &str) -> Task {
        fixture_tasks()
            .into_iter()
            .find(|task| task.id == id)
            .unwrap_or_else(|| panic!("fixture has no task {id}"))
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

    /// One synthetic task. Everything the reconciliation cares about is the ID;
    /// the title and size are what the sort and the footer read.
    fn task(id: &str, title: &str, size: u64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status: TaskStatus::Paused,
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

    // ---- refresh reconciliation --------------------------------------------
    //
    // The heart of Task 11. A refresh lands every few seconds, unannounced,
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
            detail: "task 000: deleted".into(),
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
            detail: "task 000: deleted".into(),
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
        for id in ["dbid_001", "dbid_013"] {
            app.selected.insert(id.to_string());
        }
        app.handle_key(press(KeyCode::Char('d')));

        let plan = app.pending_delete().expect("a dialog is open");
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.deletable().count(), 1);
        assert_eq!(plan.refused().count(), 1);
        assert_eq!(plan.total_size(), fixture_task("dbid_001").size);
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

    #[test]
    fn p_with_nothing_selected_acts_on_the_row_under_the_cursor() {
        let mut app = app_with(4);
        app.cursor = 2;

        app.handle_key(press(KeyCode::Char('p')));

        let request = requested(&mut app).expect("a pause was requested");
        assert_eq!(request.op, OpKind::Pause);
        assert_eq!(request.ids, ["id_002"]);
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
        assert_eq!(request.op, OpKind::Resume);
        assert_eq!(request.ids, ["id_003"]);
    }

    #[test]
    fn p_with_a_selection_acts_on_the_selection_and_ignores_the_cursor() {
        let mut app = app_with(4);
        app.selected.insert("id_000".to_string());
        app.selected.insert("id_003".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('p')));

        let request = requested(&mut app).expect("a pause was requested");
        assert_eq!(request.ids, ["id_000", "id_003"]);
        assert!(
            !request.ids.contains(&"id_001".to_string()),
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
        assert_eq!(
            requested(&mut app).expect("a resume").ids,
            ["dbid_004".to_string()]
        );
    }

    #[test]
    fn a_stale_selection_falls_back_to_the_cursor_row() {
        let mut app = app_with(2);
        app.selected.insert("ghost".to_string());
        app.cursor = 1;

        app.handle_key(press(KeyCode::Char('p')));
        assert_eq!(requested(&mut app).expect("a pause").ids, ["id_001"]);
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
