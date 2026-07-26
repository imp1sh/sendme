//! Transfer manager: a priority queue + two-lane scheduler for incoming
//! offers, decoupling acceptance from the GUI's instantaneous UI state.
//!
//! The manager owns the queue, the active-transfer table and an append-only
//! event log. The GUI never gates acceptance on its own `UiState` again — it
//! only projects from a shared [`TransfersSnapshot`]. The manager is driven by
//! the worker thread (its methods are `async` because they send verdicts to the
//! per-connection offer tasks), but it never spawns tasks or touches egui
//! itself. Instead its methods *return* what the worker should do (start a
//! receive, prompt the user), and the worker executes those effects.
//!
//! # Two lanes
//!
//! - **Auto lane** — the default save folder is configured AND (global
//!   auto-accept is on OR the sender is a contact with per-contact auto-accept).
//!   No prompt. Up to `max_concurrent_receives` run in parallel; the rest wait
//!   in the queue and start as slots free up.
//! - **Interactive lane** — needs a folder picker / accept-decision. Strictly
//!   serial (one at a time). The prompt surfaces only when the offer reaches
//!   the front of the interactive lane, not on arrival, so a backlog never
//!   stacks overlapping dialogs.
//!
//! Both lanes run concurrently with each other.
//!
//! # Priority
//!
//! When `strict_contact_priority` is `true` (the default), offers from contacts
//! in the address book are served before offers from unknown senders,
//! regardless of arrival order. Within a priority class, FIFO.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use tokio::sync::{mpsc, oneshot};

use crate::balloon::FileKind;
use crate::config::Config;
use crate::contacts::{AddressBook, IncomingOffer, OfferVerdict, TransferResult};

/// How many log entries the snapshot exposes to the GUI.
const LOG_CAPACITY: usize = 200;

/// A monotonically increasing identifier for an offer/transfer.
pub type OfferId = u64;

/// Which lane an offer runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// No prompt; runs in parallel up to `max_concurrent_receives`.
    Auto,
    /// Needs a user decision; strictly serial (one at a time).
    Interactive,
}

/// Scheduling priority within a lane.
///
/// `Contact` sorts before `Stranger` so contacts are served first when
/// `strict_contact_priority` is enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Contact,
    Stranger,
}

/// Display data for an offer, carried to the GUI and the snapshot.
#[derive(Debug, Clone)]
pub struct OfferInfo {
    pub id: OfferId,
    pub from: String,
    pub from_short: String,
    pub contact_name: Option<String>,
    pub name: String,
    pub size: u64,
    pub kind: FileKind,
    pub mime: String,
    pub ticket: String,
}

/// Parameters the worker needs to spawn a receive task for a started offer.
#[derive(Debug, Clone)]
pub struct ReceiveParams {
    pub id: OfferId,
    pub ticket: String,
    pub target_dir: PathBuf,
    /// Total payload size in bytes, used for the disk-space guard.
    pub size: u64,
}

/// What the manager tells the worker to do after submitting an offer.
#[derive(Debug)]
pub enum OfferDispatch {
    /// The offer was replied to (Wait/Block/Halt) and needs no further action.
    Done,
    /// An auto-lane offer starts immediately; the worker should spawn a
    /// receive and report completion back via [`TransferManager::on_complete`].
    StartAuto(ReceiveParams),
    /// An interactive-lane offer is at the front; the worker should prompt the
    /// user and report the decision via [`TransferManager::accept_offer`] /
    /// [`TransferManager::decline_offer`].
    Prompt(OfferInfo),
}

/// What the manager tells the worker to do after a slot frees up.
#[derive(Debug, Default)]
pub struct DrainActions {
    /// Auto-lane offers that grabbed a free slot and should be spawned now.
    pub auto_starts: Vec<ReceiveParams>,
    /// An interactive-lane offer that reached the front and needs a prompt.
    pub interactive_prompt: Option<OfferInfo>,
}

/// Events the manager asks the worker to surface to the GUI.
#[derive(Debug)]
pub enum ManagerEvent {
    /// An interactive offer reached the front of the interactive lane and is
    /// awaiting an accept/decline decision.
    PromptOffer(OfferInfo),
    /// The block mode changed (toggled by the user or at startup).
    BlockModeChanged(bool),
}

/// The outcome of a receive, reported back to the manager so it can send the
/// matching `TransferResult` to the sender and drain the queue.
#[derive(Debug)]
pub enum CompletionOutcome {
    /// The file(s) were saved to disk.
    Saved,
    /// Existing file(s) were kept; nothing was overwritten.
    KeptExisting,
    /// The transfer failed (network error, write error, …).
    Failed(String),
    /// The transfer was cancelled (by the receiver or because the sender
    /// disappeared).
    Cancelled,
}

