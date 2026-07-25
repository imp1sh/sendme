//! sendme balloon: a tiny desktop companion for sendme.
//!
//! Shows a little balloon hovering on the desktop (frameless, transparent
//! window, Wayland/XWayland compatible). The upper half of the balloon sends a file,
//! the lower half receives one. A small round button on the dividing line opens
//! the address book.
//!
//! - Click the upper (blue) half: a file dialog opens, the chosen file is
//!   imported and a ticket is shown, with a button to copy the
//!   `sendme receive <ticket>` command to the clipboard. The balloon waits
//!   until the file was transferred or you press cancel.
//! - Drag and drop a file onto the balloon to send it directly.
//! - Click the lower (green) half: paste a ticket, choose where to save,
//!   and the data is downloaded to that location.
//! - Click the round button in the middle: open the address book. Add contacts
//!   by nickname + node id, then send a file's ticket directly to a contact over
//!   iroh. The contact's balloon prompts them to accept, and on accept the data
//!   is fetched automatically.

use std::{
    path::PathBuf,
    sync::{mpsc as std_mpsc, Arc},
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, Pos2, Rect, RichText, Sense, Shape,
    Stroke, Vec2, ViewportBuilder, ViewportCommand,
};
use indicatif::HumanBytes;
use iroh::EndpointId;
use iroh_blobs::ticket::BlobTicket;
use sendme::balloon::{
    autostart_is_enabled, disable_autostart, enable_autostart, parse_ticket, receive_ticket,
    send_file, FileKind, OverwriteDecision, ReceiveEvent, SendEvent,
};
use sendme::config::{Config, ConflictDefault, NotificationConfig};
use sendme::contacts::{
    create_contact_endpoint, load_or_create_secret, run_accept_loop, send_offer, AddressBook,
    Contact, IncomingOffer, OfferResult, TransferResult,
};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;

/// Fire a desktop notification, honouring the user's notification settings.
///
/// Spawns a detached thread so the DBus / NSUserNotification call cannot
/// block the egui UI thread.  Errors are silently discarded — if no
/// notification daemon is running (e.g. bare i3/sway without mako/dunst),
/// the balloon's own visual change is the fallback.
fn fire_notification(notif: &NotificationConfig, summary: &str, body: String) {
    if !notif.enabled {
        return;
    }
    let urgency = notif.urgency.to_notify_rust();
    // Per the freedesktop spec, critical notifications should not auto-expire.
    // For low/normal we let the notification daemon decide its own timeout
    // (dunst, mako, swaync, GNOME, … all configure expiry per urgency), so
    // we don't send a client-side timeout hint that most Sway daemons ignore.
    let timeout = if urgency == notify_rust::Urgency::Critical {
        notify_rust::Timeout::Never
    } else {
        notify_rust::Timeout::Default
    };
    let summary = summary.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .urgency(urgency)
            .timeout(timeout)
            .show();
    });
}

/// Fire a CRITICAL desktop notification that stays on screen until dismissed.
///
/// Unlike [`fire_notification`], this ignores the user's
/// `notifications.enabled` setting — config and validation errors must always
/// surface, because a silent fallback to defaults would leave the user
/// wondering why their settings "didn't take effect". On most Linux
/// notification daemons (GNOME, mako, dunst, …) critical urgency keeps the
/// notification visible until the user explicitly dismisses it.
fn fire_critical_notification(summary: &str, body: String) {
    let summary = summary.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .urgency(notify_rust::Urgency::Critical)
            .timeout(notify_rust::Timeout::Never)
            .show();
    });
}

/// Notification for an incoming transfer offer that awaits an Accept/Decline
/// decision.
///
/// When `contact_name` is `Some`, the sender is a known contact and the
/// notification names them; otherwise the sender is flagged as unknown so
/// the user can judge the request before accepting.
fn notify_incoming_offer(
    notif: &NotificationConfig,
    contact_name: Option<&str>,
    from_short: &str,
    name: &str,
    size: u64,
    kind: FileKind,
    mime: &str,
) {
    let contact_name = contact_name.map(|s| s.to_string());
    let from_short = from_short.to_string();
    let name = name.to_string();
    let type_label = type_label(kind, mime);
    let who = match &contact_name {
        Some(cn) => format!("your contact \"{cn}\""),
        None => format!("an unknown sender ({from_short}) — not in your address book"),
    };
    let body = format!(
        "{who} wants to send you: {name} ({}, {type_label})",
        HumanBytes(size)
    );
    fire_notification(notif, "sendme-balloon: incoming transfer", body);
}

/// Notification for a transfer that was accepted automatically (global or
/// per-contact auto-accept). Sent so the user is never surprised by files
/// appearing on disk without any visible activity.
fn notify_auto_accepted(
    notif: &NotificationConfig,
    contact_name: Option<&str>,
    from_short: &str,
    name: &str,
    size: u64,
    kind: FileKind,
    mime: &str,
) {
    let contact_name = contact_name.map(|s| s.to_string());
    let from_short = from_short.to_string();
    let name = name.to_string();
    let type_label = type_label(kind, mime);
    let who = match &contact_name {
        Some(cn) => format!("contact \"{cn}\""),
        None => format!("unknown sender ({from_short})"),
    };
    let body = format!(
        "Auto-accepted from {who}: {name} ({}, {type_label}). Saving to the default folder.",
        HumanBytes(size)
    );
    fire_notification(notif, "sendme-balloon: auto-accepted", body);
}

/// Render a compact, human-readable label for an offered transfer's type,
/// e.g. "directory", "PDF (application/pdf)" or just "file".
fn type_label(kind: FileKind, mime: &str) -> String {
    match kind {
        FileKind::Directory => "directory".to_string(),
        FileKind::File => {
            if mime.is_empty() || mime == "application/octet-stream" {
                "file".to_string()
            } else {
                let friendly = friendly_type_name(mime);
                match friendly {
                    Some(f) => format!("{f} ({mime})"),
                    None => mime.to_string(),
                }
            }
        }
    }
}

/// Map a MIME type to a short, friendly name for display. Returns `None` when
/// no friendly alias is known, in which case the raw MIME is shown.
fn friendly_type_name(mime: &str) -> Option<&'static str> {
    match mime {
        "application/pdf" => Some("PDF"),
        "image/png" => Some("PNG image"),
        "image/jpeg" => Some("JPEG image"),
        "image/gif" => Some("GIF image"),
        "image/webp" => Some("WebP image"),
        "image/svg+xml" => Some("SVG image"),
        "text/plain" => Some("plain text"),
        "text/html" => Some("HTML"),
        "application/zip" => Some("ZIP archive"),
        "application/x-tar" => Some("tar archive"),
        "application/gzip" | "application/x-gzip" => Some("gzip archive"),
        "application/x-7z-compressed" => Some("7z archive"),
        "application/x-rar-compressed" => Some("RAR archive"),
        "application/x-bzip2" => Some("bzip2 archive"),
        "video/mp4" => Some("MP4 video"),
        "video/x-matroska" => Some("Matroska video"),
        "audio/mpeg" => Some("MP3 audio"),
        "audio/ogg" => Some("Ogg audio"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some("Word document")
        }
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            Some("Excel spreadsheet")
        }
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            Some("PowerPoint presentation")
        }
        "application/json" => Some("JSON"),
        "application/xml" | "text/xml" => Some("XML"),
        "application/octet-stream" => Some("binary"),
        _ => None,
    }
}

/// Commands from the GUI to the background worker.
enum Command {
    PickAndSend,
    SendPath(PathBuf),
    /// Push a transfer ticket to a contact over the contact endpoint.
    SendOffer {
        node_id: EndpointId,
        ticket: String,
        name: String,
        size: u64,
        kind: FileKind,
        mime: String,
    },
    CancelSend,
    Receive {
        ticket: String,
    },
    /// Begin receiving a ticket that arrived via an accepted offer.
    ReceiveOffer {
        ticket: String,
        /// Used to report the transfer outcome back to the sender via the
        /// offer stream. ``None`` for manual (non-offer) receives.
        result_tx: Option<oneshot::Sender<TransferResult>>,
    },
    CancelReceive,
    /// User decided whether to overwrite existing files during a receive.
    ResolveConflict(OverwriteDecision),
}

/// Events from the background worker to the GUI.
enum UiEvent {
    FilePickCancelled,
    SendStarted {
        name: String,
        path: PathBuf,
    },
    Send(SendEvent),
    TicketInvalid(String),
    FolderPickCancelled,
    /// The save-folder picker was cancelled during an offer-initiated receive.
    OfferFolderCancelled,
    ReceiveStarting,
    Receive(ReceiveEvent),
    /// Our own contact-endpoint node id is known.
    NodeIdReady(String),
    /// A remote peer pushed a transfer ticket to us.
    OfferReceived(IncomingOffer),
    /// The contact accepted our outgoing offer and saved the file(s).
    OfferTransferSaved {
        name: String,
    },
    /// The contact accepted but kept existing file(s); nothing was overwritten.
    OfferKeptExisting {
        name: String,
    },
    /// The contact declined our outgoing offer.
    OfferRejected,
    /// Something went wrong with an outgoing offer (or the contact endpoint).
    OfferError(String),
}

