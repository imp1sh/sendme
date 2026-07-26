//! Backend for the `sendme-balloon` desktop app.
//!
//! This module reuses the core send/receive machinery of sendme, but instead
//! of printing to a terminal it reports progress through channels, so a GUI
//! can display it. It also supports cancellation.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use data_encoding::HEXLOWER;
use indicatif::{MultiProgress, ProgressDrawTarget};
use iroh::{endpoint::presets, Endpoint, RelayMode};
use iroh_blobs::{
    api::remote::GetProgressItem,
    format::collection::Collection,
    get::request::get_hash_seq_and_sizes,
    provider::{
        self,
        events::{ConnectMode, EventMask, EventSender, ProviderMessage, RequestUpdate},
    },
    store::fs::FsStore,
    ticket::BlobTicket,
    BlobFormat, BlobsProtocol,
};
use n0_future::StreamExt;
use rand::RngExt;
use tokio::{
    select,
    sync::{mpsc, oneshot, Mutex},
};

use crate::config::Config;
use crate::{
    apply_options, export, export_conflicts, get_or_create_secret, import, AddrInfoOptions,
};

/// Whether an offered transfer is a single file or a directory.
///
/// Carried through the offer protocol so the receiver can show the kind of
/// the incoming transfer before deciding whether to accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
}

/// Wire byte for [`FileKind::File`].
pub const KIND_FILE: u8 = 0;
/// Wire byte for [`FileKind::Directory`].
pub const KIND_DIRECTORY: u8 = 1;

impl FileKind {
    /// Encode this kind as a single wire byte.
    pub fn to_byte(self) -> u8 {
        match self {
            FileKind::File => KIND_FILE,
            FileKind::Directory => KIND_DIRECTORY,
        }
    }

    /// Decode a kind from a single wire byte.
    pub fn from_byte(b: u8) -> anyhow::Result<Self> {
        match b {
            KIND_FILE => Ok(FileKind::File),
            KIND_DIRECTORY => Ok(FileKind::Directory),
            other => anyhow::bail!("unknown file kind byte: {other}"),
        }
    }

    /// A short, human-readable label suitable for the UI.
    pub fn label(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Directory => "directory",
        }
    }
}

/// Guess the MIME type of `path` from its extension. Returns an empty string
/// for directories (which have no meaningful MIME type) and for the
/// non-balloon build (where [`mime_guess`] is not available).
fn guess_mime(path: &std::path::Path) -> String {
    if path.is_dir() {
        return String::new();
    }
    #[cfg(feature = "balloon")]
    {
        mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string()
    }
    #[cfg(not(feature = "balloon"))]
    {
        let _ = path;
        String::new()
    }
}

/// Events emitted while providing (sending) data.
#[derive(Debug, Clone)]
pub enum SendEvent {
    /// The file was imported, the ticket is ready and we are waiting for a peer.
    TicketReady {
        ticket: String,
        name: String,
        size: u64,
        /// Whether the offered path is a single file or a directory.
        kind: FileKind,
        /// Guessed MIME type (empty for directories or when unknown).
        mime: String,
    },
    /// A peer connected to us.
    PeerConnected,
    /// Bytes sent so far for the current outgoing request.
    Progress { sent: u64 },
    /// The data was completely transferred to a peer.
    Completed,
    /// A transfer was in progress but the peer cancelled (or dropped the
    /// connection) before completing it.
    PeerCancelled,
    /// Something went wrong.
    Error(String),
}

/// Events emitted while receiving data.
#[derive(Debug, Clone)]
pub enum ReceiveEvent {
    /// Connecting to the sender.
    Connecting,
    /// Connected, download is starting.
    Starting { total_files: u64, payload_size: u64 },
    /// Download progress in bytes.
    Progress { current: u64, total: u64 },
    /// Download done, exporting to the target directory.
    Exporting,
    /// One or more target files already exist; awaiting a user decision.
    Conflict { targets: Vec<PathBuf> },
    /// Everything was saved to the target directory.
    Completed { target: PathBuf },
    /// The user chose to keep the existing files; nothing was overwritten.
    KeptExisting { target: PathBuf },
    /// The sender cancelled the transfer (or dropped the connection) while the
    /// download was in progress.
    SenderCancelled,
    /// Something went wrong.
    Error(String),
}

/// User's decision when an incoming transfer would overwrite existing files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwriteDecision {
    /// Remove the existing files and save the incoming ones.
    Overwrite,
    /// Leave the existing files untouched; do not save the incoming ones.
    KeepExisting,
}

