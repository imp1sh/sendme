//! Integration test for the sendme balloon backend.
use std::time::Duration;

use sendme::balloon::{
    parse_ticket, receive_ticket, send_file, ConflictResolver, ReceiveEvent, SendEvent,
};
use sendme::config::Config;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

#[tokio::test]
async fn balloon_send_receive() -> anyhow::Result<()> {
    let src_dir = tempfile::tempdir()?;
    let tgt_dir = tempfile::tempdir()?;
    let file = src_dir.path().join("hello.txt");
    let content = b"hello from the balloon".to_vec();
    std::fs::write(&file, &content)?;

    let (se_tx, mut se_rx) = mpsc::channel(64);
    let (_cancel_tx, cancel_rx) = oneshot::channel();
    let send_task = tokio::spawn(send_file(file, se_tx, cancel_rx, Config::default()));

    // wait for the ticket
    let ticket = loop {
        match tokio::time::timeout(Duration::from_secs(60), se_rx.recv())
            .await?
            .expect("send events ended early")
        {
            SendEvent::TicketReady { ticket, .. } => break ticket,
            SendEvent::Error(e) => panic!("send error: {e}"),
            _ => {}
        }
    };
    // accepts the full command as pasted from the clipboard
    let ticket = parse_ticket(&format!("sendme receive {ticket}"))?;

    let (re_tx, mut re_rx) = mpsc::channel(64);
    let (_decide_tx, decide_rx) = oneshot::channel();
    let export_lock = Arc::new(Mutex::new(()));
    // Create a shared receive endpoint (mirrors what the worker does).
    let recv_endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .alpns(vec![])
        .secret_key(iroh::SecretKey::generate())
        .relay_mode(iroh::RelayMode::Default)
        .address_lookup(iroh::address_lookup::dns::DnsAddressLookup::n0_dns())
        .bind()
        .await?;
    tokio::spawn(receive_ticket(
        recv_endpoint,
        ticket,
        tgt_dir.path().to_path_buf(),
        re_tx,
        ConflictResolver::Prompt(decide_rx),
        export_lock,
        Config::default(),
    ));
    // wait for the receiver to finish
    loop {
        match tokio::time::timeout(Duration::from_secs(60), re_rx.recv()).await? {
            Some(ReceiveEvent::Completed { .. }) => break,
            Some(ReceiveEvent::Error(e)) => panic!("receive error: {e}"),
            Some(_) => {}
            None => panic!("receive events ended early"),
        }
    }
    assert_eq!(std::fs::read(tgt_dir.path().join("hello.txt"))?, content);

    // the sender must notice the completed transfer and terminate
    loop {
        match tokio::time::timeout(Duration::from_secs(30), se_rx.recv()).await? {
            Some(SendEvent::Completed) => break,
            Some(SendEvent::Error(e)) => panic!("send error: {e}"),
            Some(_) => {}
            None => panic!("send events ended early"),
        }
    }
    tokio::time::timeout(Duration::from_secs(10), send_task).await??;
    Ok(())
}
