use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::{
    encode_data, encode_response, ControlMessage, ErrorResponse, EventResponse, MessageKind,
    ParseEvent, Parser, ResponseMessage, ServerInfoResponse,
};

/// Shared server state that client handlers can query.
pub struct ClientContext {
    pub port: String,
    pub baudrate: u32,
    pub serial_connected: bool,
    pub client_count: Arc<std::sync::atomic::AtomicUsize>,
    pub log_file: Option<std::path::PathBuf>,
}

/// Handle a single client connection over any async stream (Unix socket or TCP).
///
/// This spawns two concurrent tasks:
/// 1. Reader: serial broadcast → protocol-encode → client socket
/// 2. Writer: client socket → parse protocol → handle commands / forward data
pub async fn handle_client(
    stream: impl AsyncRead + AsyncWrite + Send + 'static,
    broadcast_tx: broadcast::Sender<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    ctx: Arc<ClientContext>,
    cancel: CancellationToken,
) {
    let client_cancel = cancel.child_token();
    let count = ctx.client_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    tracing::info!("client connected (total: {count})");

    let (mut read_half, mut write_half) = tokio::io::split(stream);

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
        ControlMessage::History {
            lines,
            timestamps,
            since,
        } => {
            tracing::debug!(
                "history requested: {lines} lines, timestamps={timestamps}, since={since:?}"
            );

            let log_path = match &ctx.log_file {
                Some(p) if p.exists() => p,
                Some(_) => {
                    return Some(encode_response(&ResponseMessage::Error(ErrorResponse {
                        error: "log file not found".into(),
                    })));
                }
                None => {
                    return Some(encode_response(&ResponseMessage::Error(ErrorResponse {
                        error: "logging is not enabled on this server".into(),
                    })));
                }
            };

            let read_result = if let Some(ref pattern) = since {
                match regex::Regex::new(pattern) {
                    Ok(re) => crate::log_reader::read_lines_since_pattern(log_path, &re),
                    Err(e) => {
                        return Some(encode_response(&ResponseMessage::Error(ErrorResponse {
                            error: format!("invalid regex: {e}"),
                        })));
                    }
                }
            } else {
                crate::log_reader::read_last_lines(log_path, *lines)
            };

            let log_lines = match read_result {
                Ok(lines) => lines,
                Err(e) => {
                    return Some(encode_response(&ResponseMessage::Error(ErrorResponse {
                        error: format!("failed to read log: {e}"),
                    })));
                }
            };

            let mut response_bytes = Vec::new();
            for line in &log_lines {
                let display_line = if *timestamps {
                    format!("{line}\r\n")
                } else {
                    format!("{}\r\n", crate::log_reader::strip_timestamp(line))
                };
                response_bytes.extend_from_slice(&encode_data(display_line.as_bytes()));
            }

            let end_event = ResponseMessage::Event(EventResponse {
                event: "history_end".into(),
            });
            response_bytes.extend_from_slice(&encode_response(&end_event));

            Some(response_bytes)
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
                timestamps: true,
                since: None,
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
            log_file: None,
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
            log_file: None,
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
            log_file: None,
        };
        let result = handle_control_command(&ControlMessage::Unknown("foo".into()), &ctx);
        assert!(result.is_some());
        let bytes = result.unwrap();
        let json = std::str::from_utf8(&bytes[2..bytes.len() - 1]).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["error"], "unknown command");
    }

    /// Helper: parse response bytes through the protocol parser, collecting
    /// data bytes as a string and response JSON messages.
    fn parse_history_response(bytes: &[u8]) -> (String, Vec<serde_json::Value>) {
        use crate::protocol::{MessageKind, ParseEvent, Parser};
        let mut parser = Parser::new();
        let events = parser.feed_bytes(bytes);
        let mut data = Vec::new();
        let mut responses = Vec::new();
        for event in events {
            match event {
                ParseEvent::DataByte(b) => data.push(b),
                ParseEvent::Message {
                    kind: MessageKind::Response,
                    json,
                } => {
                    if let Ok(v) = serde_json::from_str(&json) {
                        responses.push(v);
                    }
                }
                _ => {}
            }
        }
        (String::from_utf8_lossy(&data).into_owned(), responses)
    }

    #[test]
    fn handle_control_history_no_log() {
        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: None,
        };
        let cmd = ControlMessage::History {
            lines: 10,
            timestamps: false,
            since: None,
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (_, responses) = parse_history_response(&result.unwrap());
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0]["error"],
            "logging is not enabled on this server"
        );
    }

    #[test]
    fn handle_control_history_missing_file() {
        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(std::path::PathBuf::from("/tmp/rmux_nonexistent_test.log")),
        };
        let cmd = ControlMessage::History {
            lines: 10,
            timestamps: false,
            since: None,
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (_, responses) = parse_history_response(&result.unwrap());
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["error"], "log file not found");
    }

    #[test]
    fn handle_control_history_returns_data() {
        let dir = std::env::temp_dir().join(format!(
            "rmux_hist_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("test.log");
        std::fs::write(
            &log_file,
            "[2024-01-01 12:00:00.000] alpha\n\
             [2024-01-01 12:00:01.000] beta\n\
             [2024-01-01 12:00:02.000] gamma\n",
        )
        .unwrap();

        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(log_file),
        };
        let cmd = ControlMessage::History {
            lines: 2,
            timestamps: false,
            since: None,
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (data, responses) = parse_history_response(&result.unwrap());

        // Should contain last 2 lines, timestamps stripped
        assert!(data.contains("beta"), "expected 'beta' in: {data}");
        assert!(data.contains("gamma"), "expected 'gamma' in: {data}");
        assert!(!data.contains("alpha"), "should not contain 'alpha': {data}");
        assert!(
            !data.contains("[2024"),
            "timestamps should be stripped: {data}"
        );

        // Should end with history_end event
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["event"], "history_end");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_control_history_with_timestamps() {
        let dir = std::env::temp_dir().join(format!(
            "rmux_hist_ts_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("test.log");
        std::fs::write(
            &log_file,
            "[2024-01-01 12:00:00.000] hello\n",
        )
        .unwrap();

        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(log_file),
        };
        let cmd = ControlMessage::History {
            lines: 10,
            timestamps: true,
            since: None,
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (data, responses) = parse_history_response(&result.unwrap());

        assert!(
            data.contains("[2024-01-01 12:00:00.000]"),
            "timestamps should be preserved: {data}"
        );
        assert!(data.contains("hello"));
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["event"], "history_end");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_control_history_since() {
        let dir = std::env::temp_dir().join(format!(
            "rmux_hist_since_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("test.log");
        std::fs::write(
            &log_file,
            "[2024-01-01 12:00:00.000] init\n\
             [2024-01-01 12:00:01.000] BOOT MARKER\n\
             [2024-01-01 12:00:02.000] running\n",
        )
        .unwrap();

        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(log_file),
        };
        let cmd = ControlMessage::History {
            lines: 0,
            timestamps: false,
            since: Some("BOOT MARKER".into()),
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (data, responses) = parse_history_response(&result.unwrap());

        assert!(data.contains("BOOT MARKER"), "should contain marker: {data}");
        assert!(data.contains("running"), "should contain lines after marker: {data}");
        assert!(!data.contains("init"), "should not contain lines before marker: {data}");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["event"], "history_end");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_control_history_invalid_regex() {
        let dir = std::env::temp_dir().join(format!(
            "rmux_hist_badre_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("test.log");
        std::fs::write(&log_file, "line\n").unwrap();

        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(log_file),
        };
        let cmd = ControlMessage::History {
            lines: 0,
            timestamps: false,
            since: Some("[invalid".into()),
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (_, responses) = parse_history_response(&result.unwrap());
        assert_eq!(responses.len(), 1);
        let error = responses[0]["error"].as_str().unwrap();
        assert!(
            error.contains("invalid regex"),
            "expected regex error, got: {error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_control_history_since_no_match() {
        let dir = std::env::temp_dir().join(format!(
            "rmux_hist_nomatch_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_file = dir.join("test.log");
        std::fs::write(&log_file, "alpha\nbeta\n").unwrap();

        let ctx = ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: Some(log_file),
        };
        let cmd = ControlMessage::History {
            lines: 0,
            timestamps: false,
            since: Some("NONEXISTENT".into()),
        };
        let result = handle_control_command(&cmd, &ctx);
        assert!(result.is_some());
        let (data, responses) = parse_history_response(&result.unwrap());

        // No data, just history_end
        assert!(data.is_empty(), "should have no data: {data}");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["event"], "history_end");

        let _ = std::fs::remove_dir_all(&dir);
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
            log_file: None,
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
            log_file: None,
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
            log_file: None,
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

    #[tokio::test]
    async fn integration_tcp_server_to_client_data() {
        use tokio::io::AsyncReadExt;

        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, _serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: None,
        });
        let cancel = CancellationToken::new();

        // Use TCP instead of Unix socket
        let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();

        let handler_cancel = cancel.clone();
        let handler_broadcast = broadcast_tx.clone();
        let handle = tokio::spawn(async move {
            let (server_stream, _) = tcp_listener.accept().await.unwrap();
            handle_client(server_stream, handler_broadcast, serial_tx, ctx, handler_cancel).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Give handler time to start and subscribe to broadcast
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        broadcast_tx.send(Arc::new(b"hello tcp".to_vec())).unwrap();

        let mut buf = [0u8; 4096];
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read(&mut buf),
        )
        .await
        .expect("timed out waiting for data")
        .expect("read failed");

        let mut parser = Parser::new();
        let events = parser.feed_bytes(&buf[..n]);
        let decoded: Vec<u8> = events
            .into_iter()
            .filter_map(|e| match e {
                ParseEvent::DataByte(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(decoded, b"hello tcp");

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn integration_tcp_client_to_serial() {
        use crate::protocol::encode_data;
        use tokio::io::AsyncWriteExt;

        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, mut serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            log_file: None,
        });
        let cancel = CancellationToken::new();

        let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();

        let handler_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            let (server_stream, _) = tcp_listener.accept().await.unwrap();
            handle_client(server_stream, broadcast_tx, serial_tx, ctx, handler_cancel).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();

        let encoded = encode_data(b"CD");
        client.write_all(&encoded).await.unwrap();

        let timeout = std::time::Duration::from_secs(2);
        let c = tokio::time::timeout(timeout, serial_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(c, vec![b'C']);

        let d = tokio::time::timeout(timeout, serial_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        assert_eq!(d, vec![b'D']);

        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn integration_tcp_client_count_tracking() {
        let (broadcast_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(256);
        let (serial_tx, _serial_rx) = mpsc::channel::<Vec<u8>>(256);
        let client_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = Arc::new(ClientContext {
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: client_count.clone(),
            log_file: None,
        });
        let cancel = CancellationToken::new();

        let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp_listener.local_addr().unwrap();

        let handler_cancel = cancel.clone();
        let handler_ctx = ctx.clone();
        let handle = tokio::spawn(async move {
            let (server_stream, _) = tcp_listener.accept().await.unwrap();
            handle_client(server_stream, broadcast_tx, serial_tx, handler_ctx, handler_cancel).await;
        });

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

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
