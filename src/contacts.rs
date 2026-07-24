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

/// ALPN used to exchange transfer-ticket offers between two balloons.
pub const OFFER_ALPN: &[u8] = b"sendme-balloon/offer/1";

/// Wire-format version for the ticket-offer frame.
const OFFER_VERSION: u8 = 1;
/// Ack byte values exchanged after an offer.
const ACK_ACCEPT: u8 = 1;
const ACK_REJECT: u8 = 0;
/// Heartbeat byte sent periodically by the receiver while the remote user is
/// still deciding whether to accept. Any data on the stream resets the QUIC
/// idle timer on both sides, so this keeps the connection alive across what
/// may be a multi-minute human pause.
const HEARTBEAT_BYTE: u8 = 0xFE;
/// How often the receiver sends a heartbeat while waiting for the user.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// A contact: a friendly name and the peer's 256-bit [`EndpointId`].
///
/// The node id is stored as its canonical string form so the address book
/// remains human-readable and survives upgrades of the underlying encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contact {
    pub name: String,
    pub node_id: String,
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
            let bytes = hex::decode(hex.trim())
                .map_err(|e| anyhow::anyhow!("secret key in {} is not valid hex: {e}", path.display()))?;
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
pub async fn create_contact_endpoint(secret: SecretKey) -> anyhow::Result<Endpoint> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![OFFER_ALPN.to_vec()])
        .secret_key(secret)
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;
    Ok(endpoint)
}

/// A ticket offered by a remote peer, handed to the GUI for an accept/decline
/// decision. `respond` carries the user's answer back to the network task.
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
    /// The transfer ticket the receiver should fetch from.
    pub ticket: String,
    /// Channel to report the user's accept (true) / decline (false) decision.
    pub respond: oneshot::Sender<bool>,
}

/// Encode a ticket offer into a single byte buffer for transmission.
///
/// Layout: `[version:u8][ticket_len:u32 BE][ticket][name_len:u32 BE][name][size:u64 BE]`.
pub fn encode_offer(ticket: &str, name: &str, size: u64) -> Vec<u8> {
    let ticket = ticket.as_bytes();
    let name = name.as_bytes();
    let mut buf = Vec::with_capacity(1 + 4 + ticket.len() + 4 + name.len() + 8);
    buf.push(OFFER_VERSION);
    buf.extend(&(ticket.len() as u32).to_be_bytes());
    buf.extend_from_slice(ticket);
    buf.extend(&(name.len() as u32).to_be_bytes());
    buf.extend_from_slice(name);
    buf.extend(&size.to_be_bytes());
    buf
}

/// Decode a ticket offer from a received byte buffer.
pub fn decode_offer(buf: &[u8]) -> anyhow::Result<(String, String, u64)> {
    if buf.len() < 1 + 4 + 8 {
        anyhow::bail!("offer frame too short");
    }
    let mut i = 0;
    let version = buf[i];
    i += 1;
    if version != OFFER_VERSION {
        anyhow::bail!("unsupported offer version {version}");
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
    Ok((ticket, name, size))
}

/// Send a ticket offer to `node_id` and wait for the peer's accept/decline
/// answer. Returns `Ok(true)` on accept, `Ok(false)` on decline.
///
/// The peer sends periodic [`HEARTBEAT_BYTE`]s while its user is still
/// deciding; those are skipped here so we only return on the final verdict.
/// The heartbeats keep the QUIC connection alive across what may be a
/// multi-minute human pause.
pub async fn send_offer(
    endpoint: &Endpoint,
    node_id: EndpointId,
    ticket: String,
    name: String,
    size: u64,
) -> anyhow::Result<bool> {
    let conn = endpoint
        .connect(node_id, OFFER_ALPN)
        .await
        .map_err(|e| anyhow::anyhow!("connecting to peer: {e}"))?;
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow::anyhow!("opening stream: {e}"))?;
    let frame = encode_offer(&ticket, &name, size);
    send.write_all(&frame)
        .await
        .map_err(|e| anyhow::anyhow!("sending offer: {e}"))?;
    send.finish()
        .map_err(|e| anyhow::anyhow!("finishing send stream: {e}"))?;
    loop {
        let mut byte = [0u8; 1];
        recv.read_exact(&mut byte)
            .await
            .map_err(|e| anyhow::anyhow!("reading reply: {e}"))?;
        match byte[0] {
            HEARTBEAT_BYTE => continue,
            ACK_ACCEPT => return Ok(true),
            ACK_REJECT => return Ok(false),
            other => anyhow::bail!("unexpected reply byte: 0x{other:02x}"),
        }
    }
}

/// Run the inbound offer loop for the lifetime of `endpoint`.
///
/// For each incoming connection the offer is decoded and forwarded to the GUI
/// through `offer_tx` as an [`IncomingOffer`] (which carries a oneshot the GUI
/// uses to report the accept/decline decision). The per-connection task then
/// awaits that decision and writes the matching ack byte back to the peer.
pub async fn run_accept_loop(endpoint: Endpoint, offer_tx: mpsc::Sender<IncomingOffer>) {
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
            let (ticket, name, size) = match decode_offer(&buf) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("decoding offer failed: {e}");
                    return;
                }
            };
            let (resp_tx, mut resp_rx) = oneshot::channel();
            let offer = IncomingOffer {
                from: from_full,
                from_short,
                name,
                size,
                ticket,
                respond: resp_tx,
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
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(HEARTBEAT_INTERVAL) => {
                        if send.write_all(&[HEARTBEAT_BYTE]).await.is_err() {
                            break;
                        }
                    }
                    accepted = &mut resp_rx => {
                        let ack = if accepted.unwrap_or(false) { ACK_ACCEPT } else { ACK_REJECT };
                        let _ = send.write_all(&[ack]).await;
                        let _ = send.finish();
                        break;
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
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), conn.closed()).await;
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
        let encoded = encode_offer(ticket, name, size);
        let (t, n, s) = decode_offer(&encoded).unwrap();
        assert_eq!(t, ticket);
        assert_eq!(n, name);
        assert_eq!(s, size);
    }

    #[test]
    fn offer_unicode_name_roundtrip() {
        let ticket = "tkt";
        let name = "Résumé 📄.pdf";
        let size = 1u64;
        let encoded = encode_offer(ticket, name, size);
        let (t, n, s) = decode_offer(&encoded).unwrap();
        assert_eq!(t, ticket);
        assert_eq!(n, name);
        assert_eq!(s, size);
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut encoded = encode_offer("t", "n", 0);
        encoded[0] = 99;
        assert!(decode_offer(&encoded).is_err());
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(decode_offer(&[]).is_err());
        assert!(decode_offer(&[1]).is_err());
    }

    #[test]
    fn address_book_remove() {
        let mut book = AddressBook {
            contacts: vec![
                Contact {
                    name: "alice".into(),
                    node_id: "aaa".into(),
                },
                Contact {
                    name: "bob".into(),
                    node_id: "bbb".into(),
                },
            ],
        };
        assert!(book.remove("bbb"));
        assert!(!book.remove("zzz"));
        assert_eq!(book.contacts.len(), 1);
        assert_eq!(book.contacts[0].name, "alice");
    }
}