/// A timestamped entry in the activity log.
#[derive(Debug, Clone)]
pub struct EventEntry {
    pub ts: Instant,
    pub kind: EventKind,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum EventKind {
    Queued,
    Started,
    Completed,
    KeptExisting,
    Failed,
    Cancelled,
    Blocked,
    Halted,
    Declined,
    Prompted,
    Unblock,
}

/// Immutable view of an active transfer for the GUI.
#[derive(Debug, Clone)]
pub struct ActiveTransferView {
    pub id: OfferId,
    pub name: String,
    pub from_short: String,
    pub contact_name: Option<String>,
    pub size: u64,
    pub kind: FileKind,
    pub lane: Lane,
}

/// Immutable view of a queued offer for the GUI.
#[derive(Debug, Clone)]
pub struct QueuedOfferView {
    pub id: OfferId,
    pub name: String,
    pub from_short: String,
    pub contact_name: Option<String>,
    pub size: u64,
    pub lane: Lane,
    pub position: usize,
}

/// A snapshot of the manager's state, shared with the GUI for rendering. The
/// manager writes (under a brief `std::sync::Mutex` hold); the GUI reads each
/// frame. Cloned cheaply enough for a per-frame render.
#[derive(Debug, Clone, Default)]
pub struct TransfersSnapshot {
    pub active: Vec<ActiveTransferView>,
    pub queued: Vec<QueuedOfferView>,
    pub block_mode: bool,
    pub log: Vec<EventEntry>,
}

/// A compact, parse-robust index of the address book for lane/priority
/// decisions. Built from [`AddressBook`] and refreshed whenever the user edits
/// their contacts.
#[derive(Debug, Clone, Default)]
pub struct ContactIndex {
    records: Vec<ContactRecord>,
}

#[derive(Debug, Clone)]
pub struct ContactRecord {
    pub node_id: String,
    pub name: String,
    pub auto_accept: bool,
}

impl ContactIndex {
    /// Build a snapshot from the address book.
    pub fn from_address_book(book: &AddressBook) -> Self {
        let records = book
            .contacts
            .iter()
            .map(|c| ContactRecord {
                node_id: c.node_id.clone(),
                name: c.name.clone(),
                auto_accept: c.auto_accept,
            })
            .collect();
        Self { records }
    }

    /// Find the contact matching `node_id`, parsing both sides to
    /// [`iroh::EndpointId`] so minor formatting differences don't break the
    /// lookup.
    pub fn find(&self, node_id: &str) -> Option<&ContactRecord> {
        let target = node_id.parse::<iroh::EndpointId>().ok()?;
        self.records.iter().find(|r| {
            r.node_id
                .parse::<iroh::EndpointId>()
                .is_ok_and(|id| id == target)
        })
    }

    pub fn is_contact(&self, node_id: &str) -> bool {
        self.find(node_id).is_some()
    }

    pub fn auto_accept_for(&self, node_id: &str) -> bool {
        self.find(node_id).is_some_and(|r| r.auto_accept)
    }

    pub fn name_for(&self, node_id: &str) -> Option<&str> {
        self.find(node_id).map(|r| r.name.as_str())
    }
}

// ── internal records ──────────────────────────────────────────────────────

struct QueuedOffer {
    id: OfferId,
    info: OfferInfo,
    #[allow(dead_code)]
    lane: Lane,
    priority: Priority,
    verdict_tx: mpsc::Sender<OfferVerdict>,
    result_tx: Option<oneshot::Sender<TransferResult>>,
    enqueued_at: Instant,
}

struct ActiveTransfer {
    id: OfferId,
    info: OfferInfo,
    lane: Lane,
    /// `Some` until the result has been sent to the sender.
    result_tx: Option<oneshot::Sender<TransferResult>>,
}

/// What [`TransferManager::cancel`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelKind {
    /// Was queued or pending; already removed and the peer was told.
    Removed,
    /// Is active; the worker should abort the task and call
    /// [`TransferManager::on_complete`] with `Cancelled`.
    Active,
    /// No such transfer.
    NotFound,
}

/// The transfer manager: the brain of the receive side.
///
/// Construct it once (in the worker), share its [`TransfersSnapshot`] with the
/// GUI, and drive it with [`submit_offer`](Self::submit_offer),
/// [`accept_offer`](Self::accept_offer), [`decline_offer`](Self::decline_offer),
/// [`on_complete`](Self::on_complete), [`cancel`](Self::cancel),
/// [`set_block_mode`](Self::set_block_mode) and
/// [`update_contacts`](Self::update_contacts).
pub struct TransferManager {
    config: Arc<Config>,
    contacts: ContactIndex,
    queue_auto: VecDeque<QueuedOffer>,
    queue_interactive: VecDeque<QueuedOffer>,
    active: Vec<ActiveTransfer>,
    /// The interactive offer currently surfaced as a prompt, awaiting the
    /// user's accept/decline. `None` when no prompt is outstanding.
    interactive_pending: Option<QueuedOffer>,
    block_mode: bool,
    event_log: VecDeque<EventEntry>,
    next_id: OfferId,
    snapshot: Arc<Mutex<TransfersSnapshot>>,
    event_tx: mpsc::UnboundedSender<ManagerEvent>,
}