/// How a receive resolves a filename conflict (when the target file already
/// exists).
///
/// Manual receives (the user pasted a ticket) use [`Self::Prompt`]: the receive
/// emits a [`ReceiveEvent::Conflict`] and waits for the GUI to deliver the
/// user's decision on the embedded oneshot.
///
/// Managed receives (offers handled by the transfer manager) use
/// [`Self::Auto`]: the decision is taken from the configuration up front, with
/// no prompt and no `Conflict` event. This keeps background/parallel transfers
/// from stalling on a human decision.
#[derive(Debug)]
pub enum ConflictResolver {
    /// Emit a [`ReceiveEvent::Conflict`] and wait on this oneshot for the
    /// user's decision.
    Prompt(oneshot::Receiver<OverwriteDecision>),
    /// Resolve automatically with this decision (no prompt, no event).
    Auto(OverwriteDecision),
}

/// Outcome of a receive, reported back to the caller so it can emit the
/// appropriate terminal event after cleanup.
enum ReceiveOutcome {
    Saved,
    KeptExisting,
    /// The sender cancelled (or dropped) the connection mid-transfer.
    SenderCancelled,
}

/// Parse a ticket from user input.
///
/// Accepts either a bare ticket or a whole `sendme receive <ticket>` command
/// as it is put into the clipboard by the CLI or by the balloon app.
pub fn parse_ticket(input: &str) -> anyhow::Result<BlobTicket> {
    let input = input.trim();
    // if a whole command was pasted, use its last whitespace separated token
    let token = input.split_whitespace().last().unwrap_or(input);
    Ok(token.parse::<BlobTicket>()?)
}

/// Send a single file (or directory), reporting progress via `events`.
///
/// The transfer can be cancelled by sending to (or dropping) `cancel`.
/// Terminates once the data was transferred completely to one peer, on
/// cancellation, or on error.
///
/// `config` supplies the relay mode, import parallelism and the various
/// timeouts, replacing previously hardcoded values.
pub async fn send_file(
    path: PathBuf,
    events: mpsc::Sender<SendEvent>,
    cancel: oneshot::Receiver<()>,
    config: Config,
) {
    // use a temp dir for the blob store, it is removed again afterwards
    let suffix = rand::rng().random::<[u8; 16]>();
    let blobs_data_dir =
        std::env::temp_dir().join(format!("sendme-balloon-send-{}", HEXLOWER.encode(&suffix)));
    let res = send_file_inner(
        path,
        blobs_data_dir.clone(),
        events.clone(),
        cancel,
        &config,
    )
    .await;
    tokio::fs::remove_dir_all(&blobs_data_dir).await.ok();
    if let Err(e) = res {
        events.send(SendEvent::Error(format!("{e:#}"))).await.ok();
    }
}

