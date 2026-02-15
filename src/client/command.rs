use regex::Regex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::protocol::{encode_control, encode_data, ControlMessage, MessageKind, ParseEvent, Parser};

/// Run the client in command mode: send a string, then collect output.
///
/// Returns the collected output as a string.
pub async fn run_command(
    stream: &mut UnixStream,
    send: Option<&str>,
    collect_secs: Option<f64>,
    wait_for: Option<&str>,
    timestamps: bool,
) -> anyhow::Result<String> {
    let mut parser = Parser::new();
    let mut output = String::new();

    // Optionally request history first
    // (handled at a higher level if --history is set)

    // Send data if specified
    if let Some(data) = send {
        let encoded = encode_data(data.as_bytes());
        stream.write_all(&encoded).await?;
    }

    // Determine timeout
    let timeout_dur = Duration::from_secs_f64(collect_secs.unwrap_or(5.0));

    // Compile wait_for regex if provided
    let pattern = wait_for
        .map(Regex::new)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;

    let mut buf = [0u8; 4096];

    let result = tokio::time::timeout(timeout_dur, async {
        loop {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok::<_, anyhow::Error>(());
            }

            let events = parser.feed_bytes(&buf[..n]);
            for event in events {
                match event {
                    ParseEvent::DataByte(b) => {
                        if b.is_ascii() {
                            output.push(b as char);
                        }
                    }
                    ParseEvent::MalformedEscape(b) => {
                        output.push('\0');
                        if b.is_ascii() {
                            output.push(b as char);
                        }
                    }
                    ParseEvent::Message {
                        kind: MessageKind::Response,
                        json,
                    } => {
                        // Could handle response messages (status replies, etc.)
                        tracing::debug!("received response: {json}");
                    }
                    _ => {}
                }
            }

            // Check if pattern matched
            if let Some(ref re) = pattern {
                if re.is_match(&output) {
                    return Ok(());
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            // Timeout reached — return what we collected
            if wait_for.is_some() {
                tracing::debug!("timeout waiting for pattern");
            }
        }
    }

    // Send disconnect
    let disconnect = encode_control(&ControlMessage::Disconnect);
    let _ = stream.write_all(&disconnect).await;

    if !timestamps {
        // Strip timestamp prefixes from lines
        output = output
            .lines()
            .map(crate::log_reader::strip_timestamp)
            .collect::<Vec<_>>()
            .join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_mode_timeout() {
        // Create a Unix socket pair
        let (mut server, mut client) = UnixStream::pair().unwrap();

        // Server sends some data
        let server_task = tokio::spawn(async move {
            server.write_all(b"hello\r\n").await.unwrap();
            // Keep connection open until client disconnects
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let output = run_command(&mut client, None, Some(0.2), None, false).await.unwrap();
        assert!(output.contains("hello"));

        server_task.abort();
    }

    #[tokio::test]
    async fn command_mode_send_and_collect() {
        let (mut server, mut client) = UnixStream::pair().unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 256];
            // Read what client sends
            let n = server.read(&mut buf).await.unwrap();
            assert!(n > 0);
            // Echo back
            server.write_all(b"OK\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let output = run_command(
            &mut client,
            Some("AT\r\n"),
            Some(0.3),
            None,
            false,
        )
        .await
        .unwrap();
        assert!(output.contains("OK"));

        server_task.abort();
    }

    #[tokio::test]
    async fn command_mode_wait_for_pattern() {
        let (mut server, mut client) = UnixStream::pair().unwrap();

        let server_task = tokio::spawn(async move {
            server.write_all(b"loading...\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            server.write_all(b"READY\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let start = std::time::Instant::now();
        let output = run_command(
            &mut client,
            None,
            Some(5.0),
            Some("READY"),
            false,
        )
        .await
        .unwrap();

        assert!(output.contains("READY"));
        // Should have completed well before the 5-second timeout
        assert!(start.elapsed() < Duration::from_secs(2));

        server_task.abort();
    }
}
