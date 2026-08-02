//! Background work and the events it reports back.
//!
//! Everything that talks to the NAS runs off the main loop and reports through
//! one [`mpsc`] channel of [`AppEvent`]. The UI therefore never blocks on the
//! network: a poll that takes ten seconds, or fails outright, costs a frame of
//! staleness and a banner, never a frozen terminal.
//!
//! Two rules hold here:
//!
//! * **The poller is non-fatal.** A failed tick sends [`AppEvent::Error`] and
//!   the loop keeps ticking; the next successful tick clears the banner. A poll
//!   failure must never end the poller or the UI — a NAS that goes away for a
//!   minute is ordinary.
//! * **There is no `Tick` event.** The poller drives *data*; redraws are driven
//!   by whatever arrives, so an idle program with an idle NAS still redraws
//!   once per refresh interval and no more.
//!
//! Op tasks report through the same channel as [`AppEvent::OpProgress`] /
//! [`AppEvent::OpDone`].
//!
//! ## The delete executor
//!
//! [`spawn_delete`] is where the plan's three-phase ordering is actually
//! carried out, one item at a time, off the UI thread. The *rules* are pure and
//! live in [`crate::delete`] ([`plan_delete_ops`] and [`ops_cancelled_by`]);
//! what is here is the I/O and the accounting:
//!
//! * a phase that fails **returns immediately**, so every later phase for that
//!   item is skipped and the DSM task survives still pointing at its data;
//! * a resolved path is re-run through [`validate_path`] immediately before the
//!   File Station call — the guard already ran when the snapshot was taken, and
//!   it is free to run again on the near side of the one call that cannot be
//!   undone;
//! * a path that is **not there** is a *skip*, not a failure, and the DSM task
//!   is still removed — **but only when the absence is explained**: this run
//!   deleted that path already, or the path came from the task's file list *and*
//!   the task never finished, so Download Station cleaned up its own partial
//!   data. A name guessed from the display title, or a finished task whose
//!   payload demonstrably existed, keeps its task instead: removing it would
//!   destroy the only pointer to data still on the volume. See
//!   [`decide_file_phase`];
//! * after the recursive delete reports success the path is looked up **once
//!   more**, and a path that is *still there* fails the item — the `status`
//!   payload's error count is the only other signal, and no real NAS response
//!   has been captured to confirm this client is reading it under the right
//!   name. A re-check that **errors, or does not answer at all**, fails the item
//!   too; only an answer this client cannot attribute to the path is read as
//!   confirmation — see [`decide_confirm_phase`] for why that one answer means
//!   opposite things before and after the delete;
//! * the pause phase resolves against a **live** status read rather than the
//!   snapshot's, because the snapshot's is as old as the confirmation dialog —
//!   and a live read that says nothing about the id asked for is read as
//!   "pause it", never as "it is idle";
//! * `--dry-run` reports what it would do and issues **no destructive call at
//!   all** — not even the existence check, which is a read but also a round trip
//!   the user did not ask for.
//!
//! [`plan_delete_ops`]: crate::delete::plan_delete_ops
//! [`ops_cancelled_by`]: crate::delete::ops_cancelled_by
//! [`validate_path`]: crate::delete::validate_path

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use crate::api::client::SynoClient;
use crate::api::download_station;
use crate::api::file_station::{self, PathInfo};
use crate::delete::{self, DeleteItem, DeleteOptions, DeletePlan, NameSource, Op};
use crate::error::Result;
use crate::model::Task;

/// How many events may queue before a sender has to wait.
///
/// Generous enough that a burst of per-item op progress never blocks an op
/// task, small enough that a UI wedged for minutes cannot accumulate an
/// unbounded backlog of stale task lists.
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Which long-running operation an [`AppEvent::OpProgress`] or
/// [`AppEvent::OpDone`] is talking about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Delete,
    Pause,
    Resume,
}

impl OpKind {
    /// How the operation is named in the footer.
    pub fn label(self) -> &'static str {
        match self {
            OpKind::Delete => "delete",
            OpKind::Pause => "pause",
            OpKind::Resume => "resume",
        }
    }

    /// How one *finished* item of the operation reads — the past tense of
    /// [`OpKind::label`], for the per-item progress line.
    pub fn past_tense(self) -> &'static str {
        match self {
            OpKind::Delete => "deleted",
            OpKind::Pause => "paused",
            OpKind::Resume => "resumed",
        }
    }
}

/// Anything that happens to the application other than a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// A fresh task list from the poller or a manual refresh.
    Tasks(Vec<Task>),
    /// A non-fatal failure to report in the footer. The program keeps running.
    Error(String),
    /// One step of a multi-task operation finished (Tasks 15 and 16).
    OpProgress {
        op: OpKind,
        /// How many items of the batch are done.
        done: usize,
        /// How many items the batch has.
        total: usize,
        /// What just happened, ready to show in the footer.
        detail: String,
    },
    /// A whole operation finished (Tasks 15 and 16).
    OpDone {
        op: OpKind,
        succeeded: usize,
        /// Items deliberately not acted on — an unresolvable delete path, a
        /// directory that was already gone.
        skipped: usize,
        failed: usize,
    },
}

/// The sending half handed to the poller and to every op task.
pub type Sender = mpsc::Sender<AppEvent>;
/// The receiving half owned by the main loop.
pub type Receiver = mpsc::Receiver<AppEvent>;

/// Build the one channel every background task reports through.
pub fn channel() -> (Sender, Receiver) {
    mpsc::channel(EVENT_CHANNEL_CAPACITY)
}

/// The `r` key's end of the wire: asks the poller to tick *now*.
///
/// A plain notification rather than a second channel, because a refresh has no
/// payload and coalescing several presses into one poll is the desired
/// behaviour — leaning on the key must not queue up ten round trips.
#[derive(Debug, Clone, Default)]
pub struct RefreshHandle(Arc<Notify>);

impl RefreshHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for an immediate refresh. Never blocks, and never fails: with no
    /// poller listening (offline `--fixture` mode) it is simply a no-op.
    pub fn request(&self) {
        self.0.notify_one();
    }

    /// Wait for the next request. A request made while a poll was in flight is
    /// remembered, so `r` pressed during a slow tick still forces the one after
    /// it.
    pub async fn requested(&self) {
        self.0.notified().await;
    }
}

/// Refresh the task list forever, reporting each result down `tx`.
///
/// Ticks on `interval` — the first tick is immediate, so the table fills in as
/// soon as the TUI starts — and in between whenever [`RefreshHandle::request`]
/// is called. The task ends only when the receiver is dropped (the UI quit) or
/// the handle is aborted; **a failed poll never ends it**.
pub fn spawn_poller(
    client: Arc<SynoClient>,
    interval: Duration,
    tx: Sender,
    refresh: RefreshHandle,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // A tick missed because a poll ran long should not be repaid with a
        // burst of back-to-back polls; the NAS only ever needs the newest list.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                () = refresh.requested() => {
                    // Manual refresh restarts the clock, so `r` does not leave
                    // a scheduled tick a fraction of a second behind it.
                    ticker.reset();
                }
            }

            if !poll_once(&client, &tx).await {
                tracing::debug!("the event channel closed; the poller is stopping");
                return;
            }
        }
    })
}

/// One poll. Returns whether the channel is still open.
///
/// Failure is data, not control flow: it becomes an [`AppEvent::Error`] for the
/// footer and the caller keeps polling.
async fn poll_once(client: &SynoClient, tx: &Sender) -> bool {
    let event = match download_station::list_tasks(client).await {
        Ok(tasks) => AppEvent::Tasks(tasks),
        Err(err) => {
            tracing::warn!(%err, "refreshing the task list failed");
            AppEvent::Error(format!("refresh failed: {err}"))
        }
    };
    tx.send(event).await.is_ok()
}