#[derive(Clone)]
enum UiState {
    /// The plain balloon, waiting for a click.
    Idle,
    /// The file open dialog is showing and the balloon is hidden.
    PickingFile,
    /// The chosen file is being imported into the blob store.
    Preparing { name: String },
    /// Ticket ready, waiting for a peer to fetch the data.
    Waiting {
        ticket: String,
        name: String,
        size: u64,
        kind: FileKind,
        mime: String,
        peer: bool,
        sent: u64,
    },
    /// The data was transferred to a peer.
    SendDone { name: String },
    /// The peer kept existing file(s) — the incoming file was NOT saved.
    /// Carries the context needed for a one-click "Rename & Retry".
    SendKept {
        name: String,
        original_path: PathBuf,
        contact_node_id: EndpointId,
        contact_name: String,
    },
    /// Asking the user to paste a ticket.
    EnterTicket { error: Option<String> },
    /// The folder picker dialog is showing and the balloon is hidden.
    PickingFolder,
    /// Download in progress.
    Receiving {
        status: String,
        current: u64,
        total: u64,
    },
    /// Download finished and saved.
    ReceiveDone { target: PathBuf },
    /// A receive would overwrite existing files; asking the user.
    ConfirmOverwrite { targets: Vec<PathBuf> },
    /// The receive was skipped; the existing files were kept.
    ReceiveKept { target: PathBuf },
    /// Something went wrong.
    Error { message: String },
    /// Browsing/managing the address book.
    AddressBook,
    /// Form to add a new contact.
    AddContact { error: Option<String> },
    /// Choosing a contact to send the current ticket to.
    PickContact {
        ticket: String,
        name: String,
        size: u64,
        kind: FileKind,
        mime: String,
        peer: bool,
        sent: u64,
    },
    /// An outgoing offer was sent; waiting for the contact's accept/decline.
    OfferPending {
        contact_name: String,
        ticket: String,
        name: String,
        size: u64,
        kind: FileKind,
        mime: String,
        sent: u64,
    },
    /// A remote peer wants to send us a file; awaiting an accept/decline.
    ///
    /// `contact_name` is `Some(name)` when the sender's node id matches a
    /// contact in the address book, so the UI can flag the request as coming
    /// from a known contact. It is `None` when the sender is unknown, in
    /// which case the UI warns the user that the sender is not in the
    /// address book.
    IncomingOffer {
        from_short: String,
        contact_name: Option<String>,
        name: String,
        size: u64,
        kind: FileKind,
        mime: String,
        ticket: String,
    },
}

const SEND_COLOR: Color32 = Color32::from_rgb(66, 133, 244);
const SEND_COLOR_HOVER: Color32 = Color32::from_rgb(108, 160, 247);
const RECV_COLOR: Color32 = Color32::from_rgb(52, 168, 83);
const RECV_COLOR_HOVER: Color32 = Color32::from_rgb(94, 190, 120);
const BUBBLE_BG: Color32 = Color32::from_rgb(32, 33, 36);
/// Amber used for cautionary highlights (e.g. an unknown sender).
const AMBER: Color32 = Color32::from_rgb(230, 160, 30);
const IDLE_SIZE: Vec2 = Vec2::new(164.0, 150.0);

struct BalloonApp {
    state: UiState,
    ticket_text: String,
    copied_at: Option<Instant>,
    last_size: Vec2,
    cmd_tx: tokio_mpsc::UnboundedSender<Command>,
    evt_rx: std_mpsc::Receiver<UiEvent>,
    /// Our own contact-endpoint node id, for display/sharing.
    node_id: String,
    address_book: AddressBook,
    /// The loaded YAML configuration. Edited by hand in a text editor, never
    /// by the GUI.
    config: Arc<Config>,
    /// oneshot used to report the user's accept/decline for the current
    /// incoming offer. Lives outside [`UiState`] so that state stays `Clone`.
    pending_offer_respond: Option<oneshot::Sender<bool>>,
    /// oneshot used to report the transfer outcome back to the sender.
    /// Taken when the user accepts an incoming offer and passed to the
    /// worker via [`Command::ReceiveOffer`].
    pending_offer_result: Option<oneshot::Sender<TransferResult>>,
    /// Text inputs for the add-contact form.
    add_contact_name: String,
    add_contact_node_id: String,
    add_contact_email: String,
    add_contact_auto_accept: bool,
    /// Whether autostart-at-login is currently enabled (cached at startup).
    autostart_enabled: bool,
    /// True while a contact-offer transfer is in progress. Suppresses the
    /// blobs-protocol ``Completed`` signal so the offer result (which
    /// distinguishes "saved" from "kept existing") is authoritative.
    offer_in_progress: bool,
    /// True while a receive was auto-accepted (global or per-contact). Forces
    /// conflicts to resolve safely (keep existing) instead of prompting, so an
    /// unattended transfer never hangs waiting for a human decision.
    auto_accepted_active: bool,
    /// The filesystem path of the file currently being sent, retained so a
    /// "Rename & Retry" can copy and re-send it if the peer reports a name
    /// collision.
    last_send_path: Option<PathBuf>,
    /// The contact (node id + name) we last sent an offer to, retained so a
    /// retry can re-offer to the same contact without asking the user to pick
    /// again.
    last_offer_contact: Option<(EndpointId, String)>,
    /// When set, the next `TicketReady` auto-fires a `SendOffer` to this
    /// contact instead of going to the `Waiting` state. Used by "Rename &
    /// Retry": the file is re-imported under a new name, and as soon as the
    /// ticket is ready the offer is pushed automatically.
    retry_contact: Option<(EndpointId, String)>,
    /// Monotonically increasing retry counter, so successive renames produce
    /// `photo (1).png`, `photo (2).png`, … Reset to 0 on every fresh send.
    retry_count: u32,
}

impl BalloonApp {
    fn new(cc: &eframe::CreationContext<'_>, config: Arc<Config>) -> Self {
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = std_mpsc::channel();
        spawn_worker(cc.egui_ctx.clone(), cmd_rx, evt_tx, config.clone());
        Self {
            state: UiState::Idle,
            ticket_text: String::new(),
            copied_at: None,
            last_size: Vec2::ZERO,
            cmd_tx,
            evt_rx,
            node_id: String::new(),
            address_book: AddressBook::load().unwrap_or_default(),
            config,
            pending_offer_respond: None,
            pending_offer_result: None,
            add_contact_name: String::new(),
            add_contact_node_id: String::new(),
            add_contact_email: String::new(),
            add_contact_auto_accept: false,
            autostart_enabled: autostart_is_enabled(),
            offer_in_progress: false,
            auto_accepted_active: false,
            last_send_path: None,
            last_offer_contact: None,
            retry_contact: None,
            retry_count: 0,
        }
    }

    fn send_cmd(&self, cmd: Command) {
        self.cmd_tx.send(cmd).ok();
    }

