mod common;

use common::{TcpTestServer, TestClient, TestServer};
use rmux::log_reader;
use rmux::protocol::ControlMessage;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Data flow tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_to_client_data_flow() {
    let server = TestServer::start("s2c").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Simulate device sending data
    server.mock_serial.write(b"hello from device").await.unwrap();

    // Client should receive the same bytes
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"hello from device");

    server.stop().await;
}

#[tokio::test]
async fn client_to_serial_data_flow() {
    let server = TestServer::start("c2s").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Give client handler time to be fully set up
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client sends data
    client.send_data(b"AT\r\n").await;

    // Read from mock serial (what the server wrote to the device)
    let mut buf = [0u8; 256];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;

    while received.len() < 4 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, server.mock_serial.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }

    assert_eq!(received, b"AT\r\n");

    server.stop().await;
}

#[tokio::test]
async fn bidirectional_data_flow() {
    let server = TestServer::start("bidir").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client sends a command
    client.send_data(b"PING").await;

    // Read what arrived at the serial device
    let mut buf = [0u8; 256];
    let mut serial_received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while serial_received.len() < 4 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, server.mock_serial.read(&mut buf)).await {
            Ok(Ok(n)) => serial_received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(serial_received, b"PING");

    // Serial device echoes back "PONG"
    server.mock_serial.write(b"PONG").await.unwrap();

    // Client should receive the echo
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"PONG");

    server.stop().await;
}

#[tokio::test]
async fn nul_bytes_in_serial_data() {
    let server = TestServer::start("nul").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Send data containing NUL bytes from the serial device
    let payload = vec![0x41, 0x00, 0x42]; // A, NUL, B
    server.mock_serial.write(&payload).await.unwrap();

    // Client must receive the exact same bytes through protocol encode/decode
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, payload, "NUL bytes must survive the encode/decode cycle");

    server.stop().await;
}

#[tokio::test]
async fn binary_data_roundtrip() {
    let server = TestServer::start("binary").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Send all 256 byte values from the serial device
    let all_bytes: Vec<u8> = (0..=255).collect();
    server.mock_serial.write(&all_bytes).await.unwrap();

    // Client should receive all 256 bytes uncorrupted
    let mut data = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while data.len() < 256 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let chunk = client.read_data(remaining).await;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
    }

    assert_eq!(data.len(), 256, "expected 256 bytes, got {}", data.len());
    assert_eq!(data, all_bytes, "binary data was corrupted in transit");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Multi-client tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multiple_clients_receive_broadcast() {
    let server = TestServer::start("multi").await;
    let mut client1 = TestClient::connect(&server.socket_path).await;
    let mut client2 = TestClient::connect(&server.socket_path).await;

    // Give both clients time to subscribe
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Serial device sends data
    server.mock_serial.write(b"broadcast msg").await.unwrap();

    // Both clients should receive it
    let data1 = client1.read_data(TIMEOUT).await;
    let data2 = client2.read_data(TIMEOUT).await;

    assert_eq!(data1, b"broadcast msg");
    assert_eq!(data2, b"broadcast msg");

    server.stop().await;
}

#[tokio::test]
async fn client_disconnect_doesnt_affect_others() {
    let server = TestServer::start("disc").await;
    let mut client1 = TestClient::connect(&server.socket_path).await;
    let client2 = TestClient::connect(&server.socket_path).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Disconnect client2
    drop(client2);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Serial device sends data — client1 should still receive it
    server.mock_serial.write(b"still here").await.unwrap();

    let data = client1.read_data(TIMEOUT).await;
    assert_eq!(data, b"still here");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Control message tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_command_returns_response() {
    let server = TestServer::start("status").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send a status control message
    client.send_control(&ControlMessage::Status).await;

    // Should get a response
    let json = client
        .read_response(TIMEOUT)
        .await
        .expect("expected a response to status command");

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["baudrate"], 115_200);
    assert!(value["port"].is_string());

    server.stop().await;
}

