mod common;

use common::{TcpTestClient, TcpTestServer, TestClient, TestServer};
use rmux::log_reader;
use rmux::protocol::ControlMessage;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Simple deterministic PRNG (xorshift32) so tests don't depend on `rand`.
fn pseudo_random_bytes(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state & 0xFF) as u8
        })
        .collect()
}

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

#[tokio::test]
async fn since_pattern_from_log() {
    let server = TestServer::start_with_logging("since_basic").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    // Send several lines including a boot marker
    server.mock_serial.write(b"init stuff\n").await.unwrap();
    server
        .mock_serial
        .write(b"U-Boot 2024.01\n")
        .await
        .unwrap();
    server
        .mock_serial
        .write(b"Loading kernel\n")
        .await
        .unwrap();
    server.mock_serial.write(b"Started\n").await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let tmp_dir = server.stop_keep_files().await;

    let re = regex::Regex::new("U-Boot").unwrap();
    let lines = log_reader::read_lines_since_pattern(&log_path, &re).unwrap();
    assert_eq!(lines.len(), 3);

    let stripped: Vec<&str> = lines.iter().map(|l| log_reader::strip_timestamp(l)).collect();
    assert_eq!(stripped, vec!["U-Boot 2024.01", "Loading kernel", "Started"]);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn since_pattern_multiple_boots() {
    let server = TestServer::start_with_logging("since_multi").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    // First boot sequence
    server
        .mock_serial
        .write(b"U-Boot 2024.01\n")
        .await
        .unwrap();
    server.mock_serial.write(b"first boot\n").await.unwrap();

    // Second boot sequence
    server
        .mock_serial
        .write(b"U-Boot 2024.01\n")
        .await
        .unwrap();
    server.mock_serial.write(b"second boot\n").await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let tmp_dir = server.stop_keep_files().await;

    let re = regex::Regex::new("U-Boot").unwrap();
    let lines = log_reader::read_lines_since_pattern(&log_path, &re).unwrap();

    // Should only get lines from the LAST boot
    let stripped: Vec<&str> = lines.iter().map(|l| log_reader::strip_timestamp(l)).collect();
    assert_eq!(stripped, vec!["U-Boot 2024.01", "second boot"]);

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[tokio::test]
async fn since_pattern_no_match_in_log() {
    let server = TestServer::start_with_logging("since_nomatch").await;
    let log_path = server.log_path.clone().expect("log_path should be set");

    server
        .mock_serial
        .write(b"some data\nmore data\n")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let tmp_dir = server.stop_keep_files().await;

    let re = regex::Regex::new("NONEXISTENT").unwrap();
    let lines = log_reader::read_lines_since_pattern(&log_path, &re).unwrap();
    assert!(lines.is_empty(), "should return empty when no match");

    let _ = std::fs::remove_dir_all(&tmp_dir);
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

// ---------------------------------------------------------------------------
// Random / non-ASCII / binary stress tests
// ---------------------------------------------------------------------------

/// Random bytes (including non-ASCII and NULs) pass through uncorrupted.
#[tokio::test]
async fn random_binary_data_survives() {
    let server = TestServer::start("rand_bin").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    let payload = pseudo_random_bytes(0xDEAD, 1024);
    server.mock_serial.write(&payload).await.unwrap();

    let data = client.read_all_data(payload.len(), TIMEOUT).await;

    assert_eq!(
        data.len(),
        payload.len(),
        "expected {} bytes, got {}",
        payload.len(),
        data.len()
    );
    assert_eq!(data, payload, "random binary data was corrupted in transit");

    server.stop().await;
}

/// A payload consisting entirely of NUL bytes (worst case for the protocol's
/// NUL-escape scheme) passes through without corruption or server exit.
#[tokio::test]
async fn all_nul_payload() {
    let server = TestServer::start("all_nul").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    let payload = vec![0x00u8; 512];
    server.mock_serial.write(&payload).await.unwrap();

    let data = client.read_all_data(payload.len(), TIMEOUT).await;

    assert_eq!(data.len(), payload.len());
    assert_eq!(data, payload);

    server.stop().await;
}

/// Many small bursts of random data sent rapidly simulate a noisy UART line.
/// Uses concurrent write/read because the total payload exceeds the PTY buffer.
#[tokio::test]
async fn rapid_random_bursts() {
    let server = TestServer::start("rand_burst").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut expected = Vec::new();
    for i in 0..100u32 {
        let chunk = pseudo_random_bytes(i.wrapping_mul(7919), 32);
        expected.extend_from_slice(&chunk);
    }

    // Write all bursts from a background task
    let mock = server.mock_serial.clone();
    let write_handle = tokio::spawn(async move {
        for i in 0..100u32 {
            let chunk = pseudo_random_bytes(i.wrapping_mul(7919), 32);
            mock.write(&chunk).await.unwrap();
        }
    });

    let data = client.read_all_data(expected.len(), TIMEOUT).await;
    write_handle.await.unwrap();

    assert_eq!(
        data.len(),
        expected.len(),
        "data loss during rapid random bursts: got {}, expected {}",
        data.len(),
        expected.len()
    );
    assert_eq!(data, expected);

    server.stop().await;
}

/// After receiving random garbage, the server should still respond to control
/// commands and deliver normal data.
#[tokio::test]
async fn server_healthy_after_random_data() {
    let server = TestServer::start("rand_health").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Send random garbage through the serial device (concurrent write/read
    // because the payload may exceed the PTY buffer)
    let garbage = pseudo_random_bytes(0xCAFE, 2048);
    let mock = server.mock_serial.clone();
    let write_garbage = garbage.clone();
    let write_handle = tokio::spawn(async move {
        mock.write(&write_garbage).await.unwrap();
    });

    // Drain the garbage from the client
    let drained = client.read_all_data(garbage.len(), TIMEOUT).await;
    write_handle.await.unwrap();
    assert_eq!(drained.len(), garbage.len(), "failed to drain garbage data");

    // Now verify the server is still healthy: send a status command
    client.send_control(&ControlMessage::Status).await;
    let json = client
        .read_response(TIMEOUT)
        .await
        .expect("server should still respond to status after random data");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["baudrate"], 115_200);

    // And verify normal data still flows
    server.mock_serial.write(b"still alive").await.unwrap();
    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"still alive");

    server.stop().await;
}

/// Multiple clients should all receive the same random data without corruption.
#[tokio::test]
async fn multiple_clients_receive_random_data() {
    let server = TestServer::start("rand_multi").await;
    let mut client1 = TestClient::connect(&server.socket_path).await;
    let mut client2 = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = pseudo_random_bytes(0xBEEF, 512);
    server.mock_serial.write(&payload).await.unwrap();

    let data1 = client1.read_all_data(payload.len(), TIMEOUT).await;
    let data2 = client2.read_all_data(payload.len(), TIMEOUT).await;

    assert_eq!(data1, payload, "client1 data corrupted");
    assert_eq!(data2, payload, "client2 data corrupted");

    server.stop().await;
}

/// Random data with logging enabled: server and log writer must not crash.
#[tokio::test]
async fn random_data_with_logging() {
    let server = TestServer::start_with_logging("rand_log").await;
    let log_path = server.log_path.clone().expect("log_path should be set");
    let mut client = TestClient::connect(&server.socket_path).await;

    // Send random data that includes embedded newlines (so the log writer
    // actually flushes lines) as well as arbitrary binary values.
    let mut payload = pseudo_random_bytes(0xF00D, 512);
    // Sprinkle some newlines to trigger log line writes
    for i in (0..payload.len()).step_by(64) {
        payload[i] = b'\n';
    }
    server.mock_serial.write(&payload).await.unwrap();

    // Drain from client
    let data = client.read_all_data(payload.len(), TIMEOUT).await;
    assert_eq!(data.len(), payload.len());

    // Wait for log writer to flush
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tmp_dir = server.stop_keep_files().await;

    // Log file should exist and not be empty
    let log_content = std::fs::read(&log_path).unwrap();
    assert!(!log_content.is_empty(), "log file should not be empty");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// Large payload of random data (16 KB) to stress the pipeline.
/// Uses concurrent write/read to avoid PTY buffer deadlock.
#[tokio::test]
async fn large_random_payload() {
    let server = TestServer::start("rand_large").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    let payload = pseudo_random_bytes(0x1234, 16384);

    // Must write and read concurrently: the PTY buffer (~4KB) is smaller
    // than the payload, so the write blocks until the reader drains data.
    let mock = server.mock_serial.clone();
    let write_payload = payload.clone();
    let write_handle = tokio::spawn(async move {
        mock.write(&write_payload).await.unwrap();
    });

    let data = client
        .read_all_data(payload.len(), Duration::from_secs(10))
        .await;

    write_handle.await.unwrap();

    assert_eq!(
        data.len(),
        payload.len(),
        "large payload: got {} bytes, expected {}",
        data.len(),
        payload.len()
    );
    assert_eq!(data, payload);

    server.stop().await;
}

/// Bytes that look like protocol control prefixes (0x00 followed by 'C' or 'R')
/// embedded in serial data should pass through as plain data, not be
/// misinterpreted as protocol messages.
#[tokio::test]
async fn protocol_like_bytes_in_serial_data() {
    let server = TestServer::start("proto_like").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // Craft a payload with bytes that look like protocol control sequences
    let payload: Vec<u8> = vec![
        b'A',
        0x00, b'C', // looks like start of a control message
        b'B',
        0x00, b'R', // looks like start of a response message
        b'C',
        0x00, 0x00, // escaped NUL
        b'D',
    ];
    server.mock_serial.write(&payload).await.unwrap();

    let data = client.read_all_data(payload.len(), TIMEOUT).await;

    assert_eq!(
        data, payload,
        "protocol-like bytes in serial data should pass through unchanged"
    );

    server.stop().await;
}

/// TCP transport handles random binary data without crashing.
#[tokio::test]
async fn tcp_random_binary_data() {
    let (server, mut mock_device) = TcpTestServer::start("tcp_rand").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    let payload = pseudo_random_bytes(0xABCD, 1024);
    mock_device.write(&payload).await.unwrap();

    let data = client.read_all_data(payload.len(), TIMEOUT).await;

    assert_eq!(data.len(), payload.len());
    assert_eq!(data, payload, "TCP random data corrupted");

    server.stop().await;
}

/// A single large chunk of data (simulates a UART burst or buffer dump) must
/// pass through without data loss or server exit.
/// Uses concurrent write/read to avoid PTY buffer deadlock.
#[tokio::test]
async fn large_single_chunk_data() {
    let server = TestServer::start("big_chunk").await;
    let mut client = TestClient::connect(&server.socket_path).await;

    // 32 KB in one write — exercises the PTY buffer backpressure path
    let payload = pseudo_random_bytes(0x7777, 32768);

    let mock = server.mock_serial.clone();
    let write_payload = payload.clone();
    let write_handle = tokio::spawn(async move {
        mock.write(&write_payload).await.unwrap();
    });

    let data = client
        .read_all_data(payload.len(), Duration::from_secs(15))
        .await;

    write_handle.await.unwrap();

    assert_eq!(
        data.len(),
        payload.len(),
        "large single chunk: got {} bytes, expected {}",
        data.len(),
        payload.len()
    );
    assert_eq!(data, payload);

    server.stop().await;
}

/// Client sends random binary data to the serial device.
#[tokio::test]
async fn client_sends_random_data_to_serial() {
    let server = TestServer::start("rand_c2s").await;
    let mut client = TestClient::connect(&server.socket_path).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Avoid NUL in the payload so we don't have to deal with protocol escaping
    // from the client side (encode_data handles it, but the mock serial read
    // gets raw bytes). Instead, use bytes 0x01-0xFF.
    let payload: Vec<u8> = pseudo_random_bytes(0x9999, 256)
        .into_iter()
        .map(|b| if b == 0 { 1 } else { b })
        .collect();

    // Send in chunks to avoid overwhelming the pipeline (the client handler
    // forwards each byte individually through the mpsc channel)
    for chunk in payload.chunks(32) {
        client.send_data(chunk).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let mut buf = [0u8; 4096];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received.len() < payload.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, server.mock_serial.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }

    assert_eq!(
        received.len(),
        payload.len(),
        "client→serial: got {} bytes, expected {}",
        received.len(),
        payload.len()
    );
    assert_eq!(received, payload);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// TCP client listener tests
// ---------------------------------------------------------------------------

/// TCP client receives data from serial device.
#[tokio::test]
async fn tcp_client_server_to_client_data_flow() {
    let server = TestServer::start_with_tcp("tcp_s2c").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.mock_serial.write(b"hello from device").await.unwrap();

    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"hello from device");

    server.stop().await;
}

/// TCP client sends data to serial device.
#[tokio::test]
async fn tcp_client_to_serial_data_flow() {
    let server = TestServer::start_with_tcp("tcp_c2s").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
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
        match tokio::time::timeout(remaining, server.mock_serial.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }

    assert_eq!(received, b"AT\r\n");

    server.stop().await;
}

/// Full round-trip over TCP: client sends → serial receives → serial responds → TCP client reads.
#[tokio::test]
async fn tcp_client_bidirectional_data_flow() {
    let server = TestServer::start_with_tcp("tcp_bidir").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    client.send_data(b"PING").await;

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

    server.mock_serial.write(b"PONG").await.unwrap();

    let data = client.read_data(TIMEOUT).await;
    assert_eq!(data, b"PONG");

    server.stop().await;
}

/// Binary data containing NUL bytes flows correctly over TCP client connection.
#[tokio::test]
async fn tcp_client_nul_bytes() {
    let server = TestServer::start_with_tcp("tcp_nul").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Serial device sends data containing NUL bytes
    let data_with_nuls = b"before\x00middle\x00after";
    server.mock_serial.write(data_with_nuls).await.unwrap();

    let received = client.read_all_data(data_with_nuls.len(), TIMEOUT).await;
    assert_eq!(received, data_with_nuls);

    server.stop().await;
}

/// Random binary data survives TCP client transport.
#[tokio::test]
async fn tcp_client_binary_data_roundtrip() {
    let server = TestServer::start_with_tcp("tcp_bin").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = pseudo_random_bytes(0xABCD, 512);
    server.mock_serial.write(&payload).await.unwrap();

    let received = client.read_all_data(payload.len(), TIMEOUT).await;
    assert_eq!(
        received.len(),
        payload.len(),
        "TCP binary roundtrip: got {} bytes, expected {}",
        received.len(),
        payload.len()
    );
    assert_eq!(received, payload);

    server.stop().await;
}

/// Two TCP clients connected simultaneously both receive serial data.
#[tokio::test]
async fn multiple_tcp_clients_receive_broadcast() {
    let server = TestServer::start_with_tcp("tcp_multi").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client1 = TcpTestClient::connect(addr).await;
    let mut client2 = TcpTestClient::connect(addr).await;

    // Give handlers time to subscribe
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.mock_serial.write(b"broadcast msg").await.unwrap();

    let data1 = client1.read_data(TIMEOUT).await;
    let data2 = client2.read_data(TIMEOUT).await;

    assert_eq!(data1, b"broadcast msg");
    assert_eq!(data2, b"broadcast msg");

    server.stop().await;
}

/// One Unix client + one TCP client both receive the same serial data.
#[tokio::test]
async fn mixed_unix_and_tcp_clients_receive_broadcast() {
    let server = TestServer::start_with_tcp("tcp_mixed").await;
    let addr = server.tcp_listen_addr.unwrap();

    let mut unix_client = TestClient::connect(&server.socket_path).await;
    let mut tcp_client = TcpTestClient::connect(addr).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    server.mock_serial.write(b"mixed broadcast").await.unwrap();

    let unix_data = unix_client.read_data(TIMEOUT).await;
    let tcp_data = tcp_client.read_data(TIMEOUT).await;

    assert_eq!(unix_data, b"mixed broadcast");
    assert_eq!(tcp_data, b"mixed broadcast");

    server.stop().await;
}

/// Drop one TCP client, verify others still receive data.
#[tokio::test]
async fn tcp_client_disconnect_doesnt_affect_others() {
    let server = TestServer::start_with_tcp("tcp_disc").await;
    let addr = server.tcp_listen_addr.unwrap();

    let client1 = TcpTestClient::connect(addr).await;
    let mut client2 = TcpTestClient::connect(addr).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Drop first client
    drop(client1);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Second client should still receive data
    server.mock_serial.write(b"still alive").await.unwrap();
    let data = client2.read_data(TIMEOUT).await;
    assert_eq!(data, b"still alive");

    server.stop().await;
}

/// Drop a TCP client, verify Unix client is unaffected (and vice versa).
#[tokio::test]
async fn mixed_client_disconnect_isolation() {
    let server = TestServer::start_with_tcp("tcp_iso").await;
    let addr = server.tcp_listen_addr.unwrap();

    let tcp_client = TcpTestClient::connect(addr).await;
    let mut unix_client = TestClient::connect(&server.socket_path).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Drop TCP client
    drop(tcp_client);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Unix client should still work
    server.mock_serial.write(b"unix ok").await.unwrap();
    let data = unix_client.read_data(TIMEOUT).await;
    assert_eq!(data, b"unix ok");

    server.stop().await;
}

/// TCP client sends status command and receives server info response.
#[tokio::test]
async fn tcp_client_status_command() {
    let server = TestServer::start_with_tcp("tcp_status").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    client.send_control(&ControlMessage::Status).await;

    let response = client.read_response(TIMEOUT).await;
    assert!(response.is_some(), "should receive a status response");
    let json: serde_json::Value = serde_json::from_str(&response.unwrap()).unwrap();
    assert_eq!(json["port"], "/dev/mock");
    assert_eq!(json["baudrate"], 115_200);
    assert!(json["serial"].as_bool().unwrap());

    server.stop().await;
}

/// TCP client sends unknown command and receives error response.
#[tokio::test]
async fn tcp_client_unknown_command() {
    let server = TestServer::start_with_tcp("tcp_unk").await;
    let addr = server.tcp_listen_addr.unwrap();
    let mut client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    client.send_control(&ControlMessage::Unknown("bogus".into())).await;

    let response = client.read_response(TIMEOUT).await;
    assert!(response.is_some(), "should receive an error response");
    let json: serde_json::Value = serde_json::from_str(&response.unwrap()).unwrap();
    assert_eq!(json["error"], "unknown command");

    server.stop().await;
}

/// TCP listener coexists with Unix socket — both accept independently.
#[tokio::test]
async fn tcp_listener_coexists_with_unix() {
    let server = TestServer::start_with_tcp("tcp_coexist").await;
    let addr = server.tcp_listen_addr.unwrap();

    // Connect via both transports
    let mut unix_client = TestClient::connect(&server.socket_path).await;
    let mut tcp_client = TcpTestClient::connect(addr).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Both should be able to send data to serial
    unix_client.send_data(b"U").await;
    tcp_client.send_data(b"T").await;

    // Read from serial — should get both bytes (order may vary)
    let mut buf = [0u8; 256];
    let mut received = Vec::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while received.len() < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, server.mock_serial.read(&mut buf)).await {
            Ok(Ok(n)) => received.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }

    assert_eq!(received.len(), 2);
    assert!(received.contains(&b'U'));
    assert!(received.contains(&b'T'));

    // Both should receive broadcast data
    server.mock_serial.write(b"both").await.unwrap();
    let unix_data = unix_client.read_data(TIMEOUT).await;
    let tcp_data = tcp_client.read_data(TIMEOUT).await;
    assert_eq!(unix_data, b"both");
    assert_eq!(tcp_data, b"both");

    server.stop().await;
}