    /// Whether auto-accept can fire for an incoming offer: a default save
    /// folder must be configured, and either the global setting is on or the
    /// sender is a contact with per-contact auto-accept enabled.
    fn should_auto_accept(&self, offer: &IncomingOffer) -> bool {
        if !self.config.auto_accept_possible() {
            return false;
        }
        if let Some(c) = self.address_book.find_by_node_id(&offer.from) {
            return c.auto_accept || self.config.auto_accept_offers;
        }
        self.config.auto_accept_offers
    }

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::FilePickCancelled => {
                if matches!(self.state, UiState::PickingFile) {
                    self.state = UiState::Idle;
                }
            }
            UiEvent::SendStarted { name, path } => {
                if matches!(self.state, UiState::PickingFile | UiState::Preparing { .. }) {
                    // A retry (retry_contact set) keeps its retry_count; a
                    // fresh send resets it and remembers the original path.
                    if self.retry_contact.is_none() {
                        self.retry_count = 0;
                        self.last_send_path = Some(path);
                    }
                    self.state = UiState::Preparing { name };
                }
            }
            UiEvent::Send(e) => match (e, &mut self.state) {
                (
                    SendEvent::TicketReady {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                    },
                    UiState::Preparing { .. } | UiState::PickingFile,
                ) => {
                    // If a retry is pending (Rename & Retry), auto-fire the
                    // offer to the same contact instead of going to Waiting.
                    if let Some((node_id, contact_name)) = self.retry_contact.take() {
                        self.last_offer_contact = Some((node_id, contact_name.clone()));
                        self.send_cmd(Command::SendOffer {
                            node_id,
                            ticket: ticket.clone(),
                            name: name.clone(),
                            size,
                            kind,
                            mime: mime.clone(),
                        });
                        self.offer_in_progress = true;
                        self.state = UiState::OfferPending {
                            contact_name,
                            ticket,
                            name,
                            size,
                            kind,
                            mime,
                            sent: 0,
                        };
                    } else {
                        self.state = UiState::Waiting {
                            ticket,
                            name,
                            size,
                            kind,
                            mime,
                            peer: false,
                            sent: 0,
                        };
                    }
                }
                (SendEvent::PeerConnected, UiState::Waiting { peer, .. }) => {
                    *peer = true;
                }
                (SendEvent::Progress { sent }, UiState::Waiting { sent: s, peer, .. }) => {
                    *peer = true;
                    *s = sent;
                }
                (SendEvent::Completed, UiState::Waiting { name, .. }) => {
                    if !self.offer_in_progress {
                        let name = name.clone();
                        self.state = UiState::SendDone { name };
                    }
                }
                (SendEvent::PeerCancelled, UiState::Waiting { name, .. }) => {
                    // the offer flow reports the outcome separately (OfferError),
                    // so only surface a manual-transfer cancellation here.
                    if !self.offer_in_progress {
                        let name = name.clone();
                        self.state = UiState::Error {
                            message: format!("the receiver cancelled the transfer of \"{name}\""),
                        };
                    }
                }
                (
                    SendEvent::Error(message),
                    UiState::Preparing { .. } | UiState::Waiting { .. },
                ) => {
                    self.state = UiState::Error { message };
                }
                // keep counters warm while the user is still picking a contact
                (SendEvent::PeerConnected, UiState::PickContact { peer, .. }) => {
                    *peer = true;
                }
                (SendEvent::Progress { sent }, UiState::PickContact { sent: s, peer, .. }) => {
                    *peer = true;
                    *s = sent;
                }
                (SendEvent::Completed, UiState::PickContact { name, .. }) => {
                    let name = name.clone();
                    self.state = UiState::SendDone { name };
                }
                (SendEvent::Error(message), UiState::PickContact { .. }) => {
                    self.state = UiState::Error { message };
                }
                (SendEvent::PeerCancelled, UiState::PickContact { name, .. }) => {
                    let name = name.clone();
                    self.state = UiState::Error {
                        message: format!("the receiver cancelled the transfer of \"{name}\""),
                    };
                }
                // while waiting for the contact's decision, a peer connecting
                // means the transfer has started -> show normal progress
                (
                    SendEvent::PeerConnected,
                    UiState::OfferPending {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        sent,
                        ..
                    },
                ) => {
                    let ticket = ticket.clone();
                    let name = name.clone();
                    let size = *size;
                    let kind = *kind;
                    let mime = mime.clone();
                    let sent = *sent;
                    self.state = UiState::Waiting {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        peer: true,
                        sent,
                    };
                }
                (
                    SendEvent::Progress { sent },
                    UiState::OfferPending {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        ..
                    },
                ) => {
                    let ticket = ticket.clone();
                    let name = name.clone();
                    let size = *size;
                    let kind = *kind;
                    let mime = mime.clone();
                    self.state = UiState::Waiting {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        peer: true,
                        sent,
                    };
                }
                (SendEvent::Completed, UiState::OfferPending { name, .. }) => {
                    if !self.offer_in_progress {
                        let name = name.clone();
                        self.state = UiState::SendDone { name };
                    }
                }
                (SendEvent::Error(message), UiState::OfferPending { .. }) => {
                    self.offer_in_progress = false;
                    self.state = UiState::Error { message };
                }
                _ => {}
            },
            UiEvent::TicketInvalid(message) => {
                self.state = UiState::EnterTicket {
                    error: Some(message),
                };
            }
            UiEvent::FolderPickCancelled => {
                if matches!(self.state, UiState::PickingFolder) {
                    self.state = UiState::EnterTicket { error: None };
                }
            }
            UiEvent::OfferFolderCancelled => {
                if matches!(self.state, UiState::PickingFolder) {
                    self.state = UiState::Idle;
                }
            }
            UiEvent::ReceiveStarting => {
                if matches!(self.state, UiState::PickingFolder) {
                    self.state = UiState::Receiving {
                        status: "connecting ...".into(),
                        current: 0,
                        total: 0,
                    };
                }
            }
            UiEvent::Receive(e) => match (e, &mut self.state) {
                (ReceiveEvent::Connecting, UiState::Receiving { status, .. }) => {
                    *status = "connecting ...".into();
                }
                (
                    ReceiveEvent::Starting {
                        total_files,
                        payload_size,
                    },
                    UiState::Receiving { status, .. },
                ) => {
                    *status = format!(
                        "downloading {} file(s), {}",
                        total_files,
                        HumanBytes(payload_size)
                    );
                }
                (
                    ReceiveEvent::Progress { current, total },
                    UiState::Receiving {
                        current: c,
                        total: t,
                        ..
                    },
                ) => {
                    *c = current;
                    *t = total;
                }
                (ReceiveEvent::Exporting, UiState::Receiving { status, .. }) => {
                    *status = "saving files ...".into();
                }
                (ReceiveEvent::Conflict { targets }, UiState::Receiving { .. }) => {
                    // Resolve according to the configured default. An
                    // auto-accepted (unattended) transfer must never block on
                    // a prompt, so "ask" is downgraded to "keep existing" for
                    // safety when auto-accept kicked the transfer off.
                    let auto = self.auto_accepted_active;
                    let decision = match (&self.config.conflict_default, auto) {
                        (ConflictDefault::Overwrite, _) => OverwriteDecision::Overwrite,
                        (ConflictDefault::KeepExisting, _) => OverwriteDecision::KeepExisting,
                        (ConflictDefault::Ask, true) => OverwriteDecision::KeepExisting,
                        (ConflictDefault::Ask, false) => {
                            self.state = UiState::ConfirmOverwrite { targets };
                            return;
                        }
                    };
                    let label = if matches!(decision, OverwriteDecision::Overwrite) {
                        "overwriting …"
                    } else {
                        "keeping existing …"
                    };
                    self.send_cmd(Command::ResolveConflict(decision));
                    self.state = UiState::Receiving {
                        status: label.into(),
                        current: 0,
                        total: 0,
                    };
                }
                (
                    ReceiveEvent::KeptExisting { target },
                    UiState::ConfirmOverwrite { .. } | UiState::Receiving { .. },
                ) => {
                    self.auto_accepted_active = false;
                    self.state = UiState::ReceiveKept { target };
                }
                (ReceiveEvent::Completed { target }, UiState::Receiving { .. }) => {
                    self.auto_accepted_active = false;
                    self.state = UiState::ReceiveDone { target };
                }
                (
                    ReceiveEvent::SenderCancelled,
                    UiState::Receiving { .. } | UiState::PickingFolder,
                ) => {
                    self.auto_accepted_active = false;
                    self.state = UiState::Error {
                        message: "the sender cancelled the transfer".into(),
                    };
                }
                (
                    ReceiveEvent::Error(message),
                    UiState::Receiving { .. } | UiState::PickingFolder,
                ) => {
                    self.auto_accepted_active = false;
                    self.state = UiState::Error { message };
                }
                _ => {}
            },
            UiEvent::NodeIdReady(id) => {
                self.node_id = id;
            }
            UiEvent::OfferReceived(offer) => {
                if matches!(self.state, UiState::Idle) {
                    // Resolve the sender against the address book so the UI
                    // can tell known contacts apart from unknown senders.
                    let contact = self.address_book.find_by_node_id(&offer.from);
                    let contact_name = contact.map(|c| c.name.clone());

                    // Auto-accept: when enabled (globally or for this contact)
                    // AND a default save folder is configured, accept without
                    // prompting. The user is still notified so files never
                    // appear on disk silently.
                    if self.should_auto_accept(&offer) {
                        let from_short = offer.from_short.clone();
                        let name = offer.name.clone();
                        let size = offer.size;
                        let kind = offer.kind;
                        let mime = offer.mime.clone();
                        let ticket = offer.ticket.clone();
                        // accept the offer on the network…
                        let _ = offer.respond.send(true);
                        // …and report the transfer outcome back to the sender.
                        let result_tx = Some(offer.result_tx);
                        notify_auto_accepted(
                            &self.config.notifications,
                            contact_name.as_deref(),
                            &from_short,
                            &name,
                            size,
                            kind,
                            &mime,
                        );
                        self.auto_accepted_active = true;
                        self.state = UiState::Receiving {
                            status: format!("auto-accepted from {from_short}, connecting …"),
                            current: 0,
                            total: 0,
                        };
                        self.send_cmd(Command::ReceiveOffer { ticket, result_tx });
                        return;
                    }

                    notify_incoming_offer(
                        &self.config.notifications,
                        contact_name.as_deref(),
                        &offer.from_short,
                        &offer.name,
                        offer.size,
                        offer.kind,
                        &offer.mime,
                    );
                    self.pending_offer_respond = Some(offer.respond);
                    self.pending_offer_result = Some(offer.result_tx);
                    self.state = UiState::IncomingOffer {
                        from_short: offer.from_short,
                        contact_name,
                        name: offer.name,
                        size: offer.size,
                        kind: offer.kind,
                        mime: offer.mime,
                        ticket: offer.ticket,
                    };
                } else {
                    // busy: decline so the sender does not hang
                    let _ = offer.respond.send(false);
                }
            }
            UiEvent::OfferTransferSaved { name } => {
                self.offer_in_progress = false;
                if matches!(
                    self.state,
                    UiState::OfferPending { .. } | UiState::Waiting { .. }
                ) {
                    self.state = UiState::SendDone { name };
                }
            }
            UiEvent::OfferKeptExisting { name } => {
                self.offer_in_progress = false;
                if matches!(
                    self.state,
                    UiState::OfferPending { .. } | UiState::Waiting { .. }
                ) {
                    let original_path = self.last_send_path.clone().unwrap_or_default();
                    let (contact_node_id, contact_name) = self
                        .last_offer_contact
                        .clone()
                        .unwrap_or_else(|| (iroh::SecretKey::generate().public(), String::new()));
                    self.state = UiState::SendKept {
                        name,
                        original_path,
                        contact_node_id,
                        contact_name,
                    };
                }
            }
            UiEvent::OfferRejected => {
                self.offer_in_progress = false;
                if matches!(self.state, UiState::OfferPending { .. }) {
                    self.send_cmd(Command::CancelSend);
                    self.state = UiState::Error {
                        message: "contact declined the transfer".into(),
                    };
                }
            }
            UiEvent::OfferError(message) => {
                self.offer_in_progress = false;
                if matches!(self.state, UiState::OfferPending { .. }) {
                    self.send_cmd(Command::CancelSend);
                    self.state = UiState::Error { message };
                }
            }
        }
    }

    fn close_operation(&mut self) {
        // declining a pending incoming offer: dropping the oneshot sender
        // makes the accept loop treat it as a rejection.
        if matches!(self.state, UiState::IncomingOffer { .. }) {
            self.pending_offer_respond.take();
            self.pending_offer_result.take();
        }
        self.offer_in_progress = false;
        self.auto_accepted_active = false;
        self.retry_contact = None;
        match &self.state {
            UiState::PickingFile
            | UiState::Preparing { .. }
            | UiState::Waiting { .. }
            | UiState::OfferPending { .. } => {
                self.send_cmd(Command::CancelSend);
            }
            UiState::PickingFolder | UiState::Receiving { .. } => {
                self.send_cmd(Command::CancelReceive);
            }
            UiState::ConfirmOverwrite { .. } => {
                self.send_cmd(Command::ResolveConflict(OverwriteDecision::KeepExisting));
            }
            _ => {}
        }
        self.state = UiState::Idle;
    }

    fn desired_size(&self) -> Vec2 {
        match self.state {
            UiState::Idle => IDLE_SIZE,
            // Roomier than the other panels so the "Start at login" tooltip
            // (which explains the XDG-autostart / bare-WM caveats) has space
            // to render fully without being clipped by the tiny frameless
            // window.
            UiState::AddressBook => Vec2::new(560.0, 460.0),
            UiState::PickContact { .. } => Vec2::new(370.0, 360.0),
            UiState::AddContact { .. } => Vec2::new(370.0, 400.0),
            UiState::IncomingOffer { .. } => Vec2::new(370.0, 290.0),
            _ => Vec2::new(370.0, 250.0),
        }
    }

    /// The idle balloon: top half sends, bottom half receives.
    fn draw_balloon(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let (rect, resp) = ui.allocate_exact_size(IDLE_SIZE, Sense::click_and_drag());
        let center = Pos2::new(rect.center().x, rect.top() + 72.0);
        let r = 60.0;
        let in_circle = |p: Pos2| center.distance(p) <= r;
        let hover = resp.hover_pos();
        let hover_top = hover
            .map(|p| in_circle(p) && p.y < center.y)
            .unwrap_or(false);
        let hover_bottom = hover
            .map(|p| in_circle(p) && p.y >= center.y)
            .unwrap_or(false);
        let files_hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        // Native file-drop events do not include coordinates, so the upper half
        // is the drop target whenever a file is over this compact window.
        let drop_hover = files_hovering;
        let send_col = if hover_top {
            SEND_COLOR_HOVER
        } else {
            SEND_COLOR
        };
        let recv_col = if hover_bottom {
            RECV_COLOR_HOVER
        } else {
            RECV_COLOR
        };
        let painter = ui.painter();

        painter.circle_filled(
            center + Vec2::new(0.0, 3.0),
            r + 2.0,
            Color32::from_black_alpha(90),
        );

        // A small knot keeps the silhouette recognizable without wasting space on a string.
        let knot_tip = Pos2::new(center.x, center.y + r + 9.0);
        let knot = vec![
            Pos2::new(center.x - 7.0, center.y + r - 3.0),
            Pos2::new(center.x + 7.0, center.y + r - 3.0),
            knot_tip,
        ];
        painter.add(Shape::convex_polygon(
            knot,
            recv_col,
            Stroke::new(1.5, Color32::from_gray(40)),
        ));

        // two visually distinct halves
        let top_half = Rect::from_min_max(
            Pos2::new(rect.left(), center.y - r - 3.0),
            Pos2::new(rect.right(), center.y),
        );
        let bottom_half = Rect::from_min_max(
            Pos2::new(rect.left(), center.y),
            Pos2::new(rect.right(), center.y + r + 3.0),
        );
        painter
            .with_clip_rect(top_half)
            .circle_filled(center, r, send_col);
        painter
            .with_clip_rect(bottom_half)
            .circle_filled(center, r, recv_col);
        if drop_hover {
            painter.with_clip_rect(top_half).circle_stroke(
                center,
                r - 4.0,
                Stroke::new(3.0, Color32::WHITE),
            );
        }
        painter.line_segment(
            [
                Pos2::new(center.x - r, center.y),
                Pos2::new(center.x + r, center.y),
            ],
            Stroke::new(1.0, Color32::from_white_alpha(180)),
        );
        painter.circle_stroke(center, r, Stroke::new(2.0, Color32::from_gray(35)));
        painter.circle_filled(
            Pos2::new(center.x - r * 0.38, center.y - r * 0.48),
            5.0,
            Color32::from_white_alpha(65),
        );

        // labels
        painter.text(
            Pos2::new(center.x, center.y - 34.0),
            Align2::CENTER_CENTER,
            if drop_hover { "Drop to send" } else { "Send" },
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(center.x, center.y - 17.0),
            Align2::CENTER_CENTER,
            if drop_hover {
                "release file"
            } else {
                "drop file or click"
            },
            FontId::proportional(10.0),
            Color32::from_white_alpha(210),
        );
        painter.text(
            Pos2::new(center.x, center.y + 27.0),
            Align2::CENTER_CENTER,
            "Receive",
            FontId::proportional(16.0),
            Color32::WHITE,
        );
        painter.text(
            Pos2::new(center.x, center.y + 44.0),
            Align2::CENTER_CENTER,
            "paste a ticket",
            FontId::proportional(10.0),
            Color32::from_white_alpha(210),
        );

        // small close button, top right
        let close_rect = Rect::from_min_size(
            Pos2::new(rect.right() - 24.0, rect.top() + 2.0),
            Vec2::splat(20.0),
        );
        let close_resp = ui.interact(close_rect, ui.id().with("close"), Sense::click());
        let close_col = if close_resp.hovered() {
            Color32::WHITE
        } else {
            Color32::from_gray(140)
        };
        ui.painter().text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "✕",
            FontId::proportional(15.0),
            close_col,
        );
        if close_resp.clicked() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }

        let dropped_path = ctx.input(|i| first_dropped_path(&i.raw.dropped_files));
        if let Some(path) = dropped_path {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.state = UiState::Preparing { name };
            self.retry_count = 0;
            self.last_send_path = Some(path.clone());
            self.send_cmd(Command::SendPath(path));
            return;
        }

        // small round button on the dividing line -> address book
        let btn_rect = Rect::from_center_size(center, Vec2::splat(26.0));
        let btn_resp = ui.interact(btn_rect, ui.id().with("addr"), Sense::click());
        let btn_hover = btn_resp.hovered();
        let btn_col = if btn_hover {
            Color32::from_gray(70)
        } else {
            Color32::from_gray(50)
        };
        painter.circle_filled(center, 12.0, btn_col);
        painter.circle_stroke(center, 12.0, Stroke::new(1.5, Color32::WHITE));
        painter.text(
            center,
            Align2::CENTER_CENTER,
            "📇",
            FontId::proportional(13.0),
            Color32::WHITE,
        );
        if btn_resp.clicked() {
            self.state = UiState::AddressBook;
            return;
        }

        // interactions: drag moves the window, click triggers an action
        if resp.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                if in_circle(p) {
                    if p.y < center.y {
                        self.state = UiState::PickingFile;
                        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                        self.send_cmd(Command::PickAndSend);
                    } else {
                        self.state = UiState::EnterTicket { error: None };
                    }
                }
            }
        }
    }

    /// Title bar of the bubble, acts as drag handle and shows a close button.
    fn title_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str) {
        let mut close = false;
        let resp = ui
            .horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(16.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = ui.button("✕").on_hover_text("close").clicked();
                });
            })
            .response;
        if close {
            self.close_operation();
        }
        let drag = ui.interact(
            resp.rect.shrink2(Vec2::new(40.0, 0.0)),
            ui.id().with("drag"),
            Sense::drag(),
        );
        if drag.drag_started() {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        ui.separator();
    }

    fn draw_state(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let state = self.state.clone();
        match state {
            UiState::Idle => unreachable!(),
            UiState::PickingFile => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("choose a file in the dialog ...");
                });
            }
            UiState::Preparing { name } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("preparing \"{name}\" ..."));
                });
            }
            UiState::Waiting {
                ticket,
                name,
                size,
                kind,
                mime,
                peer,
                sent,
            } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.label(format!(
                    "{name} ({}, {type_label})",
                    HumanBytes(size),
                    type_label = type_label(kind, &mime)
                ));
                ui.add_space(4.0);
                ui.label("Ticket for the other side:");
                egui::ScrollArea::vertical()
                    .max_height(60.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&ticket).monospace().size(10.0)).wrap(),
                        );
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("📋 Copy ticket").clicked() {
                        ctx.copy_text(ticket.clone());
                        self.copied_at = Some(Instant::now());
                    }
                    if self
                        .copied_at
                        .map(|t| t.elapsed() < Duration::from_secs(2))
                        .unwrap_or(false)
                    {
                        ui.colored_label(RECV_COLOR, "✓ copied");
                    }
                    if ui.button("Cancel").clicked() {
                        self.close_operation();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    if peer {
                        ui.label("peer connected, sending ...");
                    } else {
                        ui.label("waiting for the receiver ...");
                    }
                });
                if peer && size > 0 {
                    let sent = sent.min(size);
                    let frac = sent as f32 / size as f32;
                    ui.add(egui::ProgressBar::new(frac).text(format!(
                        "{} / {}",
                        HumanBytes(sent),
                        HumanBytes(size)
                    )));
                }
                ui.add_space(6.0);
                if ui.button("📤 Send to a contact…").clicked() {
                    self.state = UiState::PickContact {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        peer,
                        sent,
                    };
                }
            }
            UiState::SendDone { name } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.colored_label(RECV_COLOR, format!("✓ \"{name}\" was transferred."));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::SendKept {
                name,
                original_path,
                contact_node_id,
                contact_name,
            } => {
                self.title_bar(ui, ctx, "🎈 Send");
                ui.add_space(8.0);
                ui.colored_label(
                    Color32::from_rgb(230, 90, 90),
                    format!(
                        "❌ \"{name}\" was NOT saved.\n\
                         The receiver already has a file with this name."
                    ),
                );
                ui.add_space(4.0);
                ui.label(format!(
                    "Ask \"{contact_name}\" to accept it under a different filename."
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    // Rename & Retry: copy the file under a new name and
                    // re-send + re-offer to the same contact automatically.
                    let can_retry = original_path.is_file();
                    if ui
                        .add_enabled(can_retry, egui::Button::new("🔄 Rename & Retry"))
                        .on_hover_text(if can_retry {
                            "Copies the file under a new name (e.g. \"photo (1).png\") \
                             and sends it to the same contact automatically."
                        } else {
                            "Rename & Retry is only available for single files, \
                             not directories."
                        })
                        .clicked()
                    {
                        self.retry_count += 1;
                        let orig_name = original_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| name.clone());
                        let new_name = renamed_filename(&orig_name, self.retry_count);
                        let temp_dir = std::env::temp_dir()
                            .join(format!("sendme-retry-{}", uuid_like_suffix()));
                        let temp_path = temp_dir.join(&new_name);
                        // Copy synchronously — files are typically small for
                        // this use case. A large file will briefly stall the
                        // GUI, but that's acceptable for a one-click retry.
                        match std::fs::create_dir_all(&temp_dir)
                            .and_then(|_| std::fs::copy(&original_path, &temp_path))
                        {
                            Ok(_) => {
                                self.retry_contact = Some((contact_node_id, contact_name.clone()));
                                self.state = UiState::Preparing {
                                    name: new_name.clone(),
                                };
                                self.send_cmd(Command::SendPath(temp_path));
                            }
                            Err(e) => {
                                self.state = UiState::Error {
                                    message: format!("cannot copy for retry: {e}"),
                                };
                            }
                        }
                    }
                    if ui.button("OK").clicked() {
                        self.state = UiState::Idle;
                    }
                });
            }
            UiState::EnterTicket { error } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.label("Paste the ticket (or the whole \"sendme receive ...\" command):");
                ui.add(
                    egui::TextEdit::multiline(&mut self.ticket_text)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Ctrl+V to paste"),
                );
                if let Some(err) = &error {
                    ui.colored_label(
                        Color32::from_rgb(230, 90, 90),
                        format!("invalid ticket: {err}"),
                    );
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let ok = ui.add_enabled(
                        !self.ticket_text.trim().is_empty(),
                        egui::Button::new("Receive"),
                    );
                    if ok.clicked() {
                        if self.config.default_folder().is_some() {
                            // a default folder is configured: skip the picker
                            // and go straight to the receiving state.
                            self.state = UiState::Receiving {
                                status: "connecting ...".into(),
                                current: 0,
                                total: 0,
                            };
                            self.send_cmd(Command::Receive {
                                ticket: self.ticket_text.clone(),
                            });
                        } else {
                            self.state = UiState::PickingFolder;
                            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                            self.send_cmd(Command::Receive {
                                ticket: self.ticket_text.clone(),
                            });
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.state = UiState::Idle;
                    }
                });
            }
            UiState::PickingFolder => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("choose where to save in the dialog ...");
                });
            }
            UiState::Receiving {
                status,
                current,
                total,
            } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(&status);
                });
                ui.add_space(6.0);
                if total > 0 {
                    let frac = current as f32 / total as f32;
                    ui.add(egui::ProgressBar::new(frac).text(format!(
                        "{} / {}",
                        HumanBytes(current),
                        HumanBytes(total)
                    )));
                }
                ui.add_space(6.0);
                if ui.button("Cancel").clicked() {
                    self.close_operation();
                }
            }
            UiState::ReceiveDone { target } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.add_space(8.0);
                ui.colored_label(RECV_COLOR, format!("✓ saved to {}", target.display()));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::ConfirmOverwrite { targets } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.label("A file with the same name already exists:");
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(80.0)
                    .show(ui, |ui| {
                        for t in &targets {
                            ui.label(
                                RichText::new(t.display().to_string())
                                    .monospace()
                                    .size(11.0),
                            );
                        }
                    });
                ui.add_space(6.0);
                ui.label("Overwrite the existing file(s)?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Overwrite").clicked() {
                        self.send_cmd(Command::ResolveConflict(OverwriteDecision::Overwrite));
                        self.state = UiState::Receiving {
                            status: "overwriting …".into(),
                            current: 0,
                            total: 0,
                        };
                    }
                    if ui.button("Keep existing").clicked() {
                        self.send_cmd(Command::ResolveConflict(OverwriteDecision::KeepExisting));
                        self.state = UiState::Receiving {
                            status: "keeping existing …".into(),
                            current: 0,
                            total: 0,
                        };
                    }
                });
            }
            UiState::ReceiveKept { target } => {
                self.title_bar(ui, ctx, "🎈 Receive");
                ui.add_space(8.0);
                ui.colored_label(
                    Color32::from_rgb(230, 160, 30),
                    "The incoming file was discarded — a file with the same \
                     name already exists. Nothing was overwritten.",
                );
                ui.add_space(4.0);
                ui.label(format!("save folder: {}", target.display()));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::Error { message } => {
                self.title_bar(ui, ctx, "🎈 sendme");
                ui.add_space(8.0);
                ui.colored_label(Color32::from_rgb(230, 90, 90), format!("error: {message}"));
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    self.state = UiState::Idle;
                }
            }
            UiState::AddressBook => {
                self.title_bar(ui, ctx, "📇 Contacts");
                ui.label("Your node id (share with contacts):");
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .max_height(54.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&self.node_id).monospace().size(10.0))
                                .wrap(),
                        );
                    });
                ui.add_space(2.0);
                if ui.button("📋 Copy my node id").clicked() {
                    ctx.copy_text(self.node_id.clone());
                }
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let before = self.autostart_enabled;
                    ui.checkbox(&mut self.autostart_enabled, "Start at login")
                        .on_hover_text(
                            "Uses the XDG autostart standard (~/.config/autostart/). \
                             Works on GNOME, KDE, Cinnamon, MATE, XFCE and LXQt. \
                             Bare tiling WMs (i3, Sway, Hyprland, ...) do not launch \
                             XDG entries themselves — install dex-autostart or add \
                             `exec sendme-balloon` to your WM config manually.",
                        );
                    if self.autostart_enabled != before {
                        if self.autostart_enabled {
                            match enable_autostart() {
                                Ok(_) => {}
                                Err(e) => {
                                    self.autostart_enabled = false;
                                    self.state = UiState::Error {
                                        message: format!("cannot enable autostart: {e}"),
                                    };
                                }
                            }
                        } else {
                            let _ = disable_autostart();
                        }
                    }
                });
                ui.add_space(4.0);
                // Warn when auto-accept is enabled (globally or for any contact)
                // but cannot actually fire because no default save folder is
                // configured. Without that folder the picker cannot be skipped,
                // so auto-accept would hang waiting for a human.
                let any_auto = self.config.auto_accept_offers
                    || self.address_book.contacts.iter().any(|c| c.auto_accept);
                if any_auto && self.config.default_folder().is_none() {
                    ui.colored_label(
                        AMBER,
                        "Auto-accept is enabled but no default_save_folder is set \
                         in config.yaml. Set one, or auto-accept will be ignored.",
                    );
                    ui.add_space(4.0);
                }
                if self.address_book.contacts.is_empty() {
                    ui.label("(no contacts yet)");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .show(ui, |ui| {
                            let mut to_remove: Option<String> = None;
                            let mut dirty = false;
                            for i in 0..self.address_book.contacts.len() {
                                ui.horizontal(|ui| {
                                    let c = &mut self.address_book.contacts[i];
                                    ui.label(RichText::new(&c.name).strong());
                                    ui.label(RichText::new(short_id(&c.node_id)).weak().size(10.0));
                                    if !c.email.is_empty() {
                                        ui.label(RichText::new(&c.email).weak().size(10.0));
                                    }
                                    let before = c.auto_accept;
                                    ui.checkbox(&mut c.auto_accept, "auto").on_hover_text(
                                        "Accept transfers from this contact without prompting. \
                                         Needs a default_save_folder to take effect.",
                                    );
                                    if c.auto_accept != before {
                                        dirty = true;
                                    }
                                    if ui
                                        .small_button("🗑")
                                        .on_hover_text("remove contact")
                                        .clicked()
                                    {
                                        to_remove = Some(c.node_id.clone());
                                    }
                                });
                            }
                            if let Some(id) = to_remove {
                                self.address_book.remove(&id);
                                dirty = true;
                            }
                            if dirty {
                                let _ = self.address_book.save();
                            }
                        });
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("➕ Add contact").clicked() {
                        self.add_contact_name.clear();
                        self.add_contact_node_id.clear();
                        self.add_contact_email.clear();
                        self.add_contact_auto_accept = false;
                        self.state = UiState::AddContact { error: None };
                    }
                    if ui.button("Back").clicked() {
                        self.state = UiState::Idle;
                    }
                });
            }
            UiState::AddContact { error } => {
                self.title_bar(ui, ctx, "➕ Add contact");
                ui.label("Name:");
                ui.text_edit_singleline(&mut self.add_contact_name);
                ui.add_space(4.0);
                ui.label("Node id:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_contact_node_id)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("paste the 256-bit node id"),
                );
                ui.add_space(4.0);
                ui.label("Email (optional, for your reference):");
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_contact_email)
                        .hint_text("alice@example.com"),
                );
                ui.add_space(4.0);
                ui.checkbox(
                    &mut self.add_contact_auto_accept,
                    "Auto-accept from this contact",
                )
                .on_hover_text(
                    "Accept transfers from this contact without prompting. \
                         Needs a default_save_folder in config.yaml to take effect.",
                );
                if self.add_contact_auto_accept && self.config.default_folder().is_none() {
                    ui.colored_label(
                        AMBER,
                        "No default_save_folder is set. Auto-accept will be ignored \
                         until you set one in config.yaml.",
                    );
                }
                if let Some(err) = &error {
                    ui.colored_label(Color32::from_rgb(230, 90, 90), err);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let save = ui.add_enabled(
                        !self.add_contact_name.trim().is_empty()
                            && !self.add_contact_node_id.trim().is_empty(),
                        egui::Button::new("Save"),
                    );
                    if save.clicked() {
                        match self.add_contact_node_id.trim().parse::<EndpointId>() {
                            Ok(_) => {
                                let contact = Contact {
                                    name: self.add_contact_name.trim().to_string(),
                                    node_id: self.add_contact_node_id.trim().to_string(),
                                    email: self.add_contact_email.trim().to_string(),
                                    auto_accept: self.add_contact_auto_accept,
                                };
                                self.address_book.contacts.push(contact);
                                let _ = self.address_book.save();
                                self.state = UiState::AddressBook;
                            }
                            Err(e) => {
                                self.state = UiState::AddContact {
                                    error: Some(format!("invalid node id: {e}")),
                                };
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.state = UiState::AddressBook;
                    }
                });
            }
            UiState::PickContact {
                ticket,
                name,
                size,
                kind,
                mime,
                peer,
                sent,
            } => {
                self.title_bar(ui, ctx, "📤 Send to");
                ui.label(format!(
                    "Choose a contact for \"{name}\" ({}, {type_label}):",
                    HumanBytes(size),
                    type_label = type_label(kind, &mime)
                ));
                ui.add_space(6.0);
                let mut chosen: Option<(EndpointId, String)> = None;
                if self.address_book.contacts.is_empty() {
                    ui.label("(no contacts yet — add some in the address book)");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(150.0)
                        .show(ui, |ui| {
                            for c in &self.address_book.contacts {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&c.name).strong());
                                    ui.label(RichText::new(short_id(&c.node_id)).weak().size(10.0));
                                    if ui.button("Send").clicked() {
                                        if let Ok(id) = c.endpoint_id() {
                                            chosen = Some((id, c.name.clone()));
                                        }
                                    }
                                });
                            }
                        });
                }
                ui.add_space(6.0);
                if let Some((node_id, contact_name)) = chosen {
                    self.last_offer_contact = Some((node_id, contact_name.clone()));
                    self.send_cmd(Command::SendOffer {
                        node_id,
                        ticket: ticket.clone(),
                        name: name.clone(),
                        size,
                        kind,
                        mime: mime.clone(),
                    });
                    self.offer_in_progress = true;
                    self.state = UiState::OfferPending {
                        contact_name,
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        sent,
                    };
                    return;
                }
                if ui.button("Back").clicked() {
                    self.state = UiState::Waiting {
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                        peer,
                        sent,
                    };
                }
            }
            UiState::OfferPending {
                contact_name,
                name,
                size,
                kind,
                mime,
                ..
            } => {
                self.title_bar(ui, ctx, "📤 Send");
                ui.label(format!(
                    "Asking {contact_name} to accept \"{name}\" ({}, {type_label})…",
                    HumanBytes(size),
                    type_label = type_label(kind, &mime)
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("waiting for a reply…");
                });
                ui.add_space(8.0);
                if ui.button("Cancel").clicked() {
                    self.close_operation();
                }
            }
            UiState::IncomingOffer {
                from_short,
                contact_name,
                name,
                size,
                kind,
                mime,
                ticket,
            } => {
                self.title_bar(ui, ctx, "📥 Incoming transfer");
                match &contact_name {
                    Some(cn) => {
                        ui.colored_label(
                            RECV_COLOR,
                            RichText::new(format!("📇 Known contact: \"{cn}\"")).strong(),
                        );
                    }
                    None => {
                        ui.colored_label(
                            AMBER,
                            RichText::new("⚠ Unknown sender — NOT in your address book").strong(),
                        );
                    }
                }
                ui.add_space(2.0);
                ui.label(format!("node id: {from_short}"));
                ui.add_space(2.0);
                ui.label("wants to send you:");
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "\"{name}\"  ({}, {type_label})",
                        HumanBytes(size),
                        type_label = type_label(kind, &mime)
                    ))
                    .strong(),
                );
                if contact_name.is_none() {
                    ui.add_space(4.0);
                    ui.colored_label(AMBER, "Only accept if you recognise this sender.");
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("✔ Accept").clicked() {
                        if let Some(r) = self.pending_offer_respond.take() {
                            let _ = r.send(true);
                        }
                        let result_tx = self.pending_offer_result.take();
                        if self.config.default_folder().is_some() {
                            self.state = UiState::Receiving {
                                status: "connecting ...".into(),
                                current: 0,
                                total: 0,
                            };
                            self.send_cmd(Command::ReceiveOffer { ticket, result_tx });
                        } else {
                            self.state = UiState::PickingFolder;
                            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                            self.send_cmd(Command::ReceiveOffer { ticket, result_tx });
                        }
                    }
                    if ui.button("✘ Decline").clicked() {
                        if let Some(r) = self.pending_offer_respond.take() {
                            let _ = r.send(false);
                        }
                        self.pending_offer_result.take();
                        self.state = UiState::Idle;
                    }
                });
            }
        }
    }
}

