use chrono::Local;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Runs the log writer task. Receives serial data via broadcast, timestamps
/// complete lines, and writes them to the log file.
///
/// Partial lines are buffered until a newline is seen.
pub async fn run_log_writer(
    path: std::path::PathBuf,
    mut rx: broadcast::Receiver<Arc<Vec<u8>>>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut line_buf: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                // Flush any remaining partial line
                if !line_buf.is_empty() {
                    let ts = format_timestamp();
                    writer.write_all(ts.as_bytes()).await?;
                    writer.write_all(&line_buf).await?;
                    writer.write_all(b"\n").await?;
                }
                writer.flush().await?;
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(data) => {
                        for &byte in data.as_ref() {
                            if byte == b'\n' {
                                let ts = format_timestamp();
                                writer.write_all(ts.as_bytes()).await?;
                                writer.write_all(&line_buf).await?;
                                writer.write_all(b"\n").await?;
                                line_buf.clear();
                            } else if byte != b'\r' {
                                line_buf.push(byte);
                            }
                        }
                        writer.flush().await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("log writer lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

/// Format a timestamp prefix for log lines.
pub fn format_timestamp() -> String {
    Local::now().format("[%Y-%m-%d %H:%M:%S%.3f] ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format() {
        let ts = format_timestamp();
        // Should match pattern: [YYYY-MM-DD HH:MM:SS.mmm]
        assert!(ts.starts_with('['));
        assert!(ts.ends_with("] "));
        assert!(ts.len() > 20);
    }

    #[tokio::test]
    async fn log_writer_writes_complete_lines() {
        let dir = tempdir("complete");
        let log_path = dir.join("test.log");
        let (tx, rx) = tokio::sync::broadcast::channel::<Arc<Vec<u8>>>(16);
        let cancel = CancellationToken::new();

        let writer_cancel = cancel.clone();
        let writer_path = log_path.clone();
        let handle = tokio::spawn(async move {
            run_log_writer(writer_path, rx, writer_cancel).await
        });

        // Send a complete line
        tx.send(Arc::new(b"hello world\n".to_vec())).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cancel.cancel();
        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("hello world"));
        assert!(content.contains("[20")); // timestamp prefix

        cleanup_dir(&dir);
    }

    #[tokio::test]
    async fn log_writer_buffers_partial_lines() {
        let dir = tempdir("partial");
        let log_path = dir.join("test_partial.log");
        let (tx, rx) = tokio::sync::broadcast::channel::<Arc<Vec<u8>>>(16);
        let cancel = CancellationToken::new();

        let writer_cancel = cancel.clone();
        let writer_path = log_path.clone();
        let handle = tokio::spawn(async move {
            run_log_writer(writer_path, rx, writer_cancel).await
        });

        // Send partial line, then the rest
        tx.send(Arc::new(b"hel".to_vec())).unwrap();
        tx.send(Arc::new(b"lo\n".to_vec())).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cancel.cancel();
        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("hello"));

        cleanup_dir(&dir);
    }

    #[tokio::test]
    async fn log_writer_flushes_partial_on_cancel() {
        let dir = tempdir("flush");
        let log_path = dir.join("test_flush.log");
        let (tx, rx) = tokio::sync::broadcast::channel::<Arc<Vec<u8>>>(16);
        let cancel = CancellationToken::new();

        let writer_cancel = cancel.clone();
        let writer_path = log_path.clone();
        let handle = tokio::spawn(async move {
            run_log_writer(writer_path, rx, writer_cancel).await
        });

        // Send partial line without newline
        tx.send(Arc::new(b"partial data".to_vec())).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cancel.cancel();
        handle.await.unwrap().unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("partial data"));

        cleanup_dir(&dir);
    }

    fn tempdir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rmux_test_{}_{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup_dir(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