// ---------------------------------------------------------------------------
// Op tasks
// ---------------------------------------------------------------------------

/// How long a paused task is given to actually report itself paused.
///
/// Download Station accepting a `pause` says the request was queued, not that
/// the task has stopped writing. Short, because this blocks the delete of every
/// later item in the batch.
const PAUSE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the pause is re-checked.
const PAUSE_CONFIRM_INTERVAL: Duration = Duration::from_millis(500);

/// Everything a spawned operation needs: something to call, somewhere to
/// report, the poller poke that refreshes the table when it is done — and what
/// this process has already deleted.
#[derive(Debug, Clone)]
pub struct OpContext {
    pub client: Arc<SynoClient>,
    pub tx: Sender,
    pub refresh: RefreshHandle,
    /// Shared across every batch of the run; see [`DeletedPaths`].
    pub deleted: DeletedPaths,
}

impl OpContext {
    pub fn new(client: Arc<SynoClient>, tx: Sender, refresh: RefreshHandle) -> Self {
        OpContext {
            client,
            tx,
            refresh,
            deleted: DeletedPaths::default(),
        }
    }
}

/// The paths File Station has reported as **successfully deleted** during this
/// run of the program.
///
/// This is the memory that lets a *retry* tell "the files are gone because I
/// removed them a moment ago" from "the files were never where I am looking".
/// Both answer [`PathInfo::Missing`], and the executor is deliberately strict
/// about the second one — an absent path is how a mis-resolved destination
/// would otherwise get a completed download's task deleted with its payload
/// still on the volume ([`decide_file_phase`]).
///
/// Without it, strictness would be a trap: an item whose files went but whose
/// *post-delete confirmation* could not be read fails and keeps its task (see
/// [`confirm_deleted`]), and the obvious retry would then hit that refusal for
/// ever. Recording the fact at the one place that knows it — the delete
/// returning success — fixes that at its cause instead of by ignoring the
/// negative signal.
///
/// Cloned with the [`OpContext`], so every batch of a run shares one set.
#[derive(Debug, Clone, Default)]
pub struct DeletedPaths(Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl DeletedPaths {
    /// Remember that `path` was deleted, successfully, by this process.
    pub fn record(&self, path: &str) {
        // A poisoned mutex would mean a panic while holding a `HashSet`, which
        // cannot leave it inconsistent; the memory is an optimization for the
        // retry case, never a safety guard, so recovering beats propagating.
        let mut set = self.0.lock().unwrap_or_else(|err| err.into_inner());
        set.insert(path.to_string());
    }

    /// Whether this process already deleted `path`.
    pub fn contains(&self, path: &str) -> bool {
        let set = self.0.lock().unwrap_or_else(|err| err.into_inner());
        set.contains(path)
    }
}

/// Run a confirmed [`DeletePlan`] in the background.
///
/// The plan is an owned snapshot taken when the dialog opened, so nothing this
/// task reads can have moved since the user read it. Progress is reported per
/// item; the table is refreshed once at the end rather than per item, so a
/// twenty-torrent delete is not twenty full task-list round trips.
pub fn spawn_delete(ops: OpContext, plan: DeletePlan, options: DeleteOptions) -> JoinHandle<()> {
    tokio::spawn(async move { run_delete(ops, plan, options).await })
}

/// What happened to one item of a batch — a delete, a pause or a resume.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemOutcome {
    /// Everything the operation asked for was done. Carries the past tense of
    /// the operation ([`OpKind::past_tense`]) so one enum serves all three,
    /// optionally with a qualifier — a delete whose files turned out to be gone
    /// already still *removed the task*, so it is a success that reads
    /// differently, not a skip.
    Done(String),
    /// Deliberately not acted on — a refused path, or a dry run. **Nothing was
    /// changed on the NAS.**
    Skipped(String),
    /// A phase failed; every later phase for this item was cancelled.
    Failed(String),
}

impl ItemOutcome {
    /// A plain success in the operation's past tense.
    fn done(op: OpKind) -> Self {
        ItemOutcome::Done(op.past_tense().to_string())
    }

    /// How the outcome reads in the footer.
    fn detail(&self) -> String {
        match self {
            ItemOutcome::Done(what) => what.clone(),
            ItemOutcome::Skipped(why) => format!("skipped — {why}"),
            ItemOutcome::Failed(why) => format!("FAILED — {why}"),
        }
    }
}

/// The running counts of a batch, so `OpDone` cannot disagree with the
/// per-item lines that produced it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BatchTally {
    succeeded: usize,
    skipped: usize,
    failed: usize,
}

impl BatchTally {
    /// Count one item.
    fn record(&mut self, outcome: &ItemOutcome) {
        match outcome {
            ItemOutcome::Done(_) => self.succeeded += 1,
            ItemOutcome::Skipped(_) => self.skipped += 1,
            ItemOutcome::Failed(_) => self.failed += 1,
        }
    }

    /// The summary event for a finished batch.
    fn done_event(self, op: OpKind) -> AppEvent {
        AppEvent::OpDone {
            op,
            succeeded: self.succeeded,
            skipped: self.skipped,
            failed: self.failed,
        }
    }
}

async fn run_delete(ops: OpContext, plan: DeletePlan, options: DeleteOptions) {
    let total = plan.len();
    let mut tally = BatchTally::default();

    tracing::info!(
        items = total,
        delete_files = options.delete_files,
        dry_run = options.dry_run,
        "starting a delete batch"
    );

    for (index, item) in plan.items.iter().enumerate() {
        let outcome = delete_one(&ops.client, item, options, &ops.deleted).await;
        tally.record(&outcome);

        let progress = AppEvent::OpProgress {
            op: OpKind::Delete,
            done: index + 1,
            total,
            detail: format!("{}: {}", item.title, outcome.detail()),
        };
        if ops.tx.send(progress).await.is_err() {
            // The UI has gone. Finishing the batch would be work nobody can see
            // the result of, and the process is on its way out anyway.
            tracing::debug!("the event channel closed mid-delete; stopping");
            return;
        }
    }

    tracing::info!(
        succeeded = tally.succeeded,
        skipped = tally.skipped,
        failed = tally.failed,
        "delete batch finished"
    );
    let _ = ops.tx.send(tally.done_event(OpKind::Delete)).await;

    // The table is now wrong in the most visible way a table can be, so do not
    // make the user stare at deleted rows until the next scheduled tick.
    ops.refresh.request();
}

/// Carry out the phases for one item, stopping at the first failure.
async fn delete_one(
    client: &SynoClient,
    item: &DeleteItem,
    options: DeleteOptions,
    deleted: &DeletedPaths,
) -> ItemOutcome {
    let ops = delete::plan_delete_ops(item, options);
    if ops.is_empty() {
        // A refused item *while files are being deleted*: the dialog showed it
        // as SKIPPED and nothing — including the DSM task — is touched.
        return ItemOutcome::Skipped(
            item.refusal()
                .unwrap_or("there is nothing to do for this task")
                .to_string(),
        );
    }

    let mut files_were_already_gone = false;

    for (index, op) in ops.iter().enumerate() {
        match run_op(client, item, op, options, deleted).await {
            OpOutcome::Done => {}
            OpOutcome::NothingThere => files_were_already_gone = true,
            OpOutcome::Failed(why) => {
                let cancelled = delete::ops_cancelled_by(&ops, index);
                if !cancelled.is_empty() {
                    tracing::warn!(
                        id = %item.id,
                        cancelled = %delete::describe_ops(cancelled),
                        "a delete phase failed; the remaining phases are cancelled"
                    );
                }
                return ItemOutcome::Failed(why);
            }
        }
    }

    if options.dry_run {
        ItemOutcome::Skipped(format!("dry run — would {}", delete::describe_ops(&ops)))
    } else if files_were_already_gone {
        // A success, not a skip: the DSM task *was* removed. Counting it as
        // skipped made "2 succeeded, 3 skipped" the report for a batch that
        // deleted five tasks.
        ItemOutcome::Done("deleted — the files were already gone".to_string())
    } else {
        ItemOutcome::done(OpKind::Delete)
    }
}