impl TransferManager {
    /// Create a new manager. `snapshot` is shared with the GUI; `event_tx`
    /// delivers [`ManagerEvent`]s the worker bridges into `UiEvent`s.
    pub fn new(
        config: Arc<Config>,
        snapshot: Arc<Mutex<TransfersSnapshot>>,
        event_tx: mpsc::UnboundedSender<ManagerEvent>,
    ) -> Self {
        let block_mode = config.block_mode;
        let mgr = Self {
            config,
            contacts: ContactIndex::default(),
            queue_auto: VecDeque::new(),
            queue_interactive: VecDeque::new(),
            active: Vec::new(),
            interactive_pending: None,
            block_mode,
            event_log: VecDeque::with_capacity(LOG_CAPACITY),
            next_id: 1,
            snapshot,
            event_tx,
        };
        mgr.rebuild_snapshot();
        if block_mode {
            let _ = mgr.event_tx.send(ManagerEvent::BlockModeChanged(true));
        }
        mgr
    }

    /// A handle to the shared snapshot, for the GUI to read each frame.
    pub fn snapshot_handle(&self) -> Arc<Mutex<TransfersSnapshot>> {
        self.snapshot.clone()
    }

    /// Refresh the contact index from the address book. Call this whenever the
    /// user edits their contacts. Does not reorder the existing queue (a
    /// queued offer keeps its place); only future offers are affected.
    pub fn update_contacts(&mut self, idx: ContactIndex) {
        self.contacts = idx;
    }

    /// Toggle block mode. When on, every *new* offer is hard-rejected with
    /// `Block`. Already-active and already-queued transfers are unaffected.
    pub fn set_block_mode(&mut self, on: bool) {
        if self.block_mode == on {
            return;
        }
        self.block_mode = on;
        let _ = self.event_tx.send(ManagerEvent::BlockModeChanged(on));
        self.log(
            if on {
                EventKind::Blocked
            } else {
                EventKind::Unblock
            },
            if on {
                "block mode enabled — new offers will be rejected".to_string()
            } else {
                "block mode disabled — accepting again".to_string()
            },
        );
        self.rebuild_snapshot();
    }

    pub fn block_mode(&self) -> bool {
        self.block_mode
    }

    /// Submit a freshly decoded offer. Sends the appropriate verdict to the
    /// peer and returns what the worker should do next.
    pub async fn submit_offer(&mut self, offer: IncomingOffer) -> OfferDispatch {
        let id = self.next_id;
        self.next_id += 1;

        let is_contact = self.contacts.is_contact(&offer.from);
        let contact_name = self.contacts.name_for(&offer.from).map(|s| s.to_string());
        let info = OfferInfo {
            id,
            from: offer.from.clone(),
            from_short: offer.from_short.clone(),
            contact_name,
            name: offer.name.clone(),
            size: offer.size,
            kind: offer.kind,
            mime: offer.mime.clone(),
            ticket: offer.ticket.clone(),
        };

        // Block mode: hard reject every new offer.
        if self.block_mode {
            let _ = offer.verdict_tx.send(OfferVerdict::Block).await;
            self.log(
                EventKind::Blocked,
                format!("blocked \"{}\" from {}", info.name, info.from_short),
            );
            self.rebuild_snapshot();
            return OfferDispatch::Done;
        }

        // contacts_only: reject offers from senders not in the address book.
        if self.config.contacts_only && !is_contact {
            let _ = offer.verdict_tx.send(OfferVerdict::Block).await;
            self.log(
                EventKind::Blocked,
                format!("blocked unknown sender {} (contacts_only)", info.from_short),
            );
            self.rebuild_snapshot();
            return OfferDispatch::Done;
        }

        // Queue full: halt (transient — the sender may retry shortly).
        let total_queued = self.queue_auto.len() + self.queue_interactive.len();
        if total_queued >= self.config.max_queue_depth {
            let _ = offer.verdict_tx.send(OfferVerdict::Halt).await;
            self.log(
                EventKind::Halted,
                format!(
                    "queue full ({}), halted \"{}\" from {}",
                    total_queued, info.name, info.from_short
                ),
            );
            self.rebuild_snapshot();
            return OfferDispatch::Done;
        }

        let lane = if self.config.default_folder().is_none() {
            Lane::Interactive
        } else {
            let contact_auto = is_contact && self.contacts.auto_accept_for(&offer.from);
            if self.config.auto_accept_offers || contact_auto {
                Lane::Auto
            } else {
                Lane::Interactive
            }
        };
        let priority = self.priority_for(is_contact);
        let qo = QueuedOffer {
            id,
            info: info.clone(),
            lane,
            priority,
            verdict_tx: offer.verdict_tx,
            result_tx: Some(offer.result_tx),
            enqueued_at: Instant::now(),
        };

        // Auto lane with a free slot: start immediately (no Wait needed).
        if lane == Lane::Auto && self.active_auto_count() < self.config.max_concurrent_receives {
            return self.start_auto(qo).await;
        }

        // Interactive lane, idle: surface a prompt now (send Wait first so the
        // peer holds the line while the user decides).
        if lane == Lane::Interactive
            && self.interactive_pending.is_none()
            && self.active_interactive_count() == 0
        {
            let _ = qo.verdict_tx.send(OfferVerdict::Wait).await;
            let info = qo.info.clone();
            self.interactive_pending = Some(qo);
            self.log(
                EventKind::Prompted,
                format!("prompting for \"{}\" from {}", info.name, info.from_short),
            );
            self.rebuild_snapshot();
            return OfferDispatch::Prompt(info);
        }

        // Otherwise: queue with a Wait verdict.
        let _ = qo.verdict_tx.send(OfferVerdict::Wait).await;
        let name = qo.info.name.clone();
        let from_short = qo.info.from_short.clone();
        self.enqueue(qo);
        self.log(
            EventKind::Queued,
            format!("queued \"{}\" from {}", name, from_short),
        );
        self.rebuild_snapshot();
        OfferDispatch::Done
    }

