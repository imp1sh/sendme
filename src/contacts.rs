//! Address book and ticket-offer exchange for the `sendme-balloon` desktop app.
//!
//! The address book maps human-readable names to iroh [`EndpointId`]s (the
//! 256-bit public keys that uniquely identify a peer). It is persisted as JSON
//! in a per-user config directory.
//!
//! A persistent *contact endpoint* is kept alive for the whole lifetime of the
//! app. It has a stable [`EndpointId`] (derived from a persisted secret key) so
//! that other users can add it to their address books and reach it by node id
//! alone. The endpoint publishes its address via n0's pkarr/DNS service so that
//! a bare [`EndpointId`] is enough to establish a connection.
//!
//! On top of this endpoint a tiny "ticket-offer" protocol runs: when you send a
//! file you can pick a contact and the balloon pushes the transfer ticket to
//! that contact over a dedicated ALPN. The receiver is prompted to accept or
//! decline; on accept the receiver fetches the data using the usual
//! [`crate::balloon::receive_ticket`] machinery. This removes the need to
//! manually copy/paste tickets between two machines that know each other.

use std::path::PathBuf;

use anyhow::Context;
use iroh::{endpoint::presets, Endpoint, EndpointId, RelayMode, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::balloon::FileKind;

/// ALPN used to exchange transfer-ticket offers between two balloons.
pub const OFFER_ALPN: &[u8] = b"sendme-balloon/offer/1";

/// Wire-format version for the ticket-offer frame.
///
/// Version 1 carried only `ticket`, `name` and `size`. Version 2 adds the
/// file `kind` (file/directory) and a `mime` type string so the receiver can
/// show what kind of data is being offered before accepting. Version 1 frames
/// are still accepted by [`decode_offer`] for backward compatibility.
const OFFER_VERSION: u8 = 2;
/// Ack byte values exchanged after an offer.
const ACK_ACCEPT: u8 = 1;
const ACK_REJECT: u8 = 0;
/// Result byte values sent by the receiver after the transfer completes.
/// Only sent following an ``ACK_ACCEPT``; the stream stays open (with
/// heartbeats) until the receiver knows the outcome.
const RESULT_SAVED: u8 = 3;
const RESULT_KEPT_EXISTING: u8 = 4;
const RESULT_ERROR: u8 = 5;
/// Heartbeat byte sent periodically by the receiver while the remote user is
/// still deciding whether to accept, or while a transfer is in progress. Any
/// data on the stream resets the QUIC idle timer on both sides, so this keeps
/// the connection alive across what may be a multi-minute human pause.
const HEARTBEAT_BYTE: u8 = 0xFE;
// The heartbeat cadence is configurable via
// `Config::heartbeat_interval_secs`; the built-in default is 3 seconds.

/// The outcome of an outgoing offer, reported by the receiver after the
/// transfer attempt finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferResult {
    /// The contact declined the transfer.
    Declined,
    /// The contact accepted and the file(s) were saved.
    Saved,
    /// The contact accepted but kept existing file(s); nothing was overwritten.
    KeptExisting,
}

/// The outcome of a receive, sent back to the offering peer via the contact
/// endpoint so the sender knows whether its file was saved or rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferResult {
    /// The file(s) were saved to disk.
    Saved,
    /// The user chose to keep existing file(s); nothing was overwritten.
    KeptExisting,
}

/// A contact: a friendly name and the peer's 256-bit [`EndpointId`].
///
/// The node id is stored as its canonical string form so the address book
/// remains human-readable and survives upgrades of the underlying encoding.
///
/// The `email` and `auto_accept` fields were added later and carry
/// `#[serde(default)]`, so older address book JSON files (written before these
/// fields existed) keep loading: missing fields become `""` and `false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub node_id: String,
    /// Optional email address, as a cross-reference to a real person behind
    /// the node id. Free-text, not used for any network behaviour.
    #[serde(default)]
    pub email: String,
    /// When `true`, transfer offers from this contact are accepted
    /// automatically (no Accept/Decline prompt). Requires a default save
    /// folder to be configured; otherwise this flag is ignored. Overrides the
    /// global `auto_accept_offers` setting for this contact.
    #[serde(default)]
    pub auto_accept: bool,
}