/// The result of one phase, as far as the ordering rule cares.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpOutcome {
    Done,
    /// The file phase found nothing at the resolved path. Not a failure: the
    /// later phases still run.
    NothingThere,
    Failed(String),
}

/// What the pre-delete existence check means for the file phase.
///
/// Pure, and separated from the I/O for exactly one reason: the difference
/// between these three answers is the difference between "the space was
/// reclaimed", "somebody already reclaimed it" and "the files are still there
/// and the task that points at them is about to be destroyed". A regression
/// that mapped [`PathInfo::Error`] onto [`OpOutcome::NothingThere`] would
/// silently do the last of those.
///
/// **An absent path is only benign when something explains the absence.** Three
/// things can:
///
/// * *this run already deleted that exact path* (`already_deleted`, from
///   [`DeletedPaths`]) — the strongest explanation there is, and the one that
///   keeps a retry after an unreadable post-delete check from being refused for
///   ever;
/// * the path came from the task's **file list** (`name_source`) *and* the task
///   is in a state where Download Station cleans up after itself — an
///   incomplete, paused or errored download. That is the case the plan's
///   "Missing ⇒ still delete the task" rule was written for;
/// * nothing else. A name guessed from the display **title** is at least as
///   likely to have missed as to have been tidied up, and a **finished** task's
///   payload demonstrably existed, so its absence points at a mis-resolved
///   *destination* rather than at a cleanup — see
///   [`crate::delete::payload_should_exist`]. Both keep the task, so the data
///   stays reachable; both name `--no-delete-files` as the way out.
fn decide_file_phase(
    info: PathInfo,
    path: &str,
    name_source: Option<NameSource>,
    status: &crate::model::TaskStatus,
    already_deleted: bool,
) -> OpOutcome {
    match info {
        PathInfo::Found { .. } => OpOutcome::Done,

        PathInfo::Missing if already_deleted => {
            tracing::info!(path, "already deleted by this run; treating as gone");
            OpOutcome::NothingThere
        }
        PathInfo::Missing if name_source != Some(NameSource::FileList) => {
            OpOutcome::Failed(format!(
                "nothing at {path}, and that path was guessed from the task's title rather than \
             read from its file list — refusing to delete the task, which would leave no \
             pointer to the data if the guess was wrong (use --no-delete-files to remove \
             the task anyway)"
            ))
        }
        PathInfo::Missing if delete::payload_should_exist(status) => OpOutcome::Failed(format!(
            "nothing at {path}, but this task has finished, so its data should be there — \
             the resolved location is more likely wrong than the files already gone; \
             refusing to delete the task, which would leave no pointer to the payload \
             (use --no-delete-files to remove the task anyway)"
        )),
        PathInfo::Missing => OpOutcome::NothingThere,

        // Not absence — "I could not look" must never be read as "there is
        // nothing to delete", which would remove the task and strand the files.
        PathInfo::Error(code) => OpOutcome::Failed(format!(
            "could not check {path}: {}",
            crate::error::Error::dsm(code, file_station::FS_LIST_API)
        )),

        PathInfo::Unknown => OpOutcome::Failed(format!(
            "the NAS answered the existence check for {path} with nothing this client could \
             attribute to that path; refusing to delete anything on a lookup it could not read"
        )),
    }
}

/// Carry out one phase.
async fn run_op(
    client: &SynoClient,
    item: &DeleteItem,
    op: &Op,
    options: DeleteOptions,
    deleted: &DeletedPaths,
) -> OpOutcome {
    match op {
        Op::Pause => {
            if options.dry_run {
                tracing::info!(id = %item.id, "dry run: would pause the task");
                return OpOutcome::Done;
            }
            match pause_and_confirm(client, &item.id).await {
                Ok(()) => OpOutcome::Done,
                Err(err) => OpOutcome::Failed(format!("could not pause it: {err}")),
            }
        }

        Op::DeleteFiles(path) => {
            // Defence in depth. The guard ran when the snapshot was taken; it
            // runs again here because this value has crossed a task boundary
            // since, and the next call is the one with no undo.
            if let Err(err) = delete::validate_path(path) {
                tracing::error!(id = %item.id, %err, "a resolved path failed re-validation");
                return OpOutcome::Failed(err.to_string());
            }

            if options.dry_run {
                tracing::info!(id = %item.id, path, "dry run: would recursively delete this path");
                return OpOutcome::Done;
            }

            let info = match file_station::path_info(client, path).await {
                Ok(info) => info,
                Err(err) => return OpOutcome::Failed(format!("could not check {path}: {err}")),
            };

            match decide_file_phase(
                info,
                path,
                item.name_source,
                &item.status,
                deleted.contains(path),
            ) {
                OpOutcome::Done => {}
                OpOutcome::NothingThere => {
                    tracing::info!(id = %item.id, path, "nothing on disk at the resolved path");
                    return OpOutcome::NothingThere;
                }
                failed => return failed,
            }

            let paths = [path.clone()];
            if let Err(err) = file_station::delete_paths(client, &paths).await {
                return OpOutcome::Failed(format!("could not delete {path}: {err}"));
            }
            // File Station reported the delete finished with no per-path
            // errors. Recorded *before* the confirmation, because it is exactly
            // the confirmation failing that makes this memory worth having.
            deleted.record(path);
            confirm_deleted(client, path).await
        }

        Op::DeleteTask => {
            if options.dry_run {
                tracing::info!(id = %item.id, "dry run: would delete the DSM task");
                return OpOutcome::Done;
            }
            match delete_task(client, &item.id).await {
                Ok(()) => OpOutcome::Done,
                Err(err) => OpOutcome::Failed(format!("could not delete the task: {err}")),
            }
        }
    }
}

/// Look the path up once more after the recursive delete claimed success.
///
/// The only *other* evidence that the delete did anything is `path_err_num` in
/// the `status` payload, and no response from a real NAS has been captured to
/// confirm this client is reading that field under the name DSM actually uses.
/// Because the field is `#[serde(default)]`, a rename would make a delete that
/// removed nothing — a permission failure, say — look finished and clean, and
/// the DSM task would then be removed on top of files that are still there.
/// One extra `getinfo` makes that safety property hold regardless of the field
/// name.
///
/// **A lookup that did not answer at all is a failure**, not a confirmation.
/// There is no evidence in either direction, and of the two ways to be wrong,
/// keeping a task whose files are gone is the recoverable one — the user sees a
/// row and can remove it; deleting a task whose files are *not* gone leaves data
/// on the volume with nothing pointing at it.
///
/// This does not strand the task: the path is recorded in [`DeletedPaths`]
/// before this runs, so a retry reads the resulting absence as "this run
/// deleted it" and finishes the job. That is the cause of the old
/// retry-deadlock, fixed where it lives rather than by reading a failed lookup
/// as a success.
///
/// Which *readable* answers fail the item is [`decide_confirm_phase`]'s
/// question, and it is deliberately not the same list as before the delete.
async fn confirm_deleted(client: &SynoClient, path: &str) -> OpOutcome {
    match file_station::path_info(client, path).await {
        Ok(info) => decide_confirm_phase(info, path),
        Err(err) => {
            tracing::warn!(
                path,
                %err,
                "could not re-check the path after deleting it; keeping the task"
            );
            OpOutcome::Failed(format!(
                "File Station reported the delete of {path} as finished but the re-check \
                 could not be made ({err}) — keeping the task rather than removing it on \
                 an answer that never came; retrying is safe"
            ))
        }
    }
}