impl eframe::App for BalloonApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // fully transparent background, only the balloon/bubble is visible
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // apply events from the worker
        while let Ok(event) = self.evt_rx.try_recv() {
            self.apply(event);
        }
        // keep spinners animated while something is going on
        if !matches!(self.state, UiState::Idle) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        // resize the window to fit the current state
        let desired = self.desired_size();
        if self.last_size != desired {
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(desired));
            self.last_size = desired;
        }

        let frame = if matches!(self.state, UiState::Idle) {
            Frame::NONE
        } else {
            Frame::new()
                .fill(BUBBLE_BG)
                .corner_radius(CornerRadius::same(14))
                .stroke(Stroke::new(1.0, Color32::from_gray(80)))
                .inner_margin(Margin::same(12))
        };
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if matches!(self.state, UiState::Idle) {
                self.draw_balloon(ui, &ctx);
            } else {
                self.draw_state(ui, &ctx);
            }
        });
    }
}

fn first_dropped_path(files: &[egui::DroppedFile]) -> Option<PathBuf> {
    files.iter().find_map(|file| file.path.clone())
}

/// Compact, panic-safe preview of a node id.
fn short_id(id: &str) -> String {
    let count = id.chars().count();
    let prefix: String = id.chars().take(10).collect();
    if count > 10 {
        format!("{prefix}…")
    } else {
        id.to_string()
    }
}