#[tokio::test]
async fn unknown_command_returns_error() {
    let server = TestServer::start("unknown").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send an unknown control message
    client
        .send_control(&ControlMessage::Unknown(r#"{"cmd":"reboot"}"#.into()))
        .await;

    // Should get an error response
    let json = client
        .read_response(TIMEOUT)
        .await
        .expect("expected an error response");

    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["error"], "unknown command");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Edge case tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn client_connect_before_serial_data() {
    let server = TestServer::start("early").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Wait a bit, then send data from serial
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.mock_serial.write(b"delayed data").await.unwrap();

    // Client should still receive it
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"delayed data");

    server.stop().await;
}

#[tokio::test]
async fn rapid_serial_data() {
    let server = TestServer::start("rapid").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send many small chunks rapidly
    let total_chunks = 50;
    for i in 0..total_chunks {
        let chunk = format!("{i:02}");
        server.mock_serial.write(chunk.as_bytes()).await.unwrap();
    }

    // Read all data from client
    let mut data = Vec::new();
    let expected_len = total_chunks * 2; // each chunk is 2 bytes
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while data.len() < expected_len {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let chunk = client.read_data(remaining).await;
        if chunk.is_empty() {
            break;
        }
        data.extend_from_slice(&chunk);
    }

    // Verify all data was received
    let mut expected = Vec::new();
    for i in 0..total_chunks {
        expected.extend_from_slice(format!("{i:02}").as_bytes());
    }
    assert_eq!(data.len(), expected.len(), "data loss: got {} bytes, expected {}", data.len(), expected.len());
    assert_eq!(data, expected);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// History / log tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_writer_captures_serial_data() {
    let server = TestServer::start_with_logging("logcap").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    // Send several lines through mock serial
    server.mock_serial.write(b"line one\n").await.unwrap();
    server.mock_serial.write(b"line two\n").await.unwrap();
    server.mock_serial.write(b"line three\n").await.unwrap();

    // Wait for log writer to flush
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tmp_dir = server.stop_keep_files().await;

    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(content.contains("line one"), "log should contain 'line one'");
    assert!(content.contains("line two"), "log should contain 'line two'");
    assert!(content.contains("line three"), "log should contain 'line three'");

    // Each line should have a timestamp prefix
    for log_line in content.lines() {
        assert!(log_line.starts_with('['), "log line should start with timestamp: {log_line}");
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn history_shows_previous_data() {
    let server = TestServer::start_with_logging("histshow").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    // Send data with newlines through mock serial
    server.mock_serial.write(b"alpha\n").await.unwrap();
    server.mock_serial.write(b"beta\n").await.unwrap();
    server.mock_serial.write(b"gamma\n").await.unwrap();

    // Wait for log writer to flush
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tmp_dir = server.stop_keep_files().await;

    // Read last 2 lines from the log
    let lines = log_reader::read_last_lines(&log_path, 2).unwrap();
    assert_eq!(lines.len(), 2, "expected 2 lines, got {}", lines.len());

    // The lines should contain beta and gamma (last 2)
    let stripped: Vec<&str> = lines.iter().map(|l| log_reader::strip_timestamp(l)).collect();
    assert_eq!(stripped, vec!["beta", "gamma"]);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn history_strips_timestamps() {
    let server = TestServer::start_with_logging("histstrip").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    server.mock_serial.write(b"data payload\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tmp_dir = server.stop_keep_files().await;

    let lines = log_reader::read_last_lines(&log_path, 10).unwrap();
    assert!(!lines.is_empty(), "log should have at least one line");

    for line in &lines {
        let stripped = log_reader::strip_timestamp(line);
        assert!(!stripped.starts_with('['), "stripped line should not start with '['");
        assert!(!stripped.is_empty(), "stripped line should not be empty");
    }

    // Verify the actual data is preserved after stripping
    let stripped: Vec<&str> = lines.iter().map(|l| log_reader::strip_timestamp(l)).collect();
    assert!(stripped.contains(&"data payload"));

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn history_with_timestamps() {
    let server = TestServer::start_with_logging("histts").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    server.mock_serial.write(b"timestamped line\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tmp_dir = server.stop_keep_files().await;

    let lines = log_reader::read_last_lines(&log_path, 10).unwrap();
    assert!(!lines.is_empty(), "log should have at least one line");

    // Raw lines should have timestamp format: [YYYY-MM-DD HH:MM:SS.mmm] data
    for line in &lines {
        assert!(line.starts_with('['), "line should start with '[': {line}");
        assert!(line.contains("] "), "line should contain '] ': {line}");
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn history_empty_when_no_log() {
    let nonexistent = std::path::PathBuf::from("/tmp/rmux_nonexistent_log_file.log");
    let result = log_reader::read_last_lines(&nonexistent, 10);
    assert!(result.is_err(), "reading nonexistent log should return an error");
}

// ---------------------------------------------------------------------------
// TCP transport tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tcp_server_to_client_data_flow() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_s2c").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    mock_device.write(b"hello from tcp device").await.unwrap();

    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"hello from tcp device");

    server.stop().await;
}

#[tokio::test]
async fn tcp_client_to_device_data_flow() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_c2d").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    client.send_data(b"AT\r\n").await;

    let mut buf = [0u8; 256];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received.len() < 4 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, mock_device.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(received, b"AT\r\n");

    server.stop().await;
}

#[tokio::test]
async fn tcp_bidirectional_data_flow() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_bidir").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Client sends to device
    client.send_data(b"PING").await;

    let mut buf = [0u8; 256];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received.len() < 4 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, mock_device.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(received, b"PING");

    // Device sends back
    mock_device.write(b"PONG").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"PONG");

    server.stop().await;
}

#[tokio::test]
async fn tcp_reconnect_after_disconnect() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_reconn").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Verify initial connection works
    mock_device.write(b"before disconnect").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"before disconnect");

    // Drop the mock device to simulate TCP server going away
    drop(mock_device);

    // Read the "connection lost" marker
    let lost_marker = client.read_data(TIMEOUT).await;
    let lost_str = String::from_utf8_lossy(&lost_marker);
    assert!(
        lost_str.contains("--- connection lost ---"),
        "expected connection lost marker, got: {lost_str:?}"
    );

    // Start a new TCP listener on the same port and accept the reconnection
    let mut new_device = server.accept_reconnect(Duration::from_secs(10)).await;

    // Read the "reconnected" marker
    let reconn_marker = client.read_data(TIMEOUT).await;
    let reconn_str = String::from_utf8_lossy(&reconn_marker);
    assert!(
        reconn_str.contains("--- reconnected ---"),
        "expected reconnected marker, got: {reconn_str:?}"
    );

    // Verify data flows again after reconnection
    new_device.write(b"after reconnect").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"after reconnect");

    server.stop().await;
}

#[tokio::test]
async fn tcp_reconnect_write_after_reconnect() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_reconn_wr").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify initial write works
    client.send_data(b"first").await;
    let mut buf = [0u8; 256];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received.len() < 5 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, mock_device.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(received, b"first");

    // Drop mock device to simulate disconnect
    drop(mock_device);

    // Read the "connection lost" marker
    let lost_marker = client.read_data(TIMEOUT).await;
    let lost_str = String::from_utf8_lossy(&lost_marker);
    assert!(
        lost_str.contains("--- connection lost ---"),
        "expected connection lost marker, got: {lost_str:?}"
    );

    // Accept reconnection
    let mut new_device = server.accept_reconnect(Duration::from_secs(10)).await;

    // Read the "reconnected" marker
    let reconn_marker = client.read_data(TIMEOUT).await;
    let reconn_str = String::from_utf8_lossy(&reconn_marker);
    assert!(
        reconn_str.contains("--- reconnected ---"),
        "expected reconnected marker, got: {reconn_str:?}"
    );

    // Verify writes work after reconnection
    client.send_data(b"second").await;
    let mut received2 = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received2.len() < 6 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, new_device.read(&mut buf)).await {
            Ok(Ok(n)) => received2.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(received2, b"second");

    server.stop().await;
}