/// What the post-delete re-check means. The **asymmetric twin** of
/// [`decide_file_phase`], and the asymmetry is the point:
///
/// * *before* a delete, an answer this client cannot attribute to the path
///   ([`PathInfo::Unknown`]) is a hard failure — it is what an unreadable
///   `getinfo` shape produces, and reading it as absence would delete every
///   task in the batch while reclaiming nothing;
/// * *after* a delete that reported itself finished, the same answer is
///   evidence in the other direction. This code only ever gets here through a
///   [`PathInfo::Found`] on the *same* call for the *same* path, so the
///   response shape demonstrably parses on this NAS: an entry that has now
///   stopped being attributable is a path that has stopped being there. On a
///   DSM build that answers an absent path with `{"files": []}`, demanding a
///   positive `Missing` failed **every** item of **every** run — files gone,
///   task kept, footer reporting FAILED.
///
/// The relaxation stops at [`PathInfo::Unknown`]. [`PathInfo::Error`] is a
/// **readable** answer carrying a real DSM code — "I could not look at this
/// path" — and it is the same answer [`decide_file_phase`] hard-fails on, for
/// the same reason: it says nothing about whether anything is still there. A
/// recursive delete of a directory holding one entry the DSM account may not
/// remove is exactly the case that produces it, and reading it as absence would
/// delete the Download Station task off a directory that survived.
///
/// So: still `Found`, or an error code, is the failure. `Missing` and an
/// unattributable answer are the delete having done what it said it did.
fn decide_confirm_phase(info: PathInfo, path: &str) -> OpOutcome {
    match info {
        PathInfo::Found { .. } => OpOutcome::Failed(format!(
            "File Station reported the delete of {path} as finished but the path is still \
             there — leaving the task in place (use --no-delete-files to remove the task \
             without touching the files)"
        )),
        PathInfo::Missing => OpOutcome::Done,
        PathInfo::Error(code) => OpOutcome::Failed(format!(
            "File Station reported the delete of {path} as finished but the re-check \
             answered with an error rather than an absence ({}) — keeping the task, since \
             that answer does not say the path is gone",
            crate::error::Error::dsm(code, file_station::FS_LIST_API)
        )),
        PathInfo::Unknown => {
            tracing::warn!(
                path,
                "the post-delete check carried no entry for the path; the delete reported \
                 itself finished, so the task is removed"
            );
            OpOutcome::Done
        }
    }
}

/// Remove one DSM task, treating a per-task error code — **and an id the NAS
/// said nothing about** — as a failure.
///
/// By the time this runs the files are already gone, so a delete reported as
/// succeeding when the task in fact survived leaves a row in Download Station
/// pointing at nothing, and the user is told it was removed.
async fn delete_task(client: &SynoClient, id: &str) -> Result<()> {
    let ids = [id.to_string()];
    let results = download_station::delete_tasks(client, &ids).await?;
    download_station::check_task_result(id, &results)
}

/// Whether a pause has to be issued at all, given what DSM says about the task
/// **right now**.
///
/// `None` — the `getinfo` answer carried no entry for the id that was asked
/// about — means a pause **is** needed. That is the fail-*safe* direction, and
/// it is the same reasoning
/// [`check_task_result`](crate::api::download_station::check_task_result) and
/// [`PathInfo::Unknown`] apply: an empty result says nothing about the id in
/// the question, and [`crate::model::TaskList::tasks`] is `#[serde(default)]`,
/// so any payload this client cannot read arrives here as no entry at all.
/// Reading that as "idle" sends a recursive delete into a directory Download
/// Station may be writing into — the exact hazard the unconditional
/// [`Op::Pause`] exists for.
fn pause_needed(current: Option<&Task>) -> bool {
    current.is_none_or(|task| delete::requires_pause(&task.status))
}

/// The entry for the id that was asked about — **never merely the first one**.
///
/// `getinfo` takes an id list and there is nothing in the protocol that
/// promises the answer is about the id requested (or about only that one). A
/// build that ignored the parameter and answered with the whole task list would
/// otherwise let an unrelated task's status decide whether this one is paused.
fn task_with_id<'a>(tasks: &'a [Task], id: &str) -> Option<&'a Task> {
    tasks.iter().find(|task| task.id == id)
}

/// Pause one task and wait until DSM agrees that it is no longer active.
///
/// The plan's "pause → **confirm paused** → delete files" step. Accepting the
/// pause is not the same as having stopped, and deleting a directory a torrent
/// client is still writing into is how a delete half-succeeds and the directory
/// reappears.
///
/// **The decision to pause is made here, from a live read**, not from the
/// snapshot the confirmation dialog was built on: that status is as old as the
/// time the dialog spent open plus this item's place in the batch queue, and a
/// task DSM's bandwidth schedule resumed in that window would otherwise be
/// recursed through while it was writing. An idle task costs one `getinfo` and
/// no pause call — which also avoids DSM's "already paused" per-task error
/// turning a perfectly good delete into a failure.
async fn pause_and_confirm(client: &SynoClient, id: &str) -> Result<()> {
    let ids = [id.to_string()];

    let current = download_station::task_info(client, &ids).await?;
    if !pause_needed(task_with_id(&current, id)) {
        tracing::debug!(id, "the task is already inactive; no pause is needed");
        return Ok(());
    }

    let results = download_station::pause_tasks(client, &ids).await?;
    download_station::check_task_result(id, &results)?;

    let deadline = Instant::now() + PAUSE_CONFIRM_TIMEOUT;
    loop {
        let current = download_station::task_info(client, &ids).await?;
        if !pause_needed(task_with_id(&current, id)) {
            return Ok(());
        }

        if Instant::now() + PAUSE_CONFIRM_INTERVAL >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "task {id} did not report itself paused within {}s",
                    PAUSE_CONFIRM_TIMEOUT.as_secs()
                ),
            )
            .into());
        }
        tokio::time::sleep(PAUSE_CONFIRM_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Pause and resume
// ---------------------------------------------------------------------------

/// The operations this path can actually carry out.
///
/// A two-variant enum rather than [`OpKind`] so that "a delete does not belong
/// here" is a statement the type system makes instead of an unreachable `match`
/// arm — one that, if it were ever reached, answered with an empty result array
/// and would then have made every item of the batch report "DSM reported no
/// result for this task".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOp {
    Pause,
    Resume,
}

impl TaskOp {
    /// How the operation is reported to the user.
    pub fn kind(self) -> OpKind {
        match self {
            TaskOp::Pause => OpKind::Pause,
            TaskOp::Resume => OpKind::Resume,
        }
    }

    /// How the operation is named in a message.
    pub fn label(self) -> &'static str {
        self.kind().label()
    }
}