/// Generate a short random suffix for temp directory names, avoiding an
/// extra dependency on the `uuid` crate.
fn uuid_like_suffix() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{nanos:09x}")
}

/// Generate a renamed filename by inserting ` (n)` before the extension.
/// `photo.png` with retry=1 → `photo (1).png`, retry=2 → `photo (2).png`.
/// Files without an extension get the suffix appended.
fn renamed_filename(filename: &str, retry: u32) -> String {
    let n = retry.max(1);
    match filename.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
        None => format!("{filename} ({n})"),
    }
}

fn start_send(
    path: PathBuf,
    cancel_send: &mut Option<oneshot::Sender<()>>,
    evt_tx: &std_mpsc::Sender<UiEvent>,
    ctx: &egui::Context,
    config: Arc<Config>,
) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    evt_tx
        .send(UiEvent::SendStarted {
            name,
            path: path.clone(),
        })
        .ok();
    ctx.request_repaint();

    let (c_tx, c_rx) = oneshot::channel();
    *cancel_send = Some(c_tx);
    let (se_tx, mut se_rx) = tokio_mpsc::channel(64);
    tokio::spawn(send_file(path, se_tx, c_rx, (*config).clone()));

    let evt_tx = evt_tx.clone();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        while let Some(e) = se_rx.recv().await {
            evt_tx.send(UiEvent::Send(e)).ok();
            ctx.request_repaint();
        }
    });
}