    /// The user accepted an interactive prompt. Send `Accept`, move the offer
    /// to active, and return the params the worker needs to spawn the receive.
    /// Returns `None` if the id is stale (already cancelled / superseded).
    pub async fn accept_offer(
        &mut self,
        id: OfferId,
        target_dir: PathBuf,
    ) -> Option<ReceiveParams> {
        let qo = self.interactive_pending.take()?;
        if qo.id != id {
            // Stale prompt for a different id: put it back untouched.
            self.interactive_pending = Some(qo);
            return None;
        }
        let _ = qo.verdict_tx.send(OfferVerdict::Accept).await;
        let ticket = qo.info.ticket.clone();
        let name = qo.info.name.clone();
        let from_short = qo.info.from_short.clone();
        let size = qo.info.size;
        self.active.push(ActiveTransfer {
            id,
            info: qo.info,
            lane: Lane::Interactive,
            result_tx: qo.result_tx,
        });
        self.log(
            EventKind::Started,
            format!("started \"{}\" from {}", name, from_short),
        );
        self.rebuild_snapshot();
        Some(ReceiveParams {
            id,
            ticket,
            target_dir,
            size,
        })
    }

    /// The user declined an interactive prompt. Send `Block`, drop the result
    /// channel, and prompt the next interactive offer (if any).
    pub async fn decline_offer(&mut self, id: OfferId) -> Option<OfferInfo> {
        let qo = self.interactive_pending.take()?;
        if qo.id != id {
            self.interactive_pending = Some(qo);
            return None;
        }
        let _ = qo.verdict_tx.send(OfferVerdict::Block).await;
        // No result to send on decline; drop the channel.
        self.log(
            EventKind::Declined,
            format!("declined \"{}\" from {}", qo.info.name, qo.info.from_short),
        );
        self.rebuild_snapshot();
        // The interactive lane is now free: drain the next prompt.
        self.drain_interactive().await
    }

    /// Report that a receive finished. Removes it from active, sends the
    /// `TransferResult` to the sender, logs, and drains the queue (starting
    /// the next auto offers / prompting the next interactive offer).
    pub async fn on_complete(&mut self, id: OfferId, outcome: CompletionOutcome) -> DrainActions {
        if let Some(pos) = self.active.iter().position(|t| t.id == id) {
            let t = self.active.remove(pos);
            if let Some(tx) = t.result_tx {
                match outcome {
                    CompletionOutcome::Saved => {
                        let _ = tx.send(TransferResult::Saved);
                    }
                    CompletionOutcome::KeptExisting => {
                        let _ = tx.send(TransferResult::KeptExisting);
                    }
                    // Failed / Cancelled: drop the channel so the accept loop
                    // sends RESULT_ERROR (or the sender already gave up).
                    CompletionOutcome::Failed(_) | CompletionOutcome::Cancelled => {}
                }
            }
            match &outcome {
                CompletionOutcome::Saved => self.log(
                    EventKind::Completed,
                    format!("completed \"{}\" from {}", t.info.name, t.info.from_short),
                ),
                CompletionOutcome::KeptExisting => self.log(
                    EventKind::KeptExisting,
                    format!(
                        "kept existing for \"{}\" from {}",
                        t.info.name, t.info.from_short
                    ),
                ),
                CompletionOutcome::Failed(msg) => self.log(
                    EventKind::Failed,
                    format!("failed \"{}\": {msg}", t.info.name),
                ),
                CompletionOutcome::Cancelled => self.log(
                    EventKind::Cancelled,
                    format!("cancelled \"{}\" from {}", t.info.name, t.info.from_short),
                ),
            }
        }
        self.rebuild_snapshot();
        self.drain().await
    }

