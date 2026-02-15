use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::{
    encode_data, encode_response, ControlMessage, ErrorResponse, MessageKind, ParseEvent, Parser,
    ResponseMessage, ServerInfoResponse,
};

/// Shared server state that client handlers can query.
pub struct ClientContext {
    pub port: String,
    pub baudrate: u32,
    pub serial_connected: bool,
    pub client_count: Arc<std::sync::atomic::AtomicUsize>,
}

/// Handle a single client connection over a Unix socket.
///
/// This spawns two concurrent tasks:
/// 1. Reader: serial broadcast → protocol-encode → client socket
/// 2. Writer: client socket → parse protocol → handle commands / forward data
pub async fn handle_client(
    stream: UnixStream,
    broadcast_tx: broadcast::Sender<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    ctx: Arc<ClientContext>,
    cancel: CancellationToken,
) {
    let client_cancel = cancel.child_token();
    let count = ctx.client_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::info!("client connected (total: {count})");

    let (mut read_half, mut write_half) = stream.into_split();

    // Channel for control responses that need to be sent back to this client
    let (response_tx, mut response_rx) = mpsc::channel::<Vec<u8>>(64);

    // Task 1: broadcast serial data and control responses to this client
    let mut broadcast_rx = broadcast_tx.subscribe();
    let reader_cancel = client_cancel.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = reader_cancel.cancelled() => break,
                result = broadcast_rx.recv() => {
                    match result {
                        Ok(data) => {
                            let encoded = encode_data(&data);
                            if let Err(e) = write_half.write_all(&encoded).await {
                                tracing::debug!("client write error: {e}");
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("client lagged, dropped {n} messages");
                            // Disconnect lagged clients
                            break;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                Some(response_bytes) = response_rx.recv() => {
                    if let Err(e) = write_half.write_all(&response_bytes).await {
                        tracing::debug!("client response write error: {e}");
                        break;
                    }
                }
            }
        }
    });

    // Task 2: read from client, parse protocol, handle commands
    let writer_cancel = client_cancel.clone();
    let writer_ctx = ctx.clone();
    let writer_task = tokio::spawn(async move {
        let ctx = writer_ctx;
        let mut parser = Parser::new();
        let mut buf = [0u8; 4096];

        loop {
            tokio::select! {
                () = writer_cancel.cancelled() => break,
                result = read_half.read(&mut buf) => {
                    match result {
                        Ok(0) => break, // Client disconnected
                        Ok(n) => {
                            let events = parser.feed_bytes(&buf[..n]);
                            for event in events {
                                match event {
                                    ParseEvent::DataByte(b) => {
                                        let _ = serial_tx.send(vec![b]).await;
                                    }
                                    ParseEvent::MalformedEscape(b) => {
                                        // Forward the NUL and the byte as raw data
                                        let _ = serial_tx.send(vec![0x00, b]).await;
                                    }
                                    ParseEvent::Message { kind: MessageKind::Control, json } => {
                                        let cmd = ControlMessage::from_json(&json);
                                        if let Some(response_bytes) = handle_control_command(&cmd, &ctx) {
                                            let _ = response_tx.send(response_bytes).await;
                                        }
                                    }
                                    ParseEvent::Message { kind: MessageKind::Response, .. }
                                    | ParseEvent::Pending => {}
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("client read error: {e}");
                            break;
                        }
                    }
                }
            }
        }
    });

    // Wait for either task to finish, then cancel the other
    tokio::select! {
        _ = reader_task => {}
        _ = writer_task => {}
    }
    client_cancel.cancel();

    let remaining = ctx.client_count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) - 1;
    tracing::info!("client disconnected (remaining: {remaining})");
}

/// Handle a parsed control command from a client.
/// Returns encoded response bytes to send back, or None.
fn handle_control_command(
    cmd: &ControlMessage,
    ctx: &ClientContext,
) -> Option<Vec<u8>> {
    match cmd {
        ControlMessage::Status => {
            let resp = ResponseMessage::ServerInfo(ServerInfoResponse {
                serial: ctx.serial_connected,
                port: ctx.port.clone(),
                baudrate: ctx.baudrate,
                clients: u32::try_from(
                    ctx.client_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                )
                .unwrap_or(u32::MAX),
            });
            tracing::debug!("status requested");
            Some(encode_response(&resp))
        }
        ControlMessage::History { lines, timestamps } => {
            tracing::debug!("history requested: {lines} lines, timestamps={timestamps}");
            // History would be served from the log file
            None
        }
        ControlMessage::Disconnect => {
            tracing::debug!("client requested disconnect");
            None
        }
        ControlMessage::Unknown(s) => {
            tracing::warn!("unknown command: {s}");
            let resp = ResponseMessage::Error(ErrorResponse {
                error: "unknown command".into(),
            });
            Some(encode_response(&resp))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_dispatch_status() {
        let cmd = ControlMessage::from_json(r#"{"cmd":"status"}"#);
        assert_eq!(cmd, ControlMessage::Status);
    }

    #[test]
    fn control_dispatch_history() {
        let cmd = ControlMessage::from_json(r#"{"cmd":"history","lines":100,"timestamps":true}"#);
        assert_eq!(
            cmd,
            ControlMessage::History {
                lines: 100,
                timestamps: true
            }
        );
    }

    #[test]
    fn control_dispatch_disconnect() {
        let cmd = ControlMessage::from_json(r#"{"cmd":"disconnect"}"#);
        assert_eq!(cmd, ControlMessage::Disconnect);
    }

    #[test]
    fn control_dispatch_unknown() {
        let cmd = ControlMessage::from_json(r#"{"cmd":"reboot"}"#);
        assert!(matches!(cmd, ControlMessage::Unknown(_)));
    }

    #[test]
    fn client_context_count() {
        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        ctx.client_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            ctx.client_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn handle_control_status_returns_response() {
        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(2)),
        };
        let result = handle_control_command(&ControlMessage::Status, &ctx);
        assert!(result.is_some());
        let bytes = result.unwrap();
        // Should be a valid protocol response message: \0R{json}\0
        assert_eq!(bytes[0], 0x00);
        assert_eq!(bytes[1], b'R');
        assert_eq!(*bytes.last().unwrap(), 0x00);
        // Parse the JSON to verify content
        let json = std::str::from_utf8(&bytes[2..bytes.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["baudrate"], 115_200);
        assert_eq!(value["clients"], 2);
    }

    #[test]
    fn handle_control_unknown_returns_error() {
        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        let result = handle_control_command(&ControlMessage::Unknown("foo".into()), &ctx);
        assert!(result.is_some());
        let bytes = result.unwrap();
        let json = std::str::from_utf8(&bytes[2..bytes.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["error"], "unknown command");
    }

    #[tokio::test]
    async fn integration_server_to_client_data() {
        use tokio::io::AsyncReadExt;

        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, _serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let cancel = CancellationToken::new();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();

        // Spawn client handler with the server side of the pair
        let handler_cancel = cancel.clone();
        let handler_broadcast = broadcast_tx.clone();
        let handle = tokio::spawn(async move {
            handle_client(server_stream, handler_broadcast, serial_tx, ctx, handler_cancel).await;
        });

        // Give handler time to start and subscribe to broadcast
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Broadcast serial data — the handler should forward it protocol-encoded to the client
        broadcast_tx.send(Arc::new(b"hello serial".to_vec())).unwrap();

        // Read from the client side and decode through the protocol parser
        let mut buf = [0u8; 4096];
        let mut client = client_stream;
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read(&mut buf),
        )
        .await
        .expect("timed out waiting for data")
        .expect("read failed");

        // Data is now protocol-encoded, so parse it
        let mut parser = Parser::new();
        let events = parser.feed_bytes(&buf[..n]);
        let decoded: Vec<u8> = events
            .into_iter()
            .filter_map(|e| match e {
                ParseEvent::DataByte(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(decoded, b"hello serial");

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn integration_client_to_serial() {
        use crate::protocol::encode_data;
        use tokio::io::AsyncWriteExt;

        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, mut serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let cancel = CancellationToken::new();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();

        let handler_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            handle_client(server_stream, broadcast_tx, serial_tx, ctx, handler_cancel).await;
        });

        // Write protocol-encoded data from client side
        let mut client = client_stream;
        let encoded = encode_data(b"AB");
        client.write_all(&encoded).await.unwrap();

        // Each byte arrives individually via the protocol parser's DataByte events
        let timeout = std::time::Duration::from_secs(2);
        let a = tokio::time::timeout(timeout, serial_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(a, vec![b'A']);

        let b = tokio::time::timeout(timeout, serial_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(b, vec![b'B']);

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn integration_client_count_tracking() {
        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, _serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: client_count.clone(),
        });
        let cancel = CancellationToken::new();

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();

        let handler_cancel = cancel.clone();
        let handler_ctx = ctx.clone();
        let handle = tokio::spawn(async move {
            handle_client(server_stream, broadcast_tx, serial_tx, handler_ctx, handler_cancel).await;
        });

        // Give handler time to start and increment count
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(client_count.load(std::sync::atomic::Ordering::Relaxed), 1);

        // Drop client side → handler should detect disconnect and decrement
        drop(client_stream);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert_eq!(client_count.load(std::sync::atomic::Ordering::Relaxed), 0);

        cancel.cancel();
    }
}