/// Kick off a receive after a save folder has been chosen.
#[allow(clippy::too_many_arguments)]
fn start_receive(
    ticket: BlobTicket,
    dir: PathBuf,
    recv_task: &mut Option<tokio::task::JoinHandle<()>>,
    evt_tx: &std_mpsc::Sender<UiEvent>,
    ctx: &egui::Context,
    decision_rx: oneshot::Receiver<OverwriteDecision>,
    result_tx: Option<oneshot::Sender<TransferResult>>,
    config: Arc<Config>,
) {
    let _ = evt_tx.send(UiEvent::ReceiveStarting);
    ctx.request_repaint();
    let (re_tx, mut re_rx) = tokio_mpsc::channel(64);
    *recv_task = Some(tokio::spawn(receive_ticket(
        ticket,
        dir,
        re_tx,
        decision_rx,
        (*config).clone(),
    )));
    let evt_tx = evt_tx.clone();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut result_tx = result_tx;
        while let Some(e) = re_rx.recv().await {
            // Intercept the terminal receive events to report the outcome
            // back to the sender (via the offer stream). Only sent once.
            match &e {
                ReceiveEvent::Completed { .. } => {
                    if let Some(tx) = result_tx.take() {
                        let _ = tx.send(TransferResult::Saved);
                    }
                }
                ReceiveEvent::KeptExisting { .. } => {
                    if let Some(tx) = result_tx.take() {
                        let _ = tx.send(TransferResult::KeptExisting);
                    }
                }
                ReceiveEvent::Error(_) => {
                    // Dropping result_tx makes the accept loop send RESULT_ERROR.
                    result_tx.take();
                }
                ReceiveEvent::SenderCancelled => {
                    // The sender already gave up, but drop the channel so the
                    // accept loop does not hang waiting for a result.
                    result_tx.take();
                }
                _ => {}
            }
            let _ = evt_tx.send(UiEvent::Receive(e));
            ctx.request_repaint();
        }
    });
}