    /// Cancel a transfer. If it is active, the worker aborts the task and then
    /// calls [`on_complete`](Self::on_complete) with `Cancelled`. If it is
    /// queued, it is removed and the peer is sent `Block`. If it is the pending
    /// interactive prompt, it is treated as a decline.
    pub async fn cancel(&mut self, id: OfferId) -> CancelKind {
        // Pending interactive prompt?
        if let Some(qo) = self.interactive_pending.take() {
            if qo.id == id {
                let _ = qo.verdict_tx.send(OfferVerdict::Block).await;
                self.log(
                    EventKind::Cancelled,
                    format!("cancelled pending \"{}\"", qo.info.name),
                );
                self.rebuild_snapshot();
                // drain next interactive
                let _ = self.drain_interactive().await;
                return CancelKind::Removed;
            }
            self.interactive_pending = Some(qo);
        }
        // Queued?
        if self.remove_queued(id) {
            self.log(
                EventKind::Cancelled,
                format!("cancelled queued offer #{id}"),
            );
            self.rebuild_snapshot();
            return CancelKind::Removed;
        }
        // Active: the worker will abort the task and call on_complete.
        if self.active.iter().any(|t| t.id == id) {
            return CancelKind::Active;
        }
        CancelKind::NotFound
    }

    // ── helpers ───────────────────────────────────────────────────────────

    fn priority_for(&self, is_contact: bool) -> Priority {
        if self.config.strict_contact_priority && is_contact {
            Priority::Contact
        } else {
            Priority::Stranger
        }
    }

    fn active_auto_count(&self) -> usize {
        self.active.iter().filter(|t| t.lane == Lane::Auto).count()
    }

    fn active_interactive_count(&self) -> usize {
        self.active
            .iter()
            .filter(|t| t.lane == Lane::Interactive)
            .count()
    }