/// Pause (`p`) or resume (`u`) a set of tasks in the background.
///
/// Same shape as [`spawn_delete`] — an [`OpContext`], per-item
/// [`AppEvent::OpProgress`], one [`AppEvent::OpDone`], then a single refresh —
/// but a single operation rather than an ordered sequence of phases, so there
/// is nothing to cancel and nothing that can half-happen.
///
/// **One round trip for the whole batch.** Download Station takes a
/// comma-separated id list and answers with a result *per task*, which is where
/// the per-item outcomes come from; see [`task_op_outcome`].
///
/// A delete cannot be requested here: it carries an ordering and belongs to
/// [`spawn_delete`], and [`TaskOp`] has no variant for it.
pub fn spawn_task_op(
    ops: OpContext,
    op: TaskOp,
    tasks: Vec<TaskRef>,
    dry_run: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move { run_task_op(ops, op, tasks, dry_run).await })
}

/// Just enough of a task to act on it and to say which one it was.
///
/// The title is carried so the per-item progress line names the torrent rather
/// than `dbid_042`, which means nothing to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRef {
    pub id: String,
    pub title: String,
}

async fn run_task_op(ops: OpContext, op: TaskOp, tasks: Vec<TaskRef>, dry_run: bool) {
    let kind = op.kind();
    let total = tasks.len();
    if total == 0 {
        return;
    }

    let ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
    tracing::info!(
        op = kind.label(),
        tasks = total,
        dry_run,
        "starting a batch"
    );

    let outcomes = if dry_run {
        // `--dry-run` promises the NAS is not touched, and pausing somebody's
        // whole download list is a change however reversible it is. Reported as
        // *skipped*, never as a success — the same rule the delete executor
        // follows.
        tasks
            .iter()
            .map(|_| ItemOutcome::Skipped(format!("dry run — would {} this task", op.label())))
            .collect()
    } else {
        match call_task_op(&ops.client, op, &ids).await {
            Ok(results) => {
                tracing::debug!(op = op.label(), results = ?results, "per-task results");
                tasks
                    .iter()
                    .map(|task| task_op_outcome(kind, &task.id, &results))
                    .collect()
            }
            // The call itself failed, so nothing moved: every item of the batch
            // is a failure, with the one reason repeated.
            Err(err) => {
                tracing::warn!(op = op.label(), %err, "the batch call failed");
                let why = err.to_string();
                tasks
                    .iter()
                    .map(|_| ItemOutcome::Failed(why.clone()))
                    .collect::<Vec<_>>()
            }
        }
    };

    let mut tally = BatchTally::default();
    for (index, (task, outcome)) in tasks.iter().zip(&outcomes).enumerate() {
        tally.record(outcome);
        let progress = AppEvent::OpProgress {
            op: kind,
            done: index + 1,
            total,
            detail: format!("{}: {}", task.title, outcome.detail()),
        };
        if ops.tx.send(progress).await.is_err() {
            tracing::debug!("the event channel closed mid-batch; stopping");
            return;
        }
    }

    tracing::info!(
        op = op.label(),
        succeeded = tally.succeeded,
        skipped = tally.skipped,
        failed = tally.failed,
        "batch finished"
    );
    let _ = ops.tx.send(tally.done_event(kind)).await;

    // Every status on screen for these rows is now stale, and a pause the user
    // cannot see take effect is a pause they will press again.
    ops.refresh.request();
}

/// Issue the one call the operation needs.
async fn call_task_op(
    client: &SynoClient,
    op: TaskOp,
    ids: &[String],
) -> Result<Vec<download_station::TaskOpResult>> {
    match op {
        TaskOp::Pause => download_station::pause_tasks(client, ids).await,
        TaskOp::Resume => download_station::resume_tasks(client, ids).await,
    }
}