/// The background worker owns the tokio runtime and drives the actual
/// sendme send/receive operations, plus the persistent contact endpoint used
/// to exchange transfer-ticket offers between two balloons.
fn spawn_worker(
    ctx: egui::Context,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<Command>,
    evt_tx: std_mpsc::Sender<UiEvent>,
    config: Arc<Config>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");
        rt.block_on(async move {
            let emit = |e: UiEvent| {
                evt_tx.send(e).ok();
                ctx.request_repaint();
            };

            // persistent contact endpoint for ticket offers
            let contact_ep = match async {
                let secret = load_or_create_secret()?;
                let ep = create_contact_endpoint(secret, config.relay_mode.to_relay_mode()).await?;
                anyhow::Ok(ep)
            }
            .await
            {
                Ok(ep) => {
                    emit(UiEvent::NodeIdReady(ep.id().to_string()));
                    let (offer_tx, mut offer_rx) = tokio_mpsc::channel::<IncomingOffer>(16);
                    tokio::spawn(run_accept_loop(
                        ep.clone(),
                        offer_tx,
                        config.heartbeat_interval(),
                        Duration::from_secs(config.timeouts.offer_conn_close_wait_secs),
                    ));
                    let evt_tx2 = evt_tx.clone();
                    let ctx2 = ctx.clone();
                    tokio::spawn(async move {
                        while let Some(offer) = offer_rx.recv().await {
                            let _ = evt_tx2.send(UiEvent::OfferReceived(offer));
                            ctx2.request_repaint();
                        }
                    });
                    Some(ep)
                }
                Err(e) => {
                    emit(UiEvent::OfferError(format!(
                        "contact endpoint unavailable: {e:#}"
                    )));
                    None
                }
            };

            let mut cancel_send: Option<oneshot::Sender<()>> = None;
            let mut recv_task: Option<tokio::task::JoinHandle<()>> = None;
            // Sender used to deliver the user's overwrite decision to a paused
            // receive task. Filled when a receive starts, drained on resolve.
            let mut conflict_respond: Option<oneshot::Sender<OverwriteDecision>> = None;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::PickAndSend => {
                        let file = rfd::AsyncFileDialog::new()
                            .set_title("sendme: choose a file to send")
                            .pick_file()
                            .await;
                        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                        match file {
                            None => emit(UiEvent::FilePickCancelled),
                            Some(file) => start_send(
                                file.path().to_path_buf(),
                                &mut cancel_send,
                                &evt_tx,
                                &ctx,
                                config.clone(),
                            ),
                        }
                    }
                    Command::SendPath(path) => {
                        start_send(path, &mut cancel_send, &evt_tx, &ctx, config.clone());
                    }
                    Command::SendOffer {
                        node_id,
                        ticket,
                        name,
                        size,
                        kind,
                        mime,
                    } => match &contact_ep {
                        Some(ep) => {
                            let ep = ep.clone();
                            let evt_tx2 = evt_tx.clone();
                            let ctx2 = ctx.clone();
                            tokio::spawn(async move {
                                let name_for_evt = name.clone();
                                let evt =
                                    match send_offer(&ep, node_id, ticket, name, size, kind, mime)
                                        .await
                                    {
                                        Ok(OfferResult::Saved) => {
                                            UiEvent::OfferTransferSaved { name: name_for_evt }
                                        }
                                        Ok(OfferResult::KeptExisting) => {
                                            UiEvent::OfferKeptExisting { name: name_for_evt }
                                        }
                                        Ok(OfferResult::Declined) => UiEvent::OfferRejected,
                                        Err(e) => UiEvent::OfferError(format!("{e:#}")),
                                    };
                                let _ = evt_tx2.send(evt);
                                ctx2.request_repaint();
                            });
                        }
                        None => emit(UiEvent::OfferError("contact endpoint unavailable".into())),
                    },
                    Command::CancelSend => {
                        if let Some(c) = cancel_send.take() {
                            c.send(()).ok();
                        }
                    }
                    Command::Receive { ticket } => match parse_ticket(&ticket) {
                        Err(e) => {
                            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                            emit(UiEvent::TicketInvalid(e.to_string()));
                        }
                        Ok(ticket) => {
                            // Use the configured default folder if set, skipping
                            // the folder picker; otherwise ask the user.
                            let target = match config.default_folder() {
                                Some(folder) => Some(folder.to_path_buf()),
                                None => {
                                    let dir = rfd::AsyncFileDialog::new()
                                        .set_title("sendme: choose where to save")
                                        .pick_folder()
                                        .await;
                                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                                    dir.map(|d| d.path().to_path_buf())
                                }
                            };
                            match target {
                                None => emit(UiEvent::FolderPickCancelled),
                                Some(dir) => {
                                    let (dc_tx, dc_rx) = oneshot::channel();
                                    conflict_respond = Some(dc_tx);
                                    start_receive(
                                        ticket,
                                        dir,
                                        &mut recv_task,
                                        &evt_tx,
                                        &ctx,
                                        dc_rx,
                                        None,
                                        config.clone(),
                                    );
                                }
                            }
                        }
                    },
                    Command::ReceiveOffer { ticket, result_tx } => match parse_ticket(&ticket) {
                        Err(e) => {
                            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                            emit(UiEvent::TicketInvalid(e.to_string()));
                        }
                        Ok(ticket) => {
                            let target = match config.default_folder() {
                                Some(folder) => Some(folder.to_path_buf()),
                                None => {
                                    let dir = rfd::AsyncFileDialog::new()
                                        .set_title("sendme: choose where to save")
                                        .pick_folder()
                                        .await;
                                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                                    dir.map(|d| d.path().to_path_buf())
                                }
                            };
                            match target {
                                None => emit(UiEvent::OfferFolderCancelled),
                                Some(dir) => {
                                    let (dc_tx, dc_rx) = oneshot::channel();
                                    conflict_respond = Some(dc_tx);
                                    start_receive(
                                        ticket,
                                        dir,
                                        &mut recv_task,
                                        &evt_tx,
                                        &ctx,
                                        dc_rx,
                                        result_tx,
                                        config.clone(),
                                    );
                                }
                            }
                        }
                    },
                    Command::CancelReceive => {
                        if let Some(task) = recv_task.take() {
                            task.abort();
                        }
                    }
                    Command::ResolveConflict(decision) => {
                        if let Some(tx) = conflict_respond.take() {
                            let _ = tx.send(decision);
                        }
                    }
                }
            }
        });
    });
}