async fn send_file_inner(
    path: PathBuf,
    blobs_data_dir: PathBuf,
    events: mpsc::Sender<SendEvent>,
    mut cancel: oneshot::Receiver<()>,
    config: &Config,
) -> anyhow::Result<()> {
    let secret_key = get_or_create_secret(false)?;
    let relay_mode = config.relay_mode.to_relay_mode();
    let builder = Endpoint::builder(presets::N0)
        .alpns(vec![iroh_blobs::protocol::ALPN.to_vec()])
        .secret_key(secret_key)
        .relay_mode(relay_mode.clone());
    tokio::fs::create_dir_all(&blobs_data_dir).await?;
    let endpoint = builder.bind().await?;
    let store = FsStore::load(&blobs_data_dir).await?;
    let (progress_tx, mut progress_rx) = mpsc::channel(32);
    let blobs = BlobsProtocol::new(
        &store,
        Some(EventSender::new(
            progress_tx,
            EventMask {
                connected: ConnectMode::Notify,
                get: provider::events::RequestMode::NotifyLog,
                ..EventMask::DEFAULT
            },
        )),
    );
    // import the file into the store, hidden progress (the GUI shows its own)
    let mut mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    // Collect metadata about the offered path so the receiver can show the
    // kind and type of the incoming transfer before deciding to accept.
    let kind = if path.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    let mime = guess_mime(&path);
    let (temp_tag, size, _collection) = import(path, blobs.store(), &mut mp, config.jobs).await?;
    let hash = temp_tag.hash();

    let router = iroh::protocol::Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, blobs.clone())
        .spawn();

    // wait for the endpoint to figure out its address before making a ticket
    let ep = router.endpoint();
    tokio::time::timeout(
        Duration::from_secs(config.timeouts.endpoint_online_wait_secs),
        async {
            if !matches!(relay_mode, RelayMode::Disabled) {
                let _ = ep.online().await;
            }
        },
    )
    .await
    .ok();

    let mut addr = router.endpoint().addr();
    apply_options(&mut addr, AddrInfoOptions::RelayAndAddresses);
    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);
    events
        .send(SendEvent::TicketReady {
            ticket: ticket.to_string(),
            name,
            size,
            kind,
            mime,
        })
        .await
        .ok();

    // Track requests per connection so we can tell when a peer has fetched
    // everything successfully: the connection is closed with at least one
    // completed request and no started request left unfinished or aborted.
    #[derive(Default)]
    struct ConnState {
        started: usize,
        completed: usize,
        aborted: usize,
    }
    let mut connections: BTreeMap<u64, ConnState> = BTreeMap::new();
    // results of per-request watcher tasks: (connection_id, completed_ok)
    let (req_tx, mut req_rx) = mpsc::channel::<(u64, bool)>(32);
    let apply = |connections: &mut BTreeMap<u64, ConnState>, conn_id: u64, ok: bool| {
        if let Some(c) = connections.get_mut(&conn_id) {
            if ok {
                c.completed += 1;
            } else {
                c.aborted += 1;
            }
        }
    };
    let completed = loop {
        select! {
            _ = &mut cancel => break false,
            Some((conn_id, ok)) = req_rx.recv() => {
                apply(&mut connections, conn_id, ok);
            }
            msg = progress_rx.recv() => {
                let Some(msg) = msg else { break false };
                match msg {
                    ProviderMessage::ClientConnectedNotify(_) => {
                        events.send(SendEvent::PeerConnected).await.ok();
                    }
                    ProviderMessage::GetRequestReceivedNotify(msg) => {
                        let conn_id = msg.connection_id;
                        connections.entry(conn_id).or_default().started += 1;
                        let mut rx = msg.rx;
                        let req_tx = req_tx.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let mut ok = false;
                            while let Ok(Some(update)) = rx.recv().await {
                                match update {
                                    RequestUpdate::Progress(p) => {
                                        events
                                            .try_send(SendEvent::Progress { sent: p.end_offset })
                                            .ok();
                                    }
                                    RequestUpdate::Completed(_) => {
                                        ok = true;
                                        break;
                                    }
                                    RequestUpdate::Aborted(_) => break,
                                    _ => {}
                                }
                            }
                            req_tx.send((conn_id, ok)).await.ok();
                        });
                    }
                    ProviderMessage::ConnectionClosed(msg) => {
                        // give in-flight watcher results a moment to arrive
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        while let Ok((conn_id, ok)) = req_rx.try_recv() {
                            apply(&mut connections, conn_id, ok);
                        }
                        if let Some(c) = connections.remove(&msg.connection_id) {
                            if c.completed >= 1 && c.aborted == 0 && c.completed == c.started {
                                break true;
                            }
                            // a transfer was in progress but did not finish:
                            // the peer cancelled (or the connection dropped)
                            // mid-transfer. inform the UI and stop waiting.
                            if c.started > 0 {
                                events.send(SendEvent::PeerCancelled).await.ok();
                                break false;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    };
    if completed {
        events.send(SendEvent::Completed).await.ok();
    }
    drop(temp_tag);
    tokio::time::timeout(
        Duration::from_secs(config.timeouts.router_shutdown_secs),
        router.shutdown(),
    )
    .await
    .ok();
    drop(router);
    store.shutdown().await.ok();
    Ok(())
}

/// Returns true when an error chain indicates the peer deliberately closed
/// the QUIC connection — the signature of the sender cancelling a transfer
/// (as opposed to a transient network failure or a local error).
///
/// iroh/quice reports such a close as "... closed by peer: <code>" in the
/// innermost error source, so we scan the whole [`anyhow::Error::chain`].
fn is_peer_initiated_close(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.to_string().contains("closed by peer"))
}

/// Receive the data behind `ticket` and save it below `target_dir`.
///
/// If saving would overwrite existing files, a [`ReceiveEvent::Conflict`] is
/// emitted (only for the [`ConflictResolver::Prompt`] variant) and the future
/// pauses until a decision arrives on the embedded oneshot. The
/// [`ConflictResolver::Auto`] variant resolves conflicts silently using the
/// pre-supplied decision.
///
/// `endpoint` is a shared iroh endpoint used to connect to the sender. Sharing
/// one endpoint across all concurrent receives (instead of creating one per
/// receive) saves sockets, relay state and magicsocket overhead. The caller
/// owns the endpoint's lifecycle; this function does NOT close it.
///
/// `export_lock` serialises the conflict-check + export phase across parallel
/// receives so two transfers racing for the same filename cannot both write.
/// Downloads still run in parallel; only the final file-placement step is
/// serialised. Pass a shared lock for all concurrent receives; `Arc::new(
/// Mutex::new(()))` for a stand-alone receive.
///
/// `config` supplies the relay mode and download chunk size, replacing
/// previously hardcoded values.
pub async fn receive_ticket(
    endpoint: Endpoint,
    ticket: BlobTicket,
    target_dir: PathBuf,
    events: mpsc::Sender<ReceiveEvent>,
    conflict: ConflictResolver,
    export_lock: Arc<Mutex<()>>,
    config: Config,
) {
    match receive_ticket_inner(
        &endpoint,
        &ticket,
        &target_dir,
        events.clone(),
        conflict,
        export_lock,
        &config,
    )
    .await
    {
        Ok(ReceiveOutcome::Saved) => {
            events
                .send(ReceiveEvent::Completed { target: target_dir })
                .await
                .ok();
        }
        Ok(ReceiveOutcome::KeptExisting) => {
            events
                .send(ReceiveEvent::KeptExisting { target: target_dir })
                .await
                .ok();
        }
        Ok(ReceiveOutcome::SenderCancelled) => {
            events.send(ReceiveEvent::SenderCancelled).await.ok();
        }
        Err(e) => {
            events
                .send(ReceiveEvent::Error(format!("{e:#}")))
                .await
                .ok();
        }
    }
}

async fn receive_ticket_inner(
    endpoint: &Endpoint,
    ticket: &BlobTicket,
    target_dir: &Path,
    events: mpsc::Sender<ReceiveEvent>,
    conflict: ConflictResolver,
    export_lock: Arc<Mutex<()>>,
    config: &Config,
) -> anyhow::Result<ReceiveOutcome> {
    let addr = ticket.addr().clone();
    // temp dir for the blob store; keyed by hash so interrupted downloads resume
    let iroh_data_dir =
        std::env::temp_dir().join(format!("sendme-balloon-recv-{}", ticket.hash().to_hex()));
    let db = FsStore::load(&iroh_data_dir).await?;
    // Becomes true once we have established a connection to the sender. An
    // error afterwards whose chain contains "closed by peer" is treated as a
    // deliberate sender cancellation rather than a generic connection fault.
    let mut connected = false;
    let res = async {
        let hash_and_format = ticket.hash_and_format();
        let local = db.remote().local(hash_and_format).await?;
        if !local.is_complete() {
            events.send(ReceiveEvent::Connecting).await.ok();
            let connection = endpoint.connect(addr, iroh_blobs::protocol::ALPN).await?;
            connected = true;
            let (_hash_seq, sizes) = get_hash_seq_and_sizes(
                &connection,
                &hash_and_format.hash,
                config.chunk_size_bytes(),
                None,
            )
            .await?;
            let total_size = sizes.iter().copied().sum::<u64>();
            let payload_size = sizes.iter().skip(2).copied().sum::<u64>();
            let total_files = (sizes.len().saturating_sub(1)) as u64;
            events
                .send(ReceiveEvent::Starting {
                    total_files,
                    payload_size,
                })
                .await
                .ok();
            let local_size = local.local_bytes();
            let get = db.remote().execute_get(connection, local.missing());
            let mut stream = get.stream();
            while let Some(item) = stream.next().await {
                match item {
                    GetProgressItem::Progress(offset) => {
                        events
                            .try_send(ReceiveEvent::Progress {
                                current: local_size + offset,
                                total: total_size,
                            })
                            .ok();
                    }
                    GetProgressItem::Done(_) => break,
                    GetProgressItem::Error(cause) => {
                        anyhow::bail!(cause);
                    }
                }
            }
        }
        let collection = Collection::load(hash_and_format.hash, db.as_ref()).await?;
        // Serialise the conflict-check + export phase across parallel receives.
        // Two transfers racing for the same filename would otherwise both see
        // "no conflict" and both try to write, with the second one erroring
        // inside `export`. The lock only covers placement, not the download,
        // so parallel downloads are unaffected.
        let mut conflict = conflict;
        let _guard = export_lock.lock().await;
        let conflicts = export_conflicts(&collection, target_dir)?;
        if conflicts.is_empty() {
            events.send(ReceiveEvent::Exporting).await.ok();
            let mut mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
            export(&db, collection, target_dir, &mut mp, false).await?;
            anyhow::Ok(ReceiveOutcome::Saved)
        } else {
            // Resolve the conflict: prompt the user (manual receives) or use
            // the pre-supplied automatic decision (managed receives).
            let decision = match &mut conflict {
                ConflictResolver::Prompt(decision_rx) => {
                    events
                        .send(ReceiveEvent::Conflict { targets: conflicts })
                        .await
                        .ok();
                    decision_rx.await.unwrap_or(OverwriteDecision::KeepExisting)
                }
                ConflictResolver::Auto(d) => *d,
            };
            match decision {
                OverwriteDecision::KeepExisting => anyhow::Ok(ReceiveOutcome::KeptExisting),
                OverwriteDecision::Overwrite => {
                    events.send(ReceiveEvent::Exporting).await.ok();
                    let mut mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
                    export(&db, collection, target_dir, &mut mp, true).await?;
                    anyhow::Ok(ReceiveOutcome::Saved)
                }
            }
        }
    }
    .await;
    // The endpoint is shared across all concurrent receives; do NOT close it
    // here. The caller (worker) owns its lifecycle.
    db.shutdown().await.ok();
    // A peer-initiated close after we connected means the sender cancelled
    // the transfer. Translate that into a clear outcome instead of leaking a
    // cryptic "connection lost: closed by peer" error to the user.
    let res = match res {
        Ok(outcome) => Ok(outcome),
        Err(e) if connected && is_peer_initiated_close(&e) => Ok(ReceiveOutcome::SenderCancelled),
        Err(e) => Err(e),
    };
    if res.is_ok() {
        tokio::fs::remove_dir_all(&iroh_data_dir).await.ok();
    }
    res
}

// ── Autostart (desktop integration) ───────────────────────────────────────
// Gated on the balloon feature because the `dirs` crate is optional.

#[cfg(feature = "balloon")]
fn autostart_path() -> PathBuf {
    let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    config.join("autostart").join("sendme-balloon.desktop")
}

#[cfg(feature = "balloon")]
fn autostart_desktop_entry(exec_path: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=sendme balloon\n\
         Comment=Send and receive files over the internet\n\
         Exec={exec_path}\n\
         Icon=sendme-balloon\n\
         Terminal=false\n\
         Categories=Network;FileTransfer;\n\
         StartupWMClass=sendme-balloon\n\
         X-GNOME-Autostart-enabled=true\n"
    )
}

#[cfg(feature = "balloon")]
pub fn autostart_is_enabled() -> bool {
    autostart_path().exists()
}

#[cfg(feature = "balloon")]
pub fn enable_autostart() -> anyhow::Result<PathBuf> {
    let path = autostart_path();
    let exec = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "sendme-balloon".to_string());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, autostart_desktop_entry(&exec))
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(feature = "balloon")]
pub fn disable_autostart() -> anyhow::Result<()> {
    let path = autostart_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| anyhow::anyhow!("removing {}: {e}", path.display()))?;
    }
    Ok(())
}