/// What one task's entry in a `pause` / `resume` result array means.
///
/// ⚠️ The failure is **inside** a `success: true` envelope, so the entry has to
/// be read: [`check_task_results`] over the single matching entry is that
/// reading, and is the same helper the delete executor uses. An id the NAS
/// reported nothing for is a failure rather than a success — the task list
/// refresh that follows shows what really happened, and a silent "3 paused" for
/// a task that did not pause is the answer that cannot be corrected.
///
/// [`check_task_results`]: crate::api::download_station::check_task_results
fn task_op_outcome(
    op: OpKind,
    id: &str,
    results: &[download_station::TaskOpResult],
) -> ItemOutcome {
    match download_station::check_task_result(id, results) {
        Ok(()) => ItemOutcome::done(op),
        Err(err) => ItemOutcome::Failed(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! The poller itself needs a NAS and a clock, so what is tested here is the
    //! part that is neither: the refresh handshake and the event shapes. Its
    //! behaviour against a real DSM is verified by running the binary; its
    //! *effect* on the app — [`crate::app::App::apply_tasks`] — is tested
    //! thoroughly in `app.rs`.

    use super::*;
    use crate::model::TaskStatus;

    #[test]
    fn op_kinds_are_named_for_the_footer() {
        assert_eq!(OpKind::Delete.label(), "delete");
        assert_eq!(OpKind::Pause.label(), "pause");
        assert_eq!(OpKind::Resume.label(), "resume");
    }

    /// How long a refresh handshake is given before the test calls it broken.
    ///
    /// Bounded on purpose: `requested()` never returning is precisely the bug
    /// these tests exist to catch, and an unbounded `await` would express that
    /// as a hung suite rather than as a failure.
    const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn a_refresh_request_made_before_the_poller_waits_is_not_lost() {
        // `r` pressed while a poll is in flight must still force the next one,
        // otherwise a manual refresh during a slow tick silently does nothing.
        let refresh = RefreshHandle::new();
        refresh.request();
        tokio::time::timeout(HANDSHAKE_TIMEOUT, refresh.requested())
            .await
            .expect("the stored permit must complete the wait immediately");
    }

    #[tokio::test]
    async fn a_refresh_request_reaches_a_clone_of_the_handle() {
        // The app holds one clone and the poller another; they must be the same
        // notification, not two.
        let refresh = RefreshHandle::new();
        let poller_side = refresh.clone();
        refresh.request();
        tokio::time::timeout(HANDSHAKE_TIMEOUT, poller_side.requested())
            .await
            .expect("a clone must see the same notification");
    }

    #[tokio::test]
    async fn the_channel_carries_events_in_order() {
        let (tx, mut rx) = channel();
        tx.send(AppEvent::Error("first".into())).await.unwrap();
        tx.send(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded: 2,
            skipped: 1,
            failed: 0,
        })
        .await
        .unwrap();

        assert_eq!(rx.recv().await, Some(AppEvent::Error("first".into())));
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::OpDone {
                op: OpKind::Delete,
                succeeded: 2,
                skipped: 1,
                failed: 0
            })
        ));
    }

    #[tokio::test]
    async fn a_closed_channel_is_how_the_poller_learns_to_stop() {
        let (tx, rx) = channel();
        drop(rx);
        assert!(
            tx.send(AppEvent::Error("nobody is listening".into()))
                .await
                .is_err()
        );
    }

    // ---- delete executor ---------------------------------------------------
    //
    // The executor's *rules* are pure and tested in `delete.rs`; the I/O around
    // them needs a NAS and is verified by running the binary. What is left here
    // is the wording that reaches the footer.

    #[test]
    fn each_outcome_reads_differently_in_the_footer() {
        // A skip and a failure must not be mistakable for one another, and
        // neither may be mistakable for a delete that happened.
        assert_eq!(ItemOutcome::done(OpKind::Delete).detail(), "deleted");
        assert_eq!(
            ItemOutcome::Skipped("the files were already gone".into()).detail(),
            "skipped — the files were already gone"
        );
        assert_eq!(
            ItemOutcome::Failed("could not pause it".into()).detail(),
            "FAILED — could not pause it"
        );
    }

    #[test]
    fn the_pause_confirmation_is_bounded_and_polls_more_than_once() {
        assert!(PAUSE_CONFIRM_TIMEOUT > PAUSE_CONFIRM_INTERVAL * 2);
    }

    // ---- pause and resume ---------------------------------------------------
    //
    // The call is one round trip and needs a NAS; what is pure — and what would
    // silently report a failed pause as a success if it were wrong — is reading
    // the per-task result array.

    use crate::api::download_station::TaskOpResult;
    use crate::config::ResolvedConfig;

    /// A client that cannot reach anything.
    ///
    /// Constructing it opens no connection, and — the property the tests below
    /// actually lean on — its [`ApiInfoMap`] is **empty** because `discover()`
    /// was never called. Every request therefore fails in `endpoint()`, before
    /// a socket is opened, so a test asserting `failed: 0` is asserting that no
    /// request was even attempted rather than that the network happened to be
    /// slow. (The host does not resolve either, but that is the second line of
    /// defence, not the mechanism: pre-populating the API map would silently
    /// turn these into real-network tests with a 10-second connect timeout.)
    fn uncalled_client() -> Arc<SynoClient> {
        let config = ResolvedConfig {
            host: "nas.invalid".to_string(),
            port: 5001,
            https: true,
            insecure: false,
            username: "tester".to_string(),
            refresh_secs: 3,
            delete_files: true,
            dry_run: true,
            logout: false,
        };
        Arc::new(SynoClient::new(&config).expect("building a client issues no request"))
    }

    fn result(id: &str, error: i32) -> TaskOpResult {
        TaskOpResult {
            id: id.to_string(),
            error,
        }
    }

    /// Task references with a title distinguishable from the id, so a progress
    /// line that names the wrong one is visible.
    fn task_refs(ids: &[&str]) -> Vec<TaskRef> {
        ids.iter()
            .map(|id| TaskRef {
                id: (*id).to_string(),
                title: format!("Title of {id}"),
            })
            .collect()
    }

    #[test]
    fn an_operation_names_itself_in_the_past_tense_for_a_finished_item() {
        assert_eq!(OpKind::Delete.past_tense(), "deleted");
        assert_eq!(OpKind::Pause.past_tense(), "paused");
        assert_eq!(OpKind::Resume.past_tense(), "resumed");
    }

    #[test]
    fn a_zero_result_is_the_task_actually_having_been_paused() {
        let results = [result("dbid_001", 0), result("dbid_002", 0)];
        assert_eq!(
            task_op_outcome(OpKind::Pause, "dbid_002", &results),
            ItemOutcome::done(OpKind::Pause)
        );
        assert_eq!(
            task_op_outcome(OpKind::Resume, "dbid_001", &results),
            ItemOutcome::done(OpKind::Resume)
        );
    }

    #[test]
    fn a_per_task_error_code_is_a_failure_even_inside_a_successful_envelope() {
        // The trap: DSM answers `{"success": true, "data": [{"error": 544}]}`
        // for a task it did not touch. Reading only the envelope would report
        // this as a pause that happened.
        let results = [result("dbid_001", 0), result("dbid_002", 544)];
        let outcome = task_op_outcome(OpKind::Pause, "dbid_002", &results);
        assert!(
            matches!(&outcome, ItemOutcome::Failed(why) if why.contains("544")),
            "{outcome:?}"
        );
        assert_eq!(
            task_op_outcome(OpKind::Pause, "dbid_001", &results),
            ItemOutcome::done(OpKind::Pause),
            "one failing task must not condemn the rest of the batch"
        );
    }

    #[test]
    fn a_task_the_nas_said_nothing_about_is_a_failure_not_a_success() {
        let outcome = task_op_outcome(OpKind::Resume, "dbid_009", &[result("dbid_001", 0)]);
        assert!(
            matches!(&outcome, ItemOutcome::Failed(why) if why.contains("no result")),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_batch_makes_no_call_and_reports_nothing() {
        // `p` on an empty table must not produce a phantom "0 succeeded" line.
        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_task_op(ops, TaskOp::Pause, Vec::new(), false).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_dry_run_issues_no_call_and_counts_every_item_as_skipped() {
        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_task_op(
            ops,
            TaskOp::Pause,
            task_refs(&["dbid_001", "dbid_002"]),
            true,
        )
        .await;

        // Two progress lines naming the dry run, then a summary with no
        // successes — a dry run must never read as "2 succeeded".
        for expected in 1..=2 {
            match rx.recv().await {
                Some(AppEvent::OpProgress {
                    op, done, detail, ..
                }) => {
                    assert_eq!(op, OpKind::Pause);
                    assert_eq!(done, expected);
                    assert!(detail.contains("dry run"), "{detail}");
                }
                other => panic!("expected progress, got {other:?}"),
            }
        }
        assert_eq!(
            rx.recv().await,
            Some(AppEvent::OpDone {
                op: OpKind::Pause,
                succeeded: 0,
                skipped: 2,
                failed: 0,
            })
        );
    }

    // ---- the delete executor's two no-call paths ---------------------------
    //
    // Both run the *whole* three-phase executor against `uncalled_client()`,
    // whose host does not resolve. That is what makes them meaningful: if any
    // phase issued a request the call would fail and the item would be reported
    // as `failed`, so `failed: 0` is a positive assertion that **nothing was
    // sent** — not merely that nothing broke.

    const FIXTURE: &str = include_str!("../tests/fixtures/task_list.json");

    fn fixture_tasks() -> Vec<Task> {
        crate::api::client::parse_envelope::<crate::model::TaskList>(
            FIXTURE,
            "SYNO.DownloadStation.Task",
        )
        .expect("the fixture must parse")
        .tasks
    }

    #[tokio::test]
    async fn a_dry_run_delete_issues_no_call_at_all_and_claims_no_successes() {
        let plan = DeletePlan::snapshot(fixture_tasks().iter());
        let total = plan.len();
        assert!(total > 1);

        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_delete(ops, plan, DeleteOptions::dry_run()).await;

        let mut done = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                // Every item, including the ones that would really have been
                // deleted, reports what it *would* do — never that it did it.
                AppEvent::OpProgress { op, detail, .. } => {
                    assert_eq!(op, OpKind::Delete);
                    assert!(
                        detail.contains("dry run") || detail.contains("skipped"),
                        "{detail}"
                    );
                }
                AppEvent::OpDone { .. } => done = Some(event),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            done,
            Some(AppEvent::OpDone {
                op: OpKind::Delete,
                succeeded: 0,
                skipped: total,
                failed: 0,
            }),
            "a dry run must report every item as skipped and nothing as done"
        );
    }

    #[tokio::test]
    async fn a_refused_item_reaches_the_nas_not_even_for_its_task_delete() {
        // The fixture's no-common-root task: `delete.rs` refuses to guess its
        // on-disk name, and the executor must therefore leave the DSM task
        // alone too — removing it would orphan exactly the data whose location
        // is in doubt. This is a **real** run, not a dry one.
        let refused: Vec<Task> = fixture_tasks()
            .into_iter()
            .filter(|task| task.id == "dbid_013")
            .collect();
        assert_eq!(refused.len(), 1);
        let plan = DeletePlan::snapshot(refused.iter());
        assert!(plan.items[0].is_refused());

        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_delete(ops, plan, DeleteOptions::default()).await;

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(matches!(
            events.first(),
            Some(AppEvent::OpProgress { detail, .. }) if detail.contains("skipped")
        ));
        assert_eq!(
            events.last(),
            Some(&AppEvent::OpDone {
                op: OpKind::Delete,
                succeeded: 0,
                skipped: 1,
                failed: 0,
            })
        );
    }

    #[tokio::test]
    async fn no_delete_files_removes_a_refused_items_task_instead_of_skipping_it() {
        // The same fixture task as above, with the flag that makes its refusal
        // meaningless: no path is used, so there is nothing to be unsure about,
        // and this is the only route left for a torrent whose on-disk name
        // cannot be resolved. A dry run keeps it off the network while still
        // reporting the ops that *would* run.
        let refused: Vec<Task> = fixture_tasks()
            .into_iter()
            .filter(|task| task.id == "dbid_013")
            .collect();
        let plan = DeletePlan::snapshot(refused.iter());
        assert!(plan.items[0].is_refused());

        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        tokio::time::timeout(
            Duration::from_secs(10),
            run_delete(
                ops,
                plan,
                DeleteOptions {
                    delete_files: false,
                    dry_run: true,
                },
            ),
        )
        .await
        .expect("a dry run issues no request and cannot hang");

        let event = rx.try_recv().expect("one progress event");
        let AppEvent::OpProgress { detail, .. } = event else {
            panic!("expected progress, got {event:?}");
        };
        assert!(
            detail.contains("would delete the DSM task"),
            "a refused item must still be removable with --no-delete-files: {detail}"
        );
        assert!(
            !detail.contains("share no single top-level directory"),
            "the refusal is about a path this run never uses: {detail}"
        );
    }

    #[test]
    fn a_path_deleted_by_this_run_is_remembered() {
        // What keeps the strict readings of an absent path from becoming a task
        // nothing can remove: the retry knows why the path is empty.
        let deleted = DeletedPaths::default();
        assert!(!deleted.contains("/downloads/X"));
        deleted.record("/downloads/X");
        assert!(deleted.contains("/downloads/X"));
        assert!(!deleted.contains("/downloads/Y"));

        // Shared with every clone, so a retry in a *later* batch of the same
        // run sees what the first batch did.
        let clone = deleted.clone();
        clone.record("/downloads/Y");
        assert!(deleted.contains("/downloads/Y"));
    }

    #[tokio::test]
    async fn a_path_that_fails_re_validation_is_never_handed_to_file_station() {
        // The defence-in-depth check at the top of the file phase. `dry_run` is
        // used because validation runs **before** the dry-run early return, so
        // this reaches the guard without needing a NAS behind it — and a `d`
        // that got here with a share root would otherwise recursively delete
        // every torrent in `/downloads`.
        let item = DeleteItem {
            id: "dbid_001".to_string(),
            title: "Corrupted.Plan".to_string(),
            size: 1024,
            status: crate::model::TaskStatus::Finished,
            // A share root: `resolve_delete_path` can never produce one, so the
            // only way it arrives here is a bug between the snapshot and now.
            target: delete::Target::Path("/downloads".to_string()),
            name_source: Some(NameSource::FileList),
        };
        let plan = DeletePlan { items: vec![item] };

        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_delete(ops, plan, DeleteOptions::dry_run()).await;

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(
            matches!(
                events.first(),
                Some(AppEvent::OpProgress { detail, .. })
                    if detail.contains("FAILED") && detail.contains("share root")
            ),
            "{events:?}"
        );
        assert_eq!(
            events.last(),
            Some(&AppEvent::OpDone {
                op: OpKind::Delete,
                succeeded: 0,
                skipped: 0,
                failed: 1,
            }),
            "an invalid path is a failure, never a skip that still deletes the task"
        );
    }

    // ---- the file phase's reading of the existence check --------------------
    //
    // Three answers, three completely different consequences: the space was
    // reclaimed, somebody had already reclaimed it, or the files are still
    // there and the task pointing at them is about to be destroyed.

    /// The pre-delete decision for an incomplete task whose path came from its
    /// file list — the ordinary case, with no memory of an earlier delete.
    fn file_phase(info: PathInfo) -> OpOutcome {
        decide_file_phase(
            info,
            "/downloads/X",
            Some(NameSource::FileList),
            &TaskStatus::Downloading,
            false,
        )
    }

    #[test]
    fn a_path_that_is_there_is_deleted() {
        assert_eq!(
            file_phase(PathInfo::Found { is_dir: true }),
            OpOutcome::Done
        );
    }

    #[test]
    fn an_absent_path_from_an_incomplete_task_is_a_skip_that_still_removes_the_task() {
        // Download Station removes its own partial data, so an incomplete
        // task's path being empty really does mean somebody already tidied up.
        for status in [
            TaskStatus::Downloading,
            TaskStatus::Waiting,
            TaskStatus::Paused,
            TaskStatus::Error,
            TaskStatus::Finishing,
        ] {
            assert_eq!(
                decide_file_phase(
                    PathInfo::Missing,
                    "/downloads/X",
                    Some(NameSource::FileList),
                    &status,
                    false
                ),
                OpOutcome::NothingThere,
                "{status}"
            );
        }
    }

    #[test]
    fn an_absent_path_on_a_finished_task_keeps_the_task() {
        // The orphaning route that goes through the *destination* rather than
        // the name: a task that finished had its payload on disk, so nothing
        // being at the resolved path says the path is wrong, not that the data
        // is gone — and deleting the row would leave that payload unreachable.
        for status in [
            TaskStatus::Finished,
            TaskStatus::Seeding,
            TaskStatus::Extracting,
        ] {
            let outcome = decide_file_phase(
                PathInfo::Missing,
                "/downloads/X",
                Some(NameSource::FileList),
                &status,
                false,
            );
            assert!(
                matches!(&outcome, OpOutcome::Failed(why)
                    if why.contains("finished") && why.contains("--no-delete-files")),
                "{status}: {outcome:?}"
            );
        }
    }

    #[test]
    fn a_path_this_run_already_deleted_reads_as_gone_whatever_the_task_says() {
        // The retry after a post-delete check that could not be made: the files
        // went, this process knows it, and the strictness above must not turn
        // that into a task nothing can ever remove.
        for source in [NameSource::FileList, NameSource::Title] {
            assert_eq!(
                decide_file_phase(
                    PathInfo::Missing,
                    "/downloads/X",
                    Some(source),
                    &TaskStatus::Finished,
                    true
                ),
                OpOutcome::NothingThere,
                "{source:?}"
            );
        }
    }

    #[test]
    fn an_absent_path_guessed_from_the_title_is_a_failure_not_a_skip() {
        // The path came from the display title, which nothing corroborates. An
        // empty answer is at least as likely to mean the guess missed as to
        // mean the data is gone — and deleting the task would destroy the only
        // pointer to a payload still sitting on the volume.
        let outcome = decide_file_phase(
            PathInfo::Missing,
            "/downloads/X",
            Some(NameSource::Title),
            &TaskStatus::Downloading,
            false,
        );
        assert!(
            matches!(&outcome, OpOutcome::Failed(why) if why.contains("guessed")),
            "{outcome:?}"
        );
        // A refused item has no provenance at all and gets the same treatment.
        assert!(matches!(
            decide_file_phase(
                PathInfo::Missing,
                "/downloads/X",
                None,
                &TaskStatus::Downloading,
                false
            ),
            OpOutcome::Failed(_)
        ));
    }

    #[test]
    fn a_lookup_error_is_never_read_as_absence() {
        // "I am not allowed to look" becoming "there is nothing to delete" is
        // how the task goes and the files stay.
        let outcome = file_phase(PathInfo::Error(403));
        assert!(
            matches!(&outcome, OpOutcome::Failed(why) if why.contains("403")),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_unreadable_getinfo_response_deletes_nothing() {
        // The whole-batch failure mode: a `getinfo` shape this client cannot
        // parse yields no entries, and calling that "already gone" would delete
        // every task in the batch while reclaiming nothing.
        let outcome = file_phase(PathInfo::Unknown);
        assert!(matches!(outcome, OpOutcome::Failed(_)), "{outcome:?}");
    }

    // ---- the post-delete re-check -------------------------------------------
    //
    // The asymmetry with `decide_file_phase` above is deliberate and is what
    // these pin: the *same* unreadable answer is a hard failure before the
    // delete and an acceptable one after it.

    #[test]
    fn a_path_still_there_after_the_delete_fails_the_item() {
        let outcome = decide_confirm_phase(PathInfo::Found { is_dir: true }, "/downloads/X");
        assert!(
            matches!(&outcome, OpOutcome::Failed(why) if why.contains("still there")),
            "{outcome:?}"
        );
        // The task survives, so the user needs the one flag that can still
        // remove it.
        assert!(matches!(
            &outcome,
            OpOutcome::Failed(why) if why.contains("--no-delete-files")
        ));
    }

    #[test]
    fn a_path_confirmed_gone_completes_the_item() {
        assert_eq!(
            decide_confirm_phase(PathInfo::Missing, "/downloads/X"),
            OpOutcome::Done
        );
    }

    #[test]
    fn an_unattributable_recheck_does_not_strand_the_task() {
        // The whole-run failure this replaced: on a DSM build that answers an
        // absent path with `{"files": []}` every item deleted its files, got
        // `Unknown` back, failed, and kept the task — a half-completed delete
        // on every single run, reported as FAILED.
        assert_eq!(
            decide_confirm_phase(PathInfo::Unknown, "/downloads/X"),
            OpOutcome::Done,
            "the delete reported itself finished and the path is not there"
        );
    }

    #[test]
    fn a_recheck_that_answers_with_an_error_keeps_the_task() {
        // An error code is a *readable* answer, and what it says is "I could not
        // look" — which is not "the path is gone". The case that produces it is
        // a recursive delete of a directory holding one entry the account may
        // not remove: File Station reports the task finished, the directory
        // survives, and removing the DSM row would orphan it.
        let outcome = decide_confirm_phase(PathInfo::Error(403), "/downloads/X");
        assert!(
            matches!(&outcome, OpOutcome::Failed(why) if why.contains("403")),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_recheck_is_the_mirror_image_of_the_pre_check_only_for_unknown() {
        // The two directions in one place, because collapsing them into one
        // helper is the tempting simplification that reintroduces both bugs.
        // The relaxation covers `Unknown` and nothing else: that is the one
        // answer the previous phase's own `Found` proves this NAS can produce
        // for a path that has stopped being there.
        assert!(matches!(
            file_phase(PathInfo::Unknown),
            OpOutcome::Failed(_)
        ));
        assert_eq!(
            decide_confirm_phase(PathInfo::Unknown, "/downloads/X"),
            OpOutcome::Done
        );

        for info in [PathInfo::Error(403), PathInfo::Found { is_dir: true }] {
            assert!(
                matches!(file_phase(info), OpOutcome::Failed(_) | OpOutcome::Done),
                "{info:?}"
            );
            assert!(
                matches!(
                    decide_confirm_phase(info, "/downloads/X"),
                    OpOutcome::Failed(_)
                ),
                "{info:?} after a finished delete says nothing about the path being gone"
            );
        }
    }

    // ---- the pause phase's live re-check ------------------------------------

    #[test]
    fn a_task_the_status_read_says_nothing_about_is_paused_anyway() {
        // Fail-safe, not fail-open. `TaskList::tasks` is `#[serde(default)]`,
        // so a `getinfo` payload this client cannot read arrives as no entry at
        // all — and reading that as "idle" walks a recursive delete into a
        // directory Download Station may still be writing into.
        assert!(pause_needed(None));
    }

    #[test]
    fn the_status_read_is_matched_to_the_id_that_was_asked_about() {
        // A build that ignored the `id` parameter would otherwise let some
        // other task's `paused` decide this one's fate.
        let tasks = fixture_tasks();
        let id = "dbid_004";
        assert!(matches!(task_with_id(&tasks, id), Some(task) if task.id == id));
        assert!(task_with_id(&tasks, "dbid_nonexistent").is_none());
        assert!(
            pause_needed(task_with_id(&tasks, "dbid_nonexistent")),
            "an answer that does not cover the id is not an answer"
        );
    }

    #[test]
    fn the_live_status_decides_the_pause_not_the_snapshot() {
        // The bug this exists for: the snapshot said `paused` when the dialog
        // opened, DSM's bandwidth schedule resumed the task while the user was
        // reading, and File Station would then recurse through a directory
        // Download Station is writing into.
        let mut task = fixture_tasks()
            .into_iter()
            .find(|task| task.id == "dbid_004")
            .expect("a paused fixture task");
        assert_eq!(task.status, crate::model::TaskStatus::Paused);
        assert!(!pause_needed(Some(&task)));

        task.status = crate::model::TaskStatus::Downloading;
        assert!(pause_needed(Some(&task)));
    }

    // ---- the pause/resume batch --------------------------------------------

    #[tokio::test]
    async fn a_batch_call_that_fails_outright_condemns_every_item_and_no_more() {
        // `uncalled_client()` has an empty API map, so the call fails in
        // `endpoint()` before a socket is opened: nothing moved, so every item
        // is a failure with the one reason repeated — never a partial success.
        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_task_op(
            ops,
            TaskOp::Resume,
            task_refs(&["dbid_001", "dbid_002"]),
            false,
        )
        .await;

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert_eq!(events.len(), 3, "{events:?}");
        for event in &events[..2] {
            assert!(
                matches!(event, AppEvent::OpProgress { detail, .. } if detail.contains("FAILED")),
                "{event:?}"
            );
        }
        assert_eq!(
            events.last(),
            Some(&AppEvent::OpDone {
                op: OpKind::Resume,
                succeeded: 0,
                skipped: 0,
                failed: 2,
            })
        );
    }

    #[tokio::test]
    async fn a_progress_line_names_the_torrent_rather_than_its_dsm_handle() {
        // `dbid_042` means nothing to anyone reading the footer.
        let (tx, mut rx) = channel();
        let ops = OpContext::new(uncalled_client(), tx, RefreshHandle::new());
        run_task_op(ops, TaskOp::Pause, task_refs(&["dbid_007"]), true).await;

        match rx.try_recv() {
            Ok(AppEvent::OpProgress { detail, .. }) => {
                assert!(detail.starts_with("Title of dbid_007:"), "{detail}")
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn the_two_operations_this_path_accepts_report_themselves_correctly() {
        assert_eq!(TaskOp::Pause.kind(), OpKind::Pause);
        assert_eq!(TaskOp::Resume.kind(), OpKind::Resume);
        assert_eq!(TaskOp::Pause.label(), "pause");
        assert_eq!(TaskOp::Resume.label(), "resume");
    }

    #[test]
    fn a_batch_tally_agrees_with_the_lines_that_produced_it() {
        let mut tally = BatchTally::default();
        tally.record(&ItemOutcome::done(OpKind::Delete));
        tally.record(&ItemOutcome::Done(
            "deleted — the files were already gone".into(),
        ));
        tally.record(&ItemOutcome::Skipped("dry run".into()));
        tally.record(&ItemOutcome::Failed("nope".into()));
        assert_eq!(
            tally.done_event(OpKind::Delete),
            AppEvent::OpDone {
                op: OpKind::Delete,
                // An item whose files were already gone still had its task
                // removed, so it is a success, not a skip.
                succeeded: 2,
                skipped: 1,
                failed: 1,
            }
        );
    }
}