impl Contact {
    /// Parse the stored node id into an [`EndpointId`].
    pub fn endpoint_id(&self) -> anyhow::Result<EndpointId> {
        self.node_id
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid node id: {e}"))
    }
}

/// The address book, persisted as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddressBook {
    pub contacts: Vec<Contact>,
}

impl AddressBook {
    /// Load the address book from disk, returning an empty one if absent.
    pub fn load() -> anyhow::Result<Self> {
        let path = address_book_path();
        match std::fs::read(&path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }

    /// Persist the address book to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = address_book_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("serializing address book")?;
        std::fs::write(&path, bytes)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
        Ok(())
    }

    /// Remove the contact with the given node id, returning whether one was
    /// removed.
    pub fn remove(&mut self, node_id: &str) -> bool {
        let before = self.contacts.len();
        self.contacts.retain(|c| c.node_id != node_id);
        self.contacts.len() != before
    }

    /// Find a contact whose node id matches the given canonical node id
    /// string (as produced by [`EndpointId::to_string`]).
    ///
    /// Both sides are parsed to [`EndpointId`] before comparing, so the
    /// lookup is robust to minor formatting differences in the stored text.
    /// Returns `None` if `node_id` is not a valid [`EndpointId`] or if no
    /// contact matches.
    pub fn find_by_node_id(&self, node_id: &str) -> Option<&Contact> {
        let target = match node_id.parse::<EndpointId>() {
            Ok(t) => t,
            Err(_) => return None,
        };
        self.contacts.iter().find(|c| {
            c.node_id
                .parse::<EndpointId>()
                .is_ok_and(|cid| cid == target)
        })
    }
}

/// Per-application directory used for persistent state (secret key, address
/// book). Falls back to a directory next to the current executable if no
/// platform config directory is known.
fn app_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    });
    base.join("sendme-balloon")
}

fn secret_path() -> PathBuf {
    app_dir().join("secret.key")
}

fn address_book_path() -> PathBuf {
    app_dir().join("addressbook.json")
}

/// Load the persisted secret key, generating and storing a fresh one on first
/// run. A stable secret key means a stable [`EndpointId`] that others can add
/// to their address books.
pub fn load_or_create_secret() -> anyhow::Result<SecretKey> {
    let path = secret_path();
    match std::fs::read_to_string(&path) {
        Ok(hex) => {
            let bytes = hex::decode(hex.trim()).map_err(|e| {
                anyhow::anyhow!("secret key in {} is not valid hex: {e}", path.display())
            })?;
            let arr: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("secret key in {} is not 32 bytes", path.display()))?;
            Ok(SecretKey::from_bytes(&arr))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("creating {}: {e}", parent.display()))?;
            }
            std::fs::write(&path, hex::encode(key.to_bytes()))
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
            Ok(key)
        }
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// Build the persistent contact endpoint.
///
/// It advertises a single ALPN ([`OFFER_ALPN`]). The [`presets::N0`] preset
/// already equips the endpoint with both a pkarr publisher (so peers can find
/// us by [`EndpointId`] alone) and a DNS address lookup (so we can dial peers
/// by their [`EndpointId`]); together with [`RelayMode::Default`] this gives
/// working node-id-only connectivity.
///
/// `relay_mode` is taken from the user's configuration so the contact
/// endpoint honours the same relay setting as the transfer endpoints.
pub async fn create_contact_endpoint(
    secret: SecretKey,
    relay_mode: RelayMode,
) -> anyhow::Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![OFFER_ALPN.to_vec()])
        .secret_key(secret)
        .relay_mode(relay_mode)
        .bind()
        .await?;
    Ok(endpoint)
}