// ── Disk-space guard ───────────────────────────────────────────────────────

/// Return the available space (in bytes) on the filesystem containing `path`.
///
/// On Unix this calls `statvfs(2)` via the `libc` crate. On non-Unix platforms
/// (or if the syscall fails) it returns `None`, which callers treat as "skip
/// the guard" so the app degrades gracefully instead of refusing to run.
#[cfg(feature = "balloon")]
pub fn available_space(path: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let c_path = CString::new(path.to_str()?).ok()?;
        let mut statv: libc::statvfs = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::statvfs(c_path.as_ptr(), &mut statv) };
        if ret != 0 {
            return None;
        }
        // frsize * bavail gives the actual free bytes available to us.
        let frsize = statv.f_frsize as u64;
        let bavail = statv.f_bavail as u64;
        Some(frsize.saturating_mul(bavail))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_initiated_close_detected_from_chain() {
        // the innermost source carries the quic "closed by peer" message
        let inner = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "closed by peer: 0");
        let e = anyhow::Error::new(inner).context("read: connection lost");
        assert!(is_peer_initiated_close(&e));
    }

    #[test]
    fn peer_initiated_close_detected_when_top_level() {
        let e = anyhow::anyhow!("connection lost: closed by peer: 0");
        assert!(is_peer_initiated_close(&e));
    }

    #[test]
    fn transient_error_not_classified_as_peer_close() {
        let e = anyhow::anyhow!("operation timed out");
        assert!(!is_peer_initiated_close(&e));

        let inner = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out");
        let e = anyhow::Error::new(inner).context("connecting to sender");
        assert!(!is_peer_initiated_close(&e));
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn available_space_returns_positive_for_temp_dir() {
        let dir = std::env::temp_dir();
        let space = available_space(&dir);
        // On all realistic platforms /tmp has at least 1 KiB free.
        // If the platform lacks the syscall, None is acceptable.
        if let Some(s) = space {
            assert!(s > 1024, "expected >1KiB free, got {s}");
        }
    }
}