#[tokio::test]
async fn tcp_reconnect_multiple_cycles() {
    let (server, mock_device) = TcpTestServer::start("tcp_mrc").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Drop first connection
    drop(mock_device);

    // Read connection lost marker
    let lost = client.read_data(TIMEOUT).await;
    assert!(String::from_utf8_lossy(&lost).contains("--- connection lost ---"));

    // Reconnect cycle 1
    let mut device1 = server.accept_reconnect(Duration::from_secs(10)).await;

    // Read reconnected marker
    let reconn = client.read_data(TIMEOUT).await;
    assert!(String::from_utf8_lossy(&reconn).contains("--- reconnected ---"));

    device1.write(b"cycle1").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"cycle1");

    // Drop again
    drop(device1);

    // Read connection lost marker
    let lost2 = client.read_data(TIMEOUT).await;
    assert!(String::from_utf8_lossy(&lost2).contains("--- connection lost ---"));

    // Reconnect cycle 2
    let mut device2 = server.accept_reconnect(Duration::from_secs(10)).await;

    // Read reconnected marker
    let reconn2 = client.read_data(TIMEOUT).await;
    assert!(String::from_utf8_lossy(&reconn2).contains("--- reconnected ---"));

    device2.write(b"cycle2").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"cycle2");

    server.stop().await;
}
