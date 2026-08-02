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
//!   is still removed. For an incomplete task that is the expected answer
//!   (Download Station cleans up its own partial data) and for a finished one
//!   it means somebody already tidied up by hand;
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
use crate::delete::{self, DeleteItem, DeleteOptions, DeletePlan, Op};
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
/// report, and the poller poke that refreshes the table when it is done.
#[derive(Debug, Clone)]
pub struct OpContext {
    pub client: Arc<SynoClient>,
    pub tx: Sender,
    pub refresh: RefreshHandle,
}

impl OpContext {
    pub fn new(client: Arc<SynoClient>, tx: Sender, refresh: RefreshHandle) -> Self {
        OpContext {
            client,
            tx,
            refresh,
        }
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

/// What happened to one item of a delete batch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ItemOutcome {
    /// Everything the plan asked for was done.
    Deleted,
    /// Deliberately not acted on — a refused path, a directory that was already
    /// gone, or a dry run.
    Skipped(String),
    /// A phase failed; every later phase for this item was cancelled.
    Failed(String),
}

impl ItemOutcome {
    /// How the outcome reads in the footer.
    fn detail(&self) -> String {
        match self {
            ItemOutcome::Deleted => "deleted".to_string(),
            ItemOutcome::Skipped(why) => format!("skipped — {why}"),
            ItemOutcome::Failed(why) => format!("FAILED — {why}"),
        }
    }
}

async fn run_delete(ops: OpContext, plan: DeletePlan, options: DeleteOptions) {
    let total = plan.len();
    let (mut succeeded, mut skipped, mut failed) = (0usize, 0usize, 0usize);

    tracing::info!(
        items = total,
        delete_files = options.delete_files,
        dry_run = options.dry_run,
        "starting a delete batch"
    );

    for (index, item) in plan.items.iter().enumerate() {
        let outcome = delete_one(&ops.client, item, options).await;
        match outcome {
            ItemOutcome::Deleted => succeeded += 1,
            ItemOutcome::Skipped(_) => skipped += 1,
            ItemOutcome::Failed(_) => failed += 1,
        }

        let detail = format!("{}: {}", item.title, outcome.detail());
        let progress = AppEvent::OpProgress {
            op: OpKind::Delete,
            done: index + 1,
            total,
            detail,
        };
        if ops.tx.send(progress).await.is_err() {
            // The UI has gone. Finishing the batch would be work nobody can see
            // the result of, and the process is on its way out anyway.
            tracing::debug!("the event channel closed mid-delete; stopping");
            return;
        }
    }

    tracing::info!(succeeded, skipped, failed, "delete batch finished");
    let _ = ops
        .tx
        .send(AppEvent::OpDone {
            op: OpKind::Delete,
            succeeded,
            skipped,
            failed,
        })
        .await;

    // The table is now wrong in the most visible way a table can be, so do not
    // make the user stare at deleted rows until the next scheduled tick.
    ops.refresh.request();
}

/// Carry out the phases for one item, stopping at the first failure.
async fn delete_one(client: &SynoClient, item: &DeleteItem, options: DeleteOptions) -> ItemOutcome {
    let ops = delete::plan_delete_ops(item, options);
    if ops.is_empty() {
        // A refused item: the dialog showed it as SKIPPED and nothing —
        // including the DSM task — is touched.
        return ItemOutcome::Skipped(
            item.refusal()
                .unwrap_or("there is nothing to do for this task")
                .to_string(),
        );
    }

    let mut files_were_already_gone = false;

    for (index, op) in ops.iter().enumerate() {
        match run_op(client, item, op, options).await {
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
        ItemOutcome::Skipped("the files were already gone; the task was removed".to_string())
    } else {
        ItemOutcome::Deleted
    }
}

/// The result of one phase, as far as the ordering rule cares.
enum OpOutcome {
    Done,
    /// The file phase found nothing at the resolved path. Not a failure: the
    /// later phases still run.
    NothingThere,
    Failed(String),
}

/// Carry out one phase.
async fn run_op(
    client: &SynoClient,
    item: &DeleteItem,
    op: &Op,
    options: DeleteOptions,
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

            match file_station::path_info(client, path).await {
                Ok(PathInfo::Found { .. }) => {
                    let paths = [path.clone()];
                    match file_station::delete_paths(client, &paths).await {
                        Ok(()) => OpOutcome::Done,
                        Err(err) => OpOutcome::Failed(format!("could not delete {path}: {err}")),
                    }
                }
                Ok(PathInfo::Missing) => {
                    tracing::info!(id = %item.id, path, "nothing on disk at the resolved path");
                    OpOutcome::NothingThere
                }
                // Not absence — "I could not look" must never be read as
                // "there is nothing to delete", which would remove the task and
                // strand the files.
                Ok(PathInfo::Error(code)) => OpOutcome::Failed(format!(
                    "could not check {path}: {}",
                    crate::error::Error::dsm(code, file_station::FS_LIST_API)
                )),
                Err(err) => OpOutcome::Failed(format!("could not check {path}: {err}")),
            }
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

/// Remove one DSM task, treating a per-task error code as a failure.
async fn delete_task(client: &SynoClient, id: &str) -> Result<()> {
    let ids = [id.to_string()];
    let results = download_station::delete_tasks(client, &ids).await?;
    download_station::check_task_results(&results)
}

/// Pause one task and wait until DSM agrees that it is no longer active.
///
/// The plan's "pause → **confirm paused** → delete files" step. Accepting the
/// pause is not the same as having stopped, and deleting a directory a torrent
/// client is still writing into is how a delete half-succeeds and the directory
/// reappears.
async fn pause_and_confirm(client: &SynoClient, id: &str) -> Result<()> {
    let ids = [id.to_string()];
    let results = download_station::pause_tasks(client, &ids).await?;
    download_station::check_task_results(&results)?;

    let deadline = Instant::now() + PAUSE_CONFIRM_TIMEOUT;
    loop {
        match download_station::task_info(client, &ids).await?.first() {
            // The task is not there any more. Whatever it was holding, it is
            // not holding it now.
            None => return Ok(()),
            Some(task) if !delete::requires_pause(&task.status) => return Ok(()),
            Some(_) => {}
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

#[cfg(test)]
mod tests {
    //! The poller itself needs a NAS and a clock, so what is tested here is the
    //! part that is neither: the refresh handshake and the event shapes. Its
    //! behaviour against a real DSM is verified by running the binary; its
    //! *effect* on the app — [`crate::app::App::apply_tasks`] — is tested
    //! thoroughly in `app.rs`.

    use super::*;

    #[test]
    fn op_kinds_are_named_for_the_footer() {
        assert_eq!(OpKind::Delete.label(), "delete");
        assert_eq!(OpKind::Pause.label(), "pause");
        assert_eq!(OpKind::Resume.label(), "resume");
    }

    #[tokio::test]
    async fn a_refresh_request_made_before_the_poller_waits_is_not_lost() {
        // `r` pressed while a poll is in flight must still force the next one,
        // otherwise a manual refresh during a slow tick silently does nothing.
        let refresh = RefreshHandle::new();
        refresh.request();
        // Completes immediately on the stored permit; if it did not, the test
        // would hang rather than fail, which is the signal either way.
        refresh.requested().await;
    }

    #[tokio::test]
    async fn a_refresh_request_reaches_a_clone_of_the_handle() {
        // The app holds one clone and the poller another; they must be the same
        // notification, not two.
        let refresh = RefreshHandle::new();
        let poller_side = refresh.clone();
        refresh.request();
        poller_side.requested().await;
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
        assert_eq!(ItemOutcome::Deleted.detail(), "deleted");
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
}
