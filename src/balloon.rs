//! Backend for the `sendme-balloon` desktop app.
//!
//! This module reuses the core send/receive machinery of sendme, but instead
//! of printing to a terminal it reports progress through channels, so a GUI
//! can display it. It also supports cancellation.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use data_encoding::HEXLOWER;
use indicatif::{MultiProgress, ProgressDrawTarget};
use iroh::{address_lookup::dns::DnsAddressLookup, endpoint::presets, Endpoint, RelayMode};
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
    sync::{mpsc, oneshot},
};

use crate::{apply_options, export, get_or_create_secret, import, AddrInfoOptions};

/// Events emitted while providing (sending) data.
#[derive(Debug, Clone)]
pub enum SendEvent {
    /// The file was imported, the ticket is ready and we are waiting for a peer.
    TicketReady {
        ticket: String,
        name: String,
        size: u64,
    },
    /// A peer connected to us.
    PeerConnected,
    /// Bytes sent so far for the current outgoing request.
    Progress { sent: u64 },
    /// The data was completely transferred to a peer.
    Completed,
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
    /// Everything was saved to the target directory.
    Completed { target: PathBuf },
    /// Something went wrong.
    Error(String),
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
pub async fn send_file(
    path: PathBuf,
    events: mpsc::Sender<SendEvent>,
    cancel: oneshot::Receiver<()>,
) {
    // use a temp dir for the blob store, it is removed again afterwards
    let suffix = rand::rng().random::<[u8; 16]>();
    let blobs_data_dir =
        std::env::temp_dir().join(format!("sendme-balloon-send-{}", HEXLOWER.encode(&suffix)));
    let res = send_file_inner(path, blobs_data_dir.clone(), events.clone(), cancel).await;
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
) -> anyhow::Result<()> {
    let secret_key = get_or_create_secret(false)?;
    let relay_mode = RelayMode::Default;
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
    let (temp_tag, size, _collection) = import(path, blobs.store(), &mut mp, None).await?;
    let hash = temp_tag.hash();

    let router = iroh::protocol::Router::builder(endpoint)
        .accept(iroh_blobs::ALPN, blobs.clone())
        .spawn();

    // wait for the endpoint to figure out its address before making a ticket
    let ep = router.endpoint();
    tokio::time::timeout(Duration::from_secs(30), async {
        if !matches!(relay_mode, RelayMode::Disabled) {
            let _ = ep.online().await;
        }
    })
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
    tokio::time::timeout(Duration::from_secs(2), router.shutdown())
        .await
        .ok();
    drop(router);
    store.shutdown().await.ok();
    Ok(())
}

/// Receive the data behind `ticket` and save it below `target_dir`.
pub async fn receive_ticket(
    ticket: BlobTicket,
    target_dir: PathBuf,
    events: mpsc::Sender<ReceiveEvent>,
) {
    match receive_ticket_inner(&ticket, &target_dir, events.clone()).await {
        Ok(()) => {
            events
                .send(ReceiveEvent::Completed { target: target_dir })
                .await
                .ok();
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
    ticket: &BlobTicket,
    target_dir: &Path,
    events: mpsc::Sender<ReceiveEvent>,
) -> anyhow::Result<()> {
    let addr = ticket.addr().clone();
    let secret_key = get_or_create_secret(false)?;
    let mut builder = Endpoint::builder(presets::N0)
        .alpns(vec![])
        .secret_key(secret_key)
        .relay_mode(RelayMode::Default);
    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.address_lookup(DnsAddressLookup::n0_dns());
    }
    let endpoint = builder.bind().await?;
    // temp dir for the blob store; keyed by hash so interrupted downloads resume
    let iroh_data_dir =
        std::env::temp_dir().join(format!("sendme-balloon-recv-{}", ticket.hash().to_hex()));
    let db = FsStore::load(&iroh_data_dir).await?;
    let res = async {
        let hash_and_format = ticket.hash_and_format();
        let local = db.remote().local(hash_and_format).await?;
        if !local.is_complete() {
            events.send(ReceiveEvent::Connecting).await.ok();
            let connection = endpoint.connect(addr, iroh_blobs::protocol::ALPN).await?;
            let (_hash_seq, sizes) =
                get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32, None)
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
        events.send(ReceiveEvent::Exporting).await.ok();
        let mut mp = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        export(&db, collection, target_dir, &mut mp).await?;
        anyhow::Ok(())
    }
    .await;
    endpoint.close().await;
    db.shutdown().await.ok();
    if res.is_ok() {
        tokio::fs::remove_dir_all(&iroh_data_dir).await.ok();
    }
    res
}

// ── Autostart (desktop integration) ───────────────────────────────────────

/// Path to the XDG autostart `.desktop` file (`~/.config/autostart/`).
fn autostart_path() -> PathBuf {
    let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    config.join("autostart").join("sendme-balloon.desktop")
}

/// The `.desktop` file content for autostart.
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

/// Check whether autostart is currently enabled.
pub fn autostart_is_enabled() -> bool {
    autostart_path().exists()
}

/// Enable autostart by writing the `.desktop` file to the XDG autostart
/// directory.  Returns the path on success.
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

/// Disable autostart by removing the `.desktop` file.
pub fn disable_autostart() -> anyhow::Result<()> {
    let path = autostart_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| anyhow::anyhow!("removing {}: {e}", path.display()))?;
    }
    Ok(())
}
