use std::sync::Arc;
use tokio::io::{AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::client::interactive::{handle_key, EscapeState, KeyAction};

/// Run the server's interactive console.
///
/// Displays serial data and forwards keyboard input to the serial port.
/// Supports the same escape sequences as the client (Ctrl+], Ctrl+T Q).
pub async fn run_console(
    mut broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let result = run_console_inner(&mut broadcast_rx, &serial_tx, &cancel).await;
    crossterm::terminal::disable_raw_mode()?;
    println!();

    if result.is_ok() {
        cancel.cancel();
    }
    result
}

async fn run_console_inner(
    broadcast_rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    serial_tx: &mpsc::Sender<Vec<u8>>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    use crossterm::event::{Event, EventStream};
    use futures::StreamExt;

    let mut event_stream = EventStream::new();
    let mut escape_state = EscapeState::Normal;
    let mut stdout = tokio::io::stdout();

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("console lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            event = event_stream.next() => {
                match event {
                    Some(Ok(Event::Key(key_event))) => {
                        match handle_key(key_event, &mut escape_state) {
                            KeyAction::Quit => break,
                            KeyAction::SendByte(b) => {
                                let _ = serial_tx.send(vec![b]).await;
                            }
                            KeyAction::SendBytes(bytes) => {
                                let _ = serial_tx.send(bytes).await;
                            }
                            KeyAction::None => {}
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        eprintln!("\r\nConsole input error: {e}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