/// A ticket offered by a remote peer, handed to the GUI for an accept/decline
/// decision. `respond` carries the user's answer back to the network task.
/// `result_tx` is used (only when accepted) to report the transfer outcome
/// back to the sender over the same offer stream.
#[derive(Debug)]
pub struct IncomingOffer {
    /// Canonical string form of the sender's [`EndpointId`].
    pub from: String,
    /// Short form (first bytes) of the sender, for a compact label.
    pub from_short: String,
    /// Name of the offered file or directory.
    pub name: String,
    /// Total payload size in bytes.
    pub size: u64,
    /// Whether the offered path is a single file or a directory.
    pub kind: FileKind,
    /// Guessed MIME type (empty for directories or when unknown).
    pub mime: String,
    /// The transfer ticket the receiver should fetch from.
    pub ticket: String,
    /// Channel to report the user's accept (true) / decline (false) decision.
    pub respond: oneshot::Sender<bool>,
    /// Channel to report the transfer outcome (only used when accepted).
    /// Dropped without sending if the user declines.
    pub result_tx: oneshot::Sender<TransferResult>,
}

/// A decoded ticket offer.
///
/// Returned by [`decode_offer`]; the fields are spread into [`IncomingOffer`]
/// by the accept loop.
pub struct DecodedOffer {
    pub ticket: String,
    pub name: String,
    pub size: u64,
    pub kind: FileKind,
    pub mime: String,
}

