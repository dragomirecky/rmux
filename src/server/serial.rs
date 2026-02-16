use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc};
use tokio_serial::SerialStream;
use tokio_util::sync::CancellationToken;

/// Maximum backoff duration for serial reconnection.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Initial backoff duration.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Open a serial port with the given settings.
pub fn open_serial(port: &str, baudrate: u32) -> Result<SerialStream, tokio_serial::Error> {
    let builder = tokio_serial::new(port, baudrate);
    SerialStream::open(&builder)
}

/// Read from a generic async reader and broadcast data.
///
/// Returns when the reader hits EOF, errors, or the token is cancelled.
pub async fn run_serial_reader(
    mut reader: impl AsyncRead + Unpin + Send,
    broadcast_tx: broadcast::Sender<Arc<Vec<u8>>>,
    cancel: CancellationToken,
) {
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        tracing::warn!("serial reader EOF");
                        return;
                    }
                    Ok(n) => {
                        let data = Arc::new(buf[..n].to_vec());
                        let _ = broadcast_tx.send(data);
                    }
                    Err(e) => {
                        tracing::error!("serial read error: {e}");
                        return;
                    }
                }
            }
        }
    }
}

/// Serial reader with reconnection logic: opens the port, reads, and reconnects on failure.
pub async fn run_serial_reader_reconnect(
    port: String,
    baudrate: u32,
    broadcast_tx: broadcast::Sender<Arc<Vec<u8>>>,
    reconnect: bool,
    cancel: CancellationToken,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        match open_serial(&port, baudrate) {
            Ok(serial) => {
                tracing::info!("serial port {} opened", port);
                backoff = INITIAL_BACKOFF;
                run_serial_reader(serial, broadcast_tx.clone(), cancel.clone()).await;
            }
            Err(e) => {
                tracing::error!("failed to open serial port {}: {e}", port);
            }
        }

        if !reconnect {
            tracing::info!("reconnect disabled, shutting down serial reader");
            return;
        }

        if cancel.is_cancelled() {
            return;
        }

        tracing::info!("reconnecting in {:?}...", backoff);
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Open a TCP connection to the given address.
pub async fn open_tcp(addr: &str) -> std::io::Result<TcpStream> {
    TcpStream::connect(addr).await
}

/// Manages a TCP connection with reconnection logic for both reader and writer.
///
/// Unlike serial (where reader and writer are independent), TCP reader and writer
/// share the same `TcpStream`. When either side fails, the entire connection must
/// be re-established. This function owns the `mpsc::Receiver` so it survives
/// across reconnections.
pub async fn run_tcp_connection(
    addr: String,
    broadcast_tx: broadcast::Sender<Arc<Vec<u8>>>,
    mut serial_rx: mpsc::Receiver<Vec<u8>>,
    reconnect: bool,
    cancel: CancellationToken,
) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        match open_tcp(&addr).await {
            Ok(stream) => {
                tracing::info!("TCP connected to {}", addr);
                backoff = INITIAL_BACKOFF;
                let (mut read_half, mut write_half) = tokio::io::split(stream);

                // Run reader and writer in a combined select loop so that
                // when either side fails we break out and reconnect both.
                let mut buf = [0u8; 4096];
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return,
                        result = read_half.read(&mut buf) => {
                            match result {
                                Ok(0) => {
                                    tracing::warn!("TCP reader EOF");
                                    break;
                                }
                                Ok(n) => {
                                    let data = Arc::new(buf[..n].to_vec());
                                    let _ = broadcast_tx.send(data);
                                }
                                Err(e) => {
                                    tracing::error!("TCP read error: {e}");
                                    break;
                                }
                            }
                        }
                        msg = serial_rx.recv() => {
                            match msg {
                                Some(data) => {
                                    if let Err(e) = write_half.write_all(&data).await {
                                        tracing::error!("TCP write error: {e}");
                                        break;
                                    }
                                }
                                None => return, // channel closed, server shutting down
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!("failed to connect to TCP {}: {e}", addr);
            }
        }

        if !reconnect {
            tracing::info!("reconnect disabled, shutting down TCP connection");
            return;
        }

        if cancel.is_cancelled() {
            return;
        }

        tracing::info!("reconnecting in {:?}...", backoff);
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Serial writer task: receives data from the mpsc channel and writes to a generic writer.
pub async fn run_serial_writer(
    mut writer: impl AsyncWrite + Unpin + Send,
    mut rx: mpsc::Receiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            msg = rx.recv() => {
                match msg {
                    Some(data) => {
                        if let Err(e) = writer.write_all(&data).await {
                            tracing::error!("serial write error: {e}");
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// Create a PTY pair with raw mode and non-blocking I/O set on both fds.
    fn create_raw_pty() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        let result = nix::pty::openpty(None, None).unwrap();
        let mut termios = nix::sys::termios::tcgetattr(&result.slave).unwrap();
        nix::sys::termios::cfmakeraw(&mut termios);
        nix::sys::termios::tcsetattr(
            &result.slave,
            nix::sys::termios::SetArg::TCSANOW,
            &termios,
        )
        .unwrap();
        // Set non-blocking for AsyncFd
        set_nonblocking(&result.master);
        set_nonblocking(&result.slave);
        (result.master, result.slave)
    }

    fn set_nonblocking(fd: &std::os::fd::OwnedFd) {
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    #[tokio::test]
    async fn serial_reader_broadcasts_data() {
        let (master, slave) = create_raw_pty();

        let cancel = CancellationToken::new();
        let (broadcast_tx, mut broadcast_rx) = broadcast::channel::<Arc<Vec<u8>>>(256);

        // Wrap slave in an AsyncFd for the reader
        let slave_async = tokio::io::unix::AsyncFd::new(slave).unwrap();
        // We need an AsyncRead adapter for the slave fd
        let slave_reader = AsyncFdReader(slave_async);

        let reader_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_serial_reader(slave_reader, broadcast_tx, reader_cancel).await;
        });

        // Write to master (simulates device sending data)
        nix::unistd::write(&master, b"hello from device").unwrap();

        // Should appear on broadcast
        let received = tokio::time::timeout(Duration::from_secs(2), broadcast_rx.recv())
            .await
            .expect("timed out")
            .expect("recv failed");
        assert_eq!(&**received, b"hello from device");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn serial_writer_sends_data() {
        let (master, slave) = create_raw_pty();

        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::channel::<Vec<u8>>(256);

        let slave_async = tokio::io::unix::AsyncFd::new(slave).unwrap();
        let slave_writer = AsyncFdWriter(slave_async);

        let writer_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_serial_writer(slave_writer, rx, writer_cancel).await;
        });

        // Send data through the channel
        tx.send(b"AT\r\n".to_vec()).await.unwrap();

        // Read from master (simulates device receiving data)
        // Give it a moment for async write to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut buf = [0u8; 256];
        let n = nix::unistd::read(master.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], b"AT\r\n");

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// `AsyncRead` adapter for an `AsyncFd<OwnedFd>`.
    struct AsyncFdReader(tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>);

    impl tokio::io::AsyncRead for AsyncFdReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            loop {
                let mut guard = match self.0.poll_read_ready(cx) {
                    std::task::Poll::Ready(Ok(guard)) => guard,
                    std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                };
                let unfilled = buf.initialize_unfilled();
                match guard.try_io(|fd| {
                    let n = nix::unistd::read(fd.as_raw_fd(), unfilled)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                    Ok(n)
                }) {
                    Ok(Ok(n)) => {
                        buf.advance(n);
                        return std::task::Poll::Ready(Ok(()));
                    }
                    Ok(Err(e)) => return std::task::Poll::Ready(Err(e)),
                    Err(_would_block) => continue,
                }
            }
        }
    }

    /// `AsyncWrite` adapter for an `AsyncFd<OwnedFd>`.
    struct AsyncFdWriter(tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>);

    impl tokio::io::AsyncWrite for AsyncFdWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            loop {
                let mut guard = match self.0.poll_write_ready(cx) {
                    std::task::Poll::Ready(Ok(guard)) => guard,
                    std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                };
                match guard.try_io(|fd| {
                    nix::unistd::write(fd, buf)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                }) {
                    Ok(Ok(n)) => return std::task::Poll::Ready(Ok(n)),
                    Ok(Err(e)) => return std::task::Poll::Ready(Err(e)),
                    Err(_would_block) => continue,
                }
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
}