    fn target_dir(&self) -> PathBuf {
        self.config
            .default_folder()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Start an auto-lane offer immediately: send `Accept`, move to active,
    /// return the dispatch.
    async fn start_auto(&mut self, qo: QueuedOffer) -> OfferDispatch {
        let _ = qo.verdict_tx.send(OfferVerdict::Accept).await;
        let id = qo.id;
        let ticket = qo.info.ticket.clone();
        let name = qo.info.name.clone();
        let from_short = qo.info.from_short.clone();
        let size = qo.info.size;
        let target_dir = self.target_dir();
        self.active.push(ActiveTransfer {
            id,
            info: qo.info,
            lane: Lane::Auto,
            result_tx: qo.result_tx,
        });
        self.log(
            EventKind::Started,
            format!("started \"{}\" from {}", name, from_short),
        );
        self.rebuild_snapshot();
        OfferDispatch::StartAuto(ReceiveParams {
            id,
            ticket,
            target_dir,
            size,
        })
    }

    fn enqueue(&mut self, qo: QueuedOffer) {
        if qo.lane == Lane::Auto {
            self.queue_auto.push_back(qo);
        } else {
            self.queue_interactive.push_back(qo);
        }
    }

    /// Remove a queued offer by id (either lane). Returns whether one was
    /// removed; sends `Block` to the peer on success.
    fn remove_queued(&mut self, id: OfferId) -> bool {
        if let Some(pos) = self.queue_auto.iter().position(|q| q.id == id) {
            if let Some(qo) = self.queue_auto.remove(pos) {
                let _ = qo.verdict_tx.try_send(OfferVerdict::Block);
                return true;
            }
        }
        if let Some(pos) = self.queue_interactive.iter().position(|q| q.id == id) {
            if let Some(qo) = self.queue_interactive.remove(pos) {
                let _ = qo.verdict_tx.try_send(OfferVerdict::Block);
                return true;
            }
        }
        false
    }

    /// Fill all free auto slots and prompt the next interactive offer if the
    /// interactive lane is idle.
    async fn drain(&mut self) -> DrainActions {
        let mut actions = DrainActions::default();

        // Auto lane: fill up to max_concurrent_receives.
        while self.active_auto_count() < self.config.max_concurrent_receives {
            let qo = match pop_best(&mut self.queue_auto) {
                Some(qo) => qo,
                None => break,
            };
            let _ = qo.verdict_tx.send(OfferVerdict::Accept).await;
            let id = qo.id;
            let ticket = qo.info.ticket.clone();
            let name = qo.info.name.clone();
            let from_short = qo.info.from_short.clone();
            let size = qo.info.size;
            let target_dir = self.target_dir();
            self.active.push(ActiveTransfer {
                id,
                info: qo.info,
                lane: Lane::Auto,
                result_tx: qo.result_tx,
            });
            self.log(
                EventKind::Started,
                format!("started \"{}\" from {}", name, from_short),
            );
            actions.auto_starts.push(ReceiveParams {
                id,
                ticket,
                target_dir,
                size,
            });
        }

        // Interactive lane: prompt if idle.
        if let Some(info) = self.drain_interactive().await {
            actions.interactive_prompt = Some(info);
        }

        if !actions.auto_starts.is_empty() || actions.interactive_prompt.is_some() {
            self.rebuild_snapshot();
        }
        actions
    }

    /// Prompt the next interactive offer if the interactive lane is idle.
    async fn drain_interactive(&mut self) -> Option<OfferInfo> {
        if self.interactive_pending.is_some() || self.active_interactive_count() > 0 {
            return None;
        }
        let qo = pop_best(&mut self.queue_interactive)?;
        let _ = qo.verdict_tx.send(OfferVerdict::Wait).await;
        let info = qo.info.clone();
        self.interactive_pending = Some(qo);
        self.log(
            EventKind::Prompted,
            format!("prompting for \"{}\" from {}", info.name, info.from_short),
        );
        self.rebuild_snapshot();
        Some(info)
    }

    fn log(&mut self, kind: EventKind, message: String) {
        self.event_log.push_back(EventEntry {
            ts: Instant::now(),
            kind,
            message,
        });
        while self.event_log.len() > LOG_CAPACITY {
            self.event_log.pop_front();
        }
    }

    fn rebuild_snapshot(&self) {
        let snap = TransfersSnapshot {
            active: self
                .active
                .iter()
                .map(|t| ActiveTransferView {
                    id: t.id,
                    name: t.info.name.clone(),
                    from_short: t.info.from_short.clone(),
                    contact_name: t.info.contact_name.clone(),
                    size: t.info.size,
                    kind: t.info.kind,
                    lane: t.lane,
                })
                .collect(),
            queued: self
                .queue_auto
                .iter()
                .chain(self.queue_interactive.iter())
                .enumerate()
                .map(|(i, q)| QueuedOfferView {
                    id: q.id,
                    name: q.info.name.clone(),
                    from_short: q.info.from_short.clone(),
                    contact_name: q.info.contact_name.clone(),
                    size: q.info.size,
                    lane: q.lane,
                    position: i,
                })
                .collect(),
            block_mode: self.block_mode,
            log: self.event_log.iter().cloned().collect(),
        };
        *self.snapshot.lock().unwrap() = snap;
    }
}

/// Pop the highest-priority, earliest-enqueued offer from `queue`.
fn pop_best(queue: &mut VecDeque<QueuedOffer>) -> Option<QueuedOffer> {
    if queue.is_empty() {
        return None;
    }
    let mut best = 0;
    for (i, qo) in queue.iter().enumerate() {
        let cur = (&queue[best].priority, queue[best].enqueued_at);
        let cand = (&qo.priority, qo.enqueued_at);
        if cand < cur {
            best = i;
        }
    }
    queue.remove(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contacts::IncomingOffer;
    use iroh::SecretKey;

    fn cfg_with_folder() -> Arc<Config> {
        Arc::new(Config {
            default_save_folder: Some(PathBuf::from("/tmp/sendme-test")),
            auto_accept_offers: true,
            ..Config::default()
        })
    }

    fn cfg_no_auto() -> Arc<Config> {
        Arc::new(Config {
            default_save_folder: Some(PathBuf::from("/tmp/sendme-test")),
            auto_accept_offers: false,
            ..Config::default()
        })
    }

    fn new_manager(config: Arc<Config>) -> (TransferManager, Arc<Mutex<TransfersSnapshot>>) {
        let snapshot = Arc::new(Mutex::new(TransfersSnapshot::default()));
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mgr = TransferManager::new(config, snapshot.clone(), event_tx);
        (mgr, snapshot)
    }

    /// Build an IncomingOffer with throwaway reply channels for a real node id.
    fn make_offer(from: &SecretKey, name: &str) -> IncomingOffer {
        let (verdict_tx, _vr) = mpsc::channel(8);
        let (result_tx, _rr) = oneshot::channel();
        let id = from.public();
        IncomingOffer {
            from: id.to_string(),
            from_short: id.fmt_short().to_string(),
            name: name.to_string(),
            size: 100,
            kind: FileKind::File,
            mime: "text/plain".to_string(),
            ticket: "tk".to_string(),
            verdict_tx,
            result_tx,
        }
    }

    #[tokio::test]
    async fn block_mode_rejects_every_offer() {
        let cfg = Arc::new(Config {
            block_mode: true,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, snap) = new_manager(cfg);
        let key = SecretKey::generate();
        let offer = make_offer(&key, "pic.png");
        let dispatch = mgr.submit_offer(offer).await;
        assert!(matches!(dispatch, OfferDispatch::Done));
        assert!(snap.lock().unwrap().active.is_empty());
        assert!(snap.lock().unwrap().queued.is_empty());
        assert!(snap.lock().unwrap().block_mode);
    }

    #[tokio::test]
    async fn auto_lane_starts_immediately_when_slot_free() {
        let (mut mgr, snap) = new_manager(cfg_with_folder());
        let key = SecretKey::generate();
        let offer = make_offer(&key, "doc.pdf");
        let dispatch = mgr.submit_offer(offer).await;
        match dispatch {
            OfferDispatch::StartAuto(p) => {
                assert_eq!(p.ticket, "tk");
                assert_eq!(p.target_dir, PathBuf::from("/tmp/sendme-test"));
            }
            other => panic!("expected StartAuto, got {other:?}"),
        }
        assert_eq!(snap.lock().unwrap().active.len(), 1);
        assert!(snap.lock().unwrap().queued.is_empty());
    }

    #[tokio::test]
    async fn auto_lane_queues_when_slots_full() {
        // max_concurrent_receives defaults to 3.
        let (mut mgr, snap) = new_manager(cfg_with_folder());
        let keys: Vec<SecretKey> = (0..4).map(|_| SecretKey::generate()).collect();
        let mut started = 0;
        for k in &keys {
            let offer = make_offer(k, "f.bin");
            if let OfferDispatch::StartAuto(_) = mgr.submit_offer(offer).await {
                started += 1;
            }
        }
        assert_eq!(started, 3, "three should start, the fourth queues");
        assert_eq!(snap.lock().unwrap().active.len(), 3);
        assert_eq!(snap.lock().unwrap().queued.len(), 1);
    }

    #[tokio::test]
    async fn queue_full_halts_excess_offer() {
        let cfg = Arc::new(Config {
            max_concurrent_receives: 1,
            max_queue_depth: 2,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, _snap) = new_manager(cfg);
        let keys: Vec<SecretKey> = (0..4).map(|_| SecretKey::generate()).collect();
        // #0 starts (slot 1), #1 and #2 queue (depth 2), #3 is halted.
        let mut results = vec![];
        for k in &keys {
            results.push(mgr.submit_offer(make_offer(k, "f.bin")).await);
        }
        assert!(matches!(results[0], OfferDispatch::StartAuto(_)));
        assert!(matches!(results[1], OfferDispatch::Done)); // queued
        assert!(matches!(results[2], OfferDispatch::Done)); // queued
        assert!(matches!(results[3], OfferDispatch::Done)); // halted
    }

    #[tokio::test]
    async fn interactive_lane_prompts_when_idle() {
        let (mut mgr, snap) = new_manager(cfg_no_auto());
        let key = SecretKey::generate();
        let offer = make_offer(&key, "note.txt");
        let dispatch = mgr.submit_offer(offer).await;
        match dispatch {
            OfferDispatch::Prompt(info) => {
                assert_eq!(info.name, "note.txt");
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        // The prompted offer is not in `queued` nor `active` — it's pending.
        assert!(snap.lock().unwrap().active.is_empty());
        assert!(snap.lock().unwrap().queued.is_empty());
    }

    #[tokio::test]
    async fn interactive_lane_queues_behind_active() {
        let (mut mgr, snap) = new_manager(cfg_no_auto());
        let k1 = SecretKey::generate();
        let k2 = SecretKey::generate();
        // First prompts.
        let d1 = mgr.submit_offer(make_offer(&k1, "a.txt")).await;
        assert!(matches!(d1, OfferDispatch::Prompt(_)));
        // Second queues (interactive lane busy).
        let d2 = mgr.submit_offer(make_offer(&k2, "b.txt")).await;
        assert!(matches!(d2, OfferDispatch::Done));
        assert_eq!(snap.lock().unwrap().queued.len(), 1);
    }

    #[tokio::test]
    async fn accept_then_complete_drains_next_interactive() {
        let (mut mgr, _snap) = new_manager(cfg_no_auto());
        let k1 = SecretKey::generate();
        let k2 = SecretKey::generate();
        let d1 = mgr.submit_offer(make_offer(&k1, "a.txt")).await;
        let id1 = match d1 {
            OfferDispatch::Prompt(i) => i.id,
            _ => panic!("expected prompt"),
        };
        mgr.submit_offer(make_offer(&k2, "b.txt")).await; // queues

        // Accept #1 -> spawns receive.
        let params = mgr
            .accept_offer(id1, PathBuf::from("/tmp/sendme-test"))
            .await
            .expect("accept returns params");
        assert_eq!(params.ticket, "tk");

        // Complete #1 -> should prompt #2.
        let drain = mgr.on_complete(id1, CompletionOutcome::Saved).await;
        assert!(
            drain.interactive_prompt.is_some(),
            "next interactive prompted"
        );
    }

    #[tokio::test]
    async fn decline_drains_next_interactive() {
        let (mut mgr, _snap) = new_manager(cfg_no_auto());
        let k1 = SecretKey::generate();
        let k2 = SecretKey::generate();
        let d1 = mgr.submit_offer(make_offer(&k1, "a.txt")).await;
        let id1 = match d1 {
            OfferDispatch::Prompt(i) => i.id,
            _ => panic!("expected prompt"),
        };
        mgr.submit_offer(make_offer(&k2, "b.txt")).await;
        let next = mgr.decline_offer(id1).await;
        assert!(next.is_some(), "declining prompts the next");
    }

    #[tokio::test]
    async fn on_complete_drains_queued_auto() {
        let cfg = Arc::new(Config {
            max_concurrent_receives: 1,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, _snap) = new_manager(cfg);
        let k1 = SecretKey::generate();
        let k2 = SecretKey::generate();
        let d1 = mgr.submit_offer(make_offer(&k1, "a.bin")).await;
        let id1 = match d1 {
            OfferDispatch::StartAuto(p) => p.id,
            _ => panic!(),
        };
        mgr.submit_offer(make_offer(&k2, "b.bin")).await; // queues
                                                          // Completing #1 frees the slot -> #2 starts.
        let drain = mgr.on_complete(id1, CompletionOutcome::Saved).await;
        assert_eq!(drain.auto_starts.len(), 1);
    }

    #[tokio::test]
    async fn contact_priority_served_first() {
        let cfg = cfg_no_auto(); // interactive lane, so both queue + prompt in order
        let (mut mgr, _snap) = new_manager(cfg);
        // Register alice as a contact.
        let alice = SecretKey::generate();
        let book = AddressBook {
            contacts: vec![crate::contacts::Contact {
                name: "alice".into(),
                node_id: alice.public().to_string(),
                email: String::new(),
                auto_accept: false,
            }],
        };
        mgr.update_contacts(ContactIndex::from_address_book(&book));

        // A stranger arrives first, then alice. Alice should be prompted first.
        let stranger = SecretKey::generate();
        mgr.submit_offer(make_offer(&stranger, "stranger.txt"))
            .await; // prompts (idle)
                    // Now the interactive lane is busy (pending). Submit alice -> queues.
        mgr.submit_offer(make_offer(&alice, "alice.txt")).await;
        // Decline the stranger prompt; the drain should pick alice (contact
        // priority) even though she arrived later.
        // Find the stranger's id from the pending prompt via the snapshot log
        // is awkward; instead decline by the pending id. The pending is the
        // stranger (it was prompted first).
        let pending_id = mgr.interactive_pending.as_ref().unwrap().id;
        let next = mgr.decline_offer(pending_id).await;
        let next = next.expect("should prompt the next");
        assert_eq!(
            next.name, "alice.txt",
            "contact should leapfrog the stranger"
        );
    }

    #[tokio::test]
    async fn cancel_queued_removes_it() {
        let cfg = Arc::new(Config {
            max_concurrent_receives: 1,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, snap) = new_manager(cfg);
        let k1 = SecretKey::generate();
        let k2 = SecretKey::generate();
        mgr.submit_offer(make_offer(&k1, "a.bin")).await; // starts
        mgr.submit_offer(make_offer(&k2, "b.bin")).await; // queues
        let queued_id = snap.lock().unwrap().queued[0].id;
        let kind = mgr.cancel(queued_id).await;
        assert_eq!(kind, CancelKind::Removed);
        assert!(snap.lock().unwrap().queued.is_empty());
    }

    #[tokio::test]
    async fn contacts_only_blocks_strangers() {
        let cfg = Arc::new(Config {
            contacts_only: true,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, snap) = new_manager(cfg);
        let stranger = SecretKey::generate();
        let dispatch = mgr.submit_offer(make_offer(&stranger, "spam.bin")).await;
        assert!(matches!(dispatch, OfferDispatch::Done));
        assert!(snap.lock().unwrap().active.is_empty());
        assert!(snap.lock().unwrap().queued.is_empty());
    }

    #[tokio::test]
    async fn contacts_only_admits_known_contacts() {
        let cfg = Arc::new(Config {
            contacts_only: true,
            ..(*cfg_with_folder()).clone()
        });
        let (mut mgr, snap) = new_manager(cfg);
        let alice = SecretKey::generate();
        let book = AddressBook {
            contacts: vec![crate::contacts::Contact {
                name: "alice".into(),
                node_id: alice.public().to_string(),
                email: String::new(),
                auto_accept: false,
            }],
        };
        mgr.update_contacts(ContactIndex::from_address_book(&book));
        let dispatch = mgr.submit_offer(make_offer(&alice, "gift.bin")).await;
        // contacts_only is on but alice is a contact -> admitted (auto lane
        // since cfg_with_folder sets auto_accept_offers=true).
        assert!(matches!(dispatch, OfferDispatch::StartAuto(_)));
        assert_eq!(snap.lock().unwrap().active.len(), 1);
    }
}