/// Encode a ticket offer into a single byte buffer for transmission.
///
/// Layout (version 2):
/// `[version:u8][ticket_len:u32 BE][ticket][name_len:u32 BE][name][size:u64 BE][kind:u8][mime_len:u32 BE][mime]`
pub fn encode_offer(ticket: &str, name: &str, size: u64, kind: FileKind, mime: &str) -> Vec<u8> {
    let ticket = ticket.as_bytes();
    let name = name.as_bytes();
    let mime = mime.as_bytes();
    let mut buf =
        Vec::with_capacity(1 + 4 + ticket.len() + 4 + name.len() + 8 + 1 + 4 + mime.len());
    buf.push(OFFER_VERSION);
    buf.extend(&(ticket.len() as u32).to_be_bytes());
    buf.extend_from_slice(ticket);
    buf.extend(&(name.len() as u32).to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend(&size.to_be_bytes());
    buf.push(kind.to_byte());
    buf.extend(&(mime.len() as u32).to_be_bytes());
    buf.extend_from_slice(mime);
    buf
}

/// Decode a ticket offer from a received byte buffer.
///
/// Accepts both version 1 frames (no kind/mime fields) and version 2 frames.
/// Version 1 frames yield [`FileKind::File`] and an empty MIME string.
pub fn decode_offer(buf: &[u8]) -> anyhow::Result<DecodedOffer> {
    if buf.is_empty() {
        anyhow::bail!("offer frame too short");
    }
    let mut i = 0;
    let version = buf[i];
    i += 1;
    if i + 4 > buf.len() {
        anyhow::bail!("offer frame truncated (header)");
    }
    let ticket_len = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
    i += 4;
    if i + ticket_len + 4 > buf.len() {
        anyhow::bail!("offer frame truncated (ticket)");
    }
    let ticket = std::str::from_utf8(&buf[i..i + ticket_len])
        .context("ticket is not valid utf-8")?
        .to_string();
    i += ticket_len;
    let name_len = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
    i += 4;
    if i + name_len + 8 > buf.len() {
        anyhow::bail!("offer frame truncated (name)");
    }
    let name = std::str::from_utf8(&buf[i..i + name_len])
        .context("name is not valid utf-8")?
        .to_string();
    i += name_len;
    let size = u64::from_be_bytes(buf[i..i + 8].try_into().unwrap());
    i += 8;
    let (kind, mime) = match version {
        OFFER_VERSION => {
            // version 2: kind byte + mime string
            if i + 1 + 4 > buf.len() {
                anyhow::bail!("offer frame truncated (kind/mime header)");
            }
            let kind = FileKind::from_byte(buf[i])?;
            i += 1;
            let mime_len = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            if i + mime_len > buf.len() {
                anyhow::bail!("offer frame truncated (mime)");
            }
            let mime = std::str::from_utf8(&buf[i..i + mime_len])
                .context("mime is not valid utf-8")?
                .to_string();
            (kind, mime)
        }
        1 => {
            // legacy version 1: no kind/mime fields
            (FileKind::File, String::new())
        }
        other => anyhow::bail!("unsupported offer version {other}"),
    };
    Ok(DecodedOffer {
        ticket,
        name,
        size,
        kind,
        mime,
    })
}

/// Send a ticket offer to `node_id` and wait for the peer's accept/decline
/// answer, then (if accepted) for the transfer result.
///
/// Returns [`OfferResult::Declined`] if the peer declines, or
/// [`OfferResult::Saved`] / [`OfferResult::KeptExisting`] once the receiver
/// reports the outcome of the transfer.
///
/// The peer sends periodic [`HEARTBEAT_BYTE`]s while its user is still
/// deciding and while the transfer is in progress; those are skipped here so
/// we only return on the final verdict and result.
pub async fn send_offer(
    endpoint: &Endpoint,
    node_id: EndpointId,
    ticket: String,
    name: String,
    size: u64,
    kind: FileKind,
    mime: String,
) -> anyhow::Result<OfferResult> {
    let conn = endpoint
        .connect(node_id, OFFER_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to peer: {e}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow::anyhow!("opening stream: {e}"))?;
    let frame = encode_offer(&ticket, &name, size, kind, &mime);
    send.write_all(&frame)
        .await
        .map_err(|e| anyhow::anyhow!("sending offer: {e}"))?;
    send.finish()
        .map_err(|e| anyhow::anyhow!("finishing send stream: {e}"))?;
    // Phase 1 — wait for the accept/decline ack (skipping heartbeats).
    loop {
        let mut byte = [0u8; 1];
        recv.read_exact(&mut byte)
            .await
            .map_err(|e| anyhow::anyhow!("reading reply: {e}"))?;
        match byte[0] {
            HEARTBEAT_BYTE => continue,
            ACK_ACCEPT => break, // accepted — proceed to phase 2
            ACK_REJECT => return Ok(OfferResult::Declined),
            other => anyhow::bail!("unexpected reply byte: 0x{other:02x}"),
        }
    }
    // Phase 2 — wait for the transfer result (skipping heartbeats).
    loop {
        let mut byte = [0u8; 1];
        recv.read_exact(&mut byte)
            .await
            .map_err(|e| anyhow::anyhow!("reading transfer result: {e}"))?;
        match byte[0] {
            HEARTBEAT_BYTE => continue,
            RESULT_SAVED => return Ok(OfferResult::Saved),
            RESULT_KEPT_EXISTING => return Ok(OfferResult::KeptExisting),
            RESULT_ERROR => anyhow::bail!("transfer failed on the receiver side"),
            other => anyhow::bail!("unexpected result byte: 0x{other:02x}"),
        }
    }
}

/// Run the inbound offer loop for the lifetime of `endpoint`.
///
/// For each incoming connection the offer is decoded and forwarded to the GUI
/// through `offer_tx` as an [`IncomingOffer`] (which carries a oneshot the GUI
/// uses to report the accept/decline decision). The per-connection task then
/// awaits that decision and writes the matching ack byte back to the peer.
///
/// `heartbeat_interval` replaces the historic fixed [`HEARTBEAT_INTERVAL`] so
/// the cadence is configurable; `conn_close_wait` bounds how long the task
/// waits for the sender to acknowledge the reply before dropping the
/// connection.
pub async fn run_accept_loop(
    endpoint: Endpoint,
    offer_tx: mpsc::Sender<IncomingOffer>,
    heartbeat_interval: std::time::Duration,
    conn_close_wait: std::time::Duration,
) {
    loop {
        let incoming = match endpoint.accept().await {
            Some(incoming) => incoming,
            None => break, // endpoint closed
        };
        let accepting = match incoming.accept() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("incoming connection failed: {e}");
                continue;
            }
        };
        let endpoint = endpoint.clone();
        let offer_tx = offer_tx.clone();
        tokio::spawn(async move {
            let conn = match accepting.await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("accepting connection failed: {e}");
                    return;
                }
            };
            let from = conn.remote_id();
            let from_short = from.fmt_short().to_string();
            let from_full = from.to_string();
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("accepting stream failed: {e}");
                    return;
                }
            };
            let buf = match recv.read_to_end(64 * 1024).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("reading offer failed: {e}");
                    return;
                }
            };
            let decoded = match decode_offer(&buf) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("decoding offer failed: {e}");
                    return;
                }
            };
            let (resp_tx, mut resp_rx) = oneshot::channel();
            let (result_tx, mut result_rx) = oneshot::channel();
            let offer = IncomingOffer {
                from: from_full,
                from_short,
                name: decoded.name,
                size: decoded.size,
                kind: decoded.kind,
                mime: decoded.mime,
                ticket: decoded.ticket,
                respond: resp_tx,
                result_tx,
            };
            if offer_tx.send(offer).await.is_err() {
                // GUI gone: decline so the sender does not hang.
                let _ = send.write_all(&[ACK_REJECT]).await;
                let _ = send.finish();
                return;
            }
            // Send heartbeats while the remote user decides. The peer skips
            // these bytes and waits for the final accept/decline. Periodic
            // data on the stream resets the QUIC idle timer on both sides,
            // keeping the connection alive across a multi-minute human pause.
            let accepted = loop {
                tokio::select! {
                    _ = tokio::time::sleep(heartbeat_interval) => {
                        if send.write_all(&[HEARTBEAT_BYTE]).await.is_err() {
                            break false;
                        }
                    }
                    accepted = &mut resp_rx => {
                        let is_accepted = accepted.unwrap_or(false);
                        let ack = if is_accepted { ACK_ACCEPT } else { ACK_REJECT };
                        let _ = send.write_all(&[ack]).await;
                        if !is_accepted {
                            let _ = send.finish();
                        }
                        break is_accepted;
                    }
                }
            };
            // Phase 2 — if accepted, keep the stream open with heartbeats
            // until the transfer result is known, then send the result byte.
            if accepted {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(heartbeat_interval) => {
                            if send.write_all(&[HEARTBEAT_BYTE]).await.is_err() {
                                break;
                            }
                        }
                        result = &mut result_rx => {
                            let result_byte = match result {
                                Ok(TransferResult::Saved) => RESULT_SAVED,
                                Ok(TransferResult::KeptExisting) => RESULT_KEPT_EXISTING,
                                Err(_) => RESULT_ERROR,
                            };
                            let _ = send.write_all(&[result_byte]).await;
                            let _ = send.finish();
                            break;
                        }
                    }
                }
            }
            // Wait for the sender to close its end of the connection.
            //
            // The endpoint does NOT hold a strong Arc<ConnectionRef> — it only
            // stores channel senders.  So dropping `conn` here would be the
            // last reference, triggering ConnectionRef::drop → implicit_close
            // → CONNECTION_CLOSE, which races ahead of (or discards) the
            // just-written ack byte before it reaches the sender.
            //
            // The sender closes the connection only AFTER send_offer() reads
            // the ack and returns, so conn.closed() resolving is proof the ack
            // was received.  The timeout is a safety net for pathological cases.
            let _ = tokio::time::timeout(conn_close_wait, conn.closed()).await;
            // keep the endpoint alive for the lifetime of this task
            drop(endpoint);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_roundtrip() {
        let ticket = "abc123def456";
        let name = "photos/cat.png";
        let size = 4096u64;
        let encoded = encode_offer(ticket, name, size, FileKind::File, "image/png");
        let d = decode_offer(&encoded).unwrap();
        assert_eq!(d.ticket, ticket);
        assert_eq!(d.name, name);
        assert_eq!(d.size, size);
        assert_eq!(d.kind, FileKind::File);
        assert_eq!(d.mime, "image/png");
    }

    #[test]
    fn offer_directory_roundtrip() {
        let encoded = encode_offer("t", "photos", 1024, FileKind::Directory, "");
        let d = decode_offer(&encoded).unwrap();
        assert_eq!(d.kind, FileKind::Directory);
        assert_eq!(d.mime, "");
        assert_eq!(d.name, "photos");
    }

    #[test]
    fn offer_unicode_name_roundtrip() {
        let ticket = "tkt";
        let name = "Résumé 📄.pdf";
        let size = 1u64;
        let encoded = encode_offer(ticket, name, size, FileKind::File, "application/pdf");
        let d = decode_offer(&encoded).unwrap();
        assert_eq!(d.ticket, ticket);
        assert_eq!(d.name, name);
        assert_eq!(d.size, size);
        assert_eq!(d.mime, "application/pdf");
    }

    #[test]
    fn decode_accepts_legacy_v1_frame() {
        // Hand-build a version 1 frame (no kind/mime fields) and ensure it
        // decodes with the documented defaults.
        let ticket = "abc";
        let name = "report.pdf";
        let size = 7u64;
        let tb = ticket.as_bytes();
        let nb = name.as_bytes();
        let mut buf = Vec::with_capacity(1 + 4 + tb.len() + 4 + nb.len() + 8);
        buf.push(1u8); // legacy version
        buf.extend(&(tb.len() as u32).to_be_bytes());
        buf.extend_from_slice(tb);
        buf.extend(&(nb.len() as u32).to_be_bytes());
        buf.extend_from_slice(nb);
        buf.extend(&size.to_be_bytes());
        let d = decode_offer(&buf).unwrap();
        assert_eq!(d.ticket, ticket);
        assert_eq!(d.name, name);
        assert_eq!(d.size, size);
        assert_eq!(d.kind, FileKind::File);
        assert_eq!(d.mime, "");
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut encoded = encode_offer("t", "n", 0, FileKind::File, "");
        encoded[0] = 99;
        assert!(decode_offer(&encoded).is_err());
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(decode_offer(&[]).is_err());
        assert!(decode_offer(&[2]).is_err());
    }

    #[test]
    fn address_book_remove() {
        let mut book = AddressBook {
            contacts: vec![
                Contact {
                    name: "alice".into(),
                    node_id: "aaa".into(),
                    email: String::new(),
                    auto_accept: false,
                },
                Contact {
                    name: "bob".into(),
                    node_id: "bbb".into(),
                    email: String::new(),
                    auto_accept: false,
                },
            ],
        };
        assert!(book.remove("bbb"));
        assert!(!book.remove("zzz"));
        assert_eq!(book.contacts.len(), 1);
        assert_eq!(book.contacts[0].name, "alice");
    }

    #[test]
    fn find_by_node_id_matches_known_contact() {
        // generate a real key pair so the node id is a valid EndpointId
        let key = SecretKey::generate();
        let id = key.public();
        let id_str = id.to_string();
        let book = AddressBook {
            contacts: vec![
                Contact {
                    name: "alice".into(),
                    node_id: id_str.clone(),
                    email: String::new(),
                    auto_accept: false,
                },
                Contact {
                    name: "bob".into(),
                    node_id: "bbb".into(),
                    email: String::new(),
                    auto_accept: false,
                },
            ],
        };
        // lookup by the canonical string form succeeds and resolves the name
        assert_eq!(book.find_by_node_id(&id_str).unwrap().name, "alice");
        // a bogus node id does not match anything and does not panic
        assert!(book.find_by_node_id("not-a-real-node-id").is_none());
        // bob's stored node id is not a valid EndpointId, so it can't match
        assert!(book.find_by_node_id("bbb").is_none());
    }

    #[test]
    fn legacy_address_book_loads_with_defaults() {
        // An address book written before the email/auto_accept fields existed.
        // It must still load, with the new fields defaulting to empty/false.
        // The on-disk shape is {"contacts": [...]}, as written by AddressBook::save.
        let json = r#"{"contacts":[{"name":"alice","node_id":"aaa"}]}"#;
        let book: AddressBook = serde_json::from_str(json).unwrap();
        assert_eq!(book.contacts.len(), 1);
        assert_eq!(book.contacts[0].name, "alice");
        assert_eq!(book.contacts[0].node_id, "aaa");
        assert_eq!(book.contacts[0].email, "");
        assert!(!book.contacts[0].auto_accept);
    }
}