fn main() -> eframe::Result {
    // Initialise logging. The `RUST_LOG` environment variable, if set, takes
    // precedence; otherwise the `log_level` from config.yaml is used.
    use tracing_subscriber::EnvFilter;

    // Load and validate the config. On any error — a malformed YAML file,
    // a wrong type, or a semantic issue like a missing/unwritable save folder
    // — fire a CRITICAL desktop notification so the user actually notices.
    // A silent fallback to defaults would leave them wondering why their
    // settings "didn't take effect". The critical urgency keeps the
    // notification on screen until dismissed on most Linux daemons.
    let config = match Config::load() {
        Ok(mut cfg) => {
            let warnings = cfg.validate();
            if !warnings.is_empty() {
                fire_critical_notification("sendme-balloon: config issues", warnings.join("\n"));
            }
            cfg
        }
        Err(e) => {
            fire_critical_notification(
                "sendme-balloon: config error",
                format!(
                    "Could not read config.yaml — using default settings.\n\
                     Please check the file for errors:\n{e:#}"
                ),
            );
            Config::default()
        }
    };
    // `validate()` may have mutated `config` (e.g. cleared a bad save folder),
    // so re-sync the log_level from the possibly-corrected config.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let config = Arc::new(config);
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size(IDLE_SIZE)
            .with_decorations(false)
            .with_transparent(true)
            .with_drag_and_drop(true)
            .with_always_on_top()
            .with_resizable(false)
            .with_app_id("sendme-balloon")
            .with_title("sendme balloon"),
        ..Default::default()
    };
    #[cfg(target_os = "linux")]
    let options = {
        let mut options = options;
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("DISPLAY").is_some() {
            // winit 0.30 has no Wayland data-device support. XWayland provides
            // working Xdnd events on compositors such as Sway/wlroots.
            options.event_loop_builder = Some(Box::new(|builder| {
                builder.with_x11();
            }));
        }
        options
    };
    eframe::run_native(
        "sendme balloon",
        options,
        Box::new(move |cc| Ok(Box::new(BalloonApp::new(cc, config.clone())))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_in_state(state: UiState) -> (BalloonApp, tokio_mpsc::UnboundedReceiver<Command>) {
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        let (_evt_tx, evt_rx) = std_mpsc::channel();
        (
            BalloonApp {
                state,
                ticket_text: String::new(),
                copied_at: None,
                last_size: Vec2::ZERO,
                cmd_tx,
                evt_rx,
                node_id: String::new(),
                address_book: AddressBook::default(),
                config: Arc::new(Config::default()),
                pending_offer_respond: None,
                pending_offer_result: None,
                add_contact_name: String::new(),
                add_contact_node_id: String::new(),
                add_contact_email: String::new(),
                add_contact_auto_accept: false,
                autostart_enabled: false,
                offer_in_progress: false,
                auto_accepted_active: false,
                last_send_path: None,
                last_offer_contact: None,
                retry_contact: None,
                retry_count: 0,
            },
            cmd_rx,
        )
    }

    #[test]
    fn dropped_file_does_not_require_pointer_coordinates() {
        let path = PathBuf::from("example.txt");
        let files = vec![egui::DroppedFile {
            path: Some(path.clone()),
            ..Default::default()
        }];

        assert_eq!(first_dropped_path(&files), Some(path));
    }

    #[test]
    fn closing_send_cancels_without_quitting() {
        let (mut app, mut commands) = app_in_state(UiState::Waiting {
            ticket: "ticket".into(),
            name: "example.txt".into(),
            size: 10,
            kind: FileKind::File,
            mime: "text/plain".into(),
            peer: false,
            sent: 0,
        });

        app.close_operation();

        assert!(matches!(app.state, UiState::Idle));
        assert!(matches!(commands.try_recv(), Ok(Command::CancelSend)));

        app.apply(UiEvent::SendStarted {
            name: "example.txt".into(),
            path: PathBuf::from("example.txt"),
        });
        assert!(matches!(app.state, UiState::Idle));
    }

    #[test]
    fn cancelling_file_chooser_returns_to_idle_balloon() {
        let (mut app, _) = app_in_state(UiState::PickingFile);

        app.apply(UiEvent::FilePickCancelled);

        assert!(matches!(app.state, UiState::Idle));
    }

    #[test]
    fn short_id_truncates_long_ids() {
        let id = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnop";
        let s = short_id(id);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 11);
    }

    #[test]
    fn short_id_preserves_short_ids() {
        let id = "abc";
        assert_eq!(short_id(id), "abc");
    }

    #[test]
    fn renamed_filename_inserts_counter_before_extension() {
        assert_eq!(renamed_filename("photo.png", 1), "photo (1).png");
        assert_eq!(renamed_filename("photo.png", 2), "photo (2).png");
        assert_eq!(renamed_filename("archive.tar.gz", 1), "archive.tar (1).gz");
    }

    #[test]
    fn renamed_filename_handles_no_extension() {
        assert_eq!(renamed_filename("README", 1), "README (1)");
    }

    #[test]
    fn renamed_filename_clamps_to_minimum_one() {
        assert_eq!(renamed_filename("photo.png", 0), "photo (1).png");
    }

    #[test]
    fn type_label_for_directory() {
        assert_eq!(type_label(FileKind::Directory, ""), "directory");
        // mime is ignored for directories
        assert_eq!(
            type_label(FileKind::Directory, "application/pdf"),
            "directory"
        );
    }

    #[test]
    fn type_label_for_known_mime_is_friendly_plus_raw() {
        let label = type_label(FileKind::File, "application/pdf");
        assert!(label.contains("PDF"), "label was: {label}");
        assert!(label.contains("application/pdf"), "label was: {label}");
    }

    #[test]
    fn type_label_for_unknown_mime_falls_back_to_file() {
        assert_eq!(type_label(FileKind::File, ""), "file");
        assert_eq!(
            type_label(FileKind::File, "application/octet-stream"),
            "file"
        );
    }

    #[test]
    fn incoming_offer_while_busy_is_declined() {
        let (mut app, _cmds) = app_in_state(UiState::Waiting {
            ticket: "t".into(),
            name: "n".into(),
            size: 1,
            kind: FileKind::File,
            mime: "".into(),
            peer: false,
            sent: 0,
        });
        let (tx, rx) = oneshot::channel();
        let (rtx, _rrx) = oneshot::channel();
        let offer = IncomingOffer {
            from: "f".into(),
            from_short: "f".into(),
            name: "n".into(),
            size: 1,
            kind: FileKind::File,
            mime: "".into(),
            ticket: "t".into(),
            respond: tx,
            result_tx: rtx,
        };
        app.apply(UiEvent::OfferReceived(offer));
        // busy -> declined, state unchanged
        assert!(matches!(app.state, UiState::Waiting { .. }));
        assert_eq!(rx.blocking_recv(), Ok(false));
    }

    #[test]
    fn incoming_offer_when_idle_prompts_user() {
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        let (tx, rx) = oneshot::channel();
        let (rtx, _rrx) = oneshot::channel();
        let offer = IncomingOffer {
            from: "f".into(),
            from_short: "f".into(),
            name: "pic.png".into(),
            size: 42,
            kind: FileKind::File,
            mime: "image/png".into(),
            ticket: "tk".into(),
            respond: tx,
            result_tx: rtx,
        };
        app.apply(UiEvent::OfferReceived(offer));
        assert!(matches!(app.state, UiState::IncomingOffer { .. }));
        // simulate the user declining via close_operation
        app.close_operation();
        assert!(matches!(app.state, UiState::Idle));
        assert!(rx.blocking_recv().is_err());
    }

    #[test]
    fn peer_cancelled_shows_speaking_error_in_waiting() {
        let (mut app, _cmds) = app_in_state(UiState::Waiting {
            ticket: "t".into(),
            name: "report.pdf".into(),
            size: 1024,
            kind: FileKind::File,
            mime: "application/pdf".into(),
            peer: true,
            sent: 512,
        });
        app.offer_in_progress = false;
        app.apply(UiEvent::Send(SendEvent::PeerCancelled));
        match &app.state {
            UiState::Error { message } => {
                assert!(message.contains("cancelled"), "message was: {message}");
                assert!(message.contains("report.pdf"), "message was: {message}");
            }
            _ => panic!("expected Error state after peer cancellation"),
        }
    }

    #[test]
    fn peer_cancelled_suppressed_during_offer_flow() {
        let (mut app, _cmds) = app_in_state(UiState::Waiting {
            ticket: "t".into(),
            name: "report.pdf".into(),
            size: 1024,
            kind: FileKind::File,
            mime: "application/pdf".into(),
            peer: true,
            sent: 512,
        });
        app.offer_in_progress = true;
        app.apply(UiEvent::Send(SendEvent::PeerCancelled));
        // suppressed — the offer flow reports the outcome separately
        assert!(matches!(app.state, UiState::Waiting { .. }));
    }

    #[test]
    fn peer_cancelled_in_pick_contact_shows_error() {
        let (mut app, _cmds) = app_in_state(UiState::PickContact {
            ticket: "t".into(),
            name: "photo.png".into(),
            size: 2048,
            kind: FileKind::File,
            mime: "image/png".into(),
            peer: true,
            sent: 10,
        });
        app.apply(UiEvent::Send(SendEvent::PeerCancelled));
        match &app.state {
            UiState::Error { message } => {
                assert!(message.contains("cancelled"), "message was: {message}");
                assert!(message.contains("photo.png"), "message was: {message}");
            }
            _ => panic!("expected Error state after peer cancellation"),
        }
    }

    #[test]
    fn sender_cancelled_shows_speaking_error_while_receiving() {
        let (mut app, _cmds) = app_in_state(UiState::Receiving {
            status: "downloading ...".into(),
            current: 100,
            total: 1000,
        });
        app.apply(UiEvent::Receive(ReceiveEvent::SenderCancelled));
        match &app.state {
            UiState::Error { message } => {
                assert!(message.contains("sender"), "message was: {message}");
                assert!(message.contains("cancelled"), "message was: {message}");
            }
            _ => panic!("expected Error state after sender cancellation"),
        }
    }

    #[test]
    fn sender_cancelled_ignored_outside_receiving() {
        // arriving in an unrelated state must not clobber it
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        app.apply(UiEvent::Receive(ReceiveEvent::SenderCancelled));
        assert!(matches!(app.state, UiState::Idle));
    }

    /// Helper: build an IncomingOffer for a real node id, with throwaway
    /// reply channels.
    fn make_offer(from: &str) -> IncomingOffer {
        let (respond, _rx) = oneshot::channel();
        let (result_tx, _rx) = oneshot::channel();
        IncomingOffer {
            from: from.to_string(),
            from_short: "short".into(),
            name: "pic.png".into(),
            size: 42,
            kind: FileKind::File,
            mime: "image/png".into(),
            ticket: "tk".into(),
            respond,
            result_tx,
        }
    }

    fn with_default_folder(app: &mut BalloonApp, folder: &str) {
        let cfg = Config {
            default_save_folder: Some(PathBuf::from(folder)),
            ..Config::default()
        };
        app.config = Arc::new(cfg);
    }

    #[test]
    fn auto_accept_ignored_without_default_folder_even_if_contact_opted_in() {
        use iroh::SecretKey;
        let key = SecretKey::generate();
        let id_str = key.public().to_string();
        let offer = make_offer(&id_str);
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        app.address_book.contacts.push(Contact {
            name: "alice".into(),
            node_id: id_str,
            email: String::new(),
            auto_accept: true,
        });
        // contact opted in, but no default folder -> cannot auto-accept
        assert!(!app.should_auto_accept(&offer));
    }

    #[test]
    fn auto_accept_fires_for_opted_in_contact_when_folder_set() {
        use iroh::SecretKey;
        let key = SecretKey::generate();
        let id_str = key.public().to_string();
        let offer = make_offer(&id_str);
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        app.address_book.contacts.push(Contact {
            name: "alice".into(),
            node_id: id_str.clone(),
            email: "alice@example.com".into(),
            auto_accept: true,
        });
        with_default_folder(&mut app, "/tmp/sendme");
        assert!(app.should_auto_accept(&offer));
    }

    #[test]
    fn global_auto_accept_applies_to_unknown_senders_only_with_folder() {
        use iroh::SecretKey;
        let key = SecretKey::generate();
        let id_str = key.public().to_string(); // not in the address book
        let offer = make_offer(&id_str);
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        // global on, no folder -> ignored
        let cfg = Config {
            auto_accept_offers: true,
            ..Config::default()
        };
        app.config = Arc::new(cfg);
        assert!(!app.should_auto_accept(&offer));
        // folder set -> now accepts even unknown senders
        with_default_folder(&mut app, "/tmp/sendme");
        // with_default_folder resets auto_accept_offers to false, so set again
        let cfg = Config {
            auto_accept_offers: true,
            default_save_folder: Some(PathBuf::from("/tmp/sendme")),
            ..Config::default()
        };
        app.config = Arc::new(cfg);
        assert!(app.should_auto_accept(&offer));
    }

    #[test]
    fn auto_accept_off_for_normal_contact_when_global_off() {
        use iroh::SecretKey;
        let key = SecretKey::generate();
        let id_str = key.public().to_string();
        let offer = make_offer(&id_str);
        let (mut app, _cmds) = app_in_state(UiState::Idle);
        app.address_book.contacts.push(Contact {
            name: "bob".into(),
            node_id: id_str,
            email: String::new(),
            auto_accept: false,
        });
        with_default_folder(&mut app, "/tmp/sendme");
        assert!(!app.should_auto_accept(&offer));
    }
}
