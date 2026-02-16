use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::client::interactive::{run_interactive, LineTimestamper};

/// Run the server's interactive console.
///
/// Bridges broadcast serial data into an mpsc channel and delegates to
/// `run_interactive` for terminal I/O. When the user quits (Ctrl+]),
/// cancels the server.
pub async fn run_console(
    broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    timestamps: bool,
) -> anyhow::Result<()> {
    let (incoming_tx, incoming_rx) = mpsc::channel::<Vec<u8>>(256);

    let bridge_cancel = cancel.clone();
    let bridge_handle = tokio::spawn(bridge_broadcast_to_mpsc(
        broadcast_rx,
        incoming_tx,
        bridge_cancel,
    ));

    let result = run_interactive(incoming_rx, serial_tx, timestamps).await;

    bridge_handle.abort();

    if result.is_ok() {
        cancel.cancel();
    }

    result
}

/// Run a non-interactive monitor that outputs serial data to stdout.
///
/// Unlike the interactive console, this does not use raw terminal mode
/// or handle keyboard input. Useful for piping or logging server output.
pub async fn run_monitor(
    mut broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    cancel: CancellationToken,
    timestamps: bool,
) -> anyhow::Result<()> {
    let mut stdout = tokio::io::stdout();
    let mut timestamper = LineTimestamper::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        if timestamps {
                            let stamped = timestamper.process(&data);
                            stdout.write_all(&stamped).await?;
                        } else {
                            stdout.write_all(&data).await?;
                        }
                        stdout.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("monitor lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

/// Bridge from a broadcast channel to an mpsc channel.
///
/// Runs until the broadcast sender is dropped, the cancel token fires,
/// or the mpsc receiver is dropped.
async fn bridge_broadcast_to_mpsc(
    mut broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        if tx.send((*data).clone()).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("console lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
