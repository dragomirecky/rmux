use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmux::protocol::{
    encode_control, encode_data, ControlMessage, MessageKind, ParseEvent, Parser,
};
use rmux::server::broadcaster;
use rmux::server::client_handler::{self, ClientContext};
use rmux::server::serial;

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// AsyncFd I/O adapters for PTY file descriptors
// ---------------------------------------------------------------------------

/// `AsyncRead` adapter for an `AsyncFd<OwnedFd>`.
pub struct AsyncFdReader(pub AsyncFd<OwnedFd>);

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
pub struct AsyncFdWriter(pub AsyncFd<OwnedFd>);

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

fn set_nonblocking(fd: &OwnedFd) {
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

// ---------------------------------------------------------------------------
// MockSerial — uses a PTY pair to simulate a serial device
// ---------------------------------------------------------------------------

pub struct MockSerial {
    master: AsyncFd<OwnedFd>,
}

impl MockSerial {
    /// Create a new mock serial device. Returns `(mock, slave_reader, slave_writer)`.
    ///
    /// The `MockSerial` holds the master side for injecting/capturing data.
    /// The slave reader/writer are passed to the server's serial tasks.
    pub fn new() -> std::io::Result<(Self, AsyncFdReader, AsyncFdWriter)> {
        let result = nix::pty::openpty(None, None)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Set both sides to raw mode
        let mut termios = nix::sys::termios::tcgetattr(&result.slave)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        nix::sys::termios::cfmakeraw(&mut termios);
        nix::sys::termios::tcsetattr(
            &result.slave,
            nix::sys::termios::SetArg::TCSANOW,
            &termios,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Set all fds to non-blocking for AsyncFd
        set_nonblocking(&result.master);
        set_nonblocking(&result.slave);

        // Duplicate the slave fd so we can create both a reader and writer
        let slave_fd2 = nix::unistd::dup(result.slave.as_raw_fd())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let slave_fd2 = unsafe { OwnedFd::from_raw_fd(slave_fd2) };
        set_nonblocking(&slave_fd2);

        let master = AsyncFd::new(result.master)?;
        let slave_reader = AsyncFdReader(AsyncFd::new(result.slave)?);
        let slave_writer = AsyncFdWriter(AsyncFd::new(slave_fd2)?);

        Ok((MockSerial { master }, slave_reader, slave_writer))
    }

    /// Write data to the master side (simulates device sending data to the server).
    pub async fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let mut guard = self.master.writable().await?;
            match guard.try_io(|fd| {
                nix::unistd::write(fd, &data[written..])
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            }) {
                Ok(Ok(n)) => written += n,
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
        Ok(())
    }

    /// Read data from the master side (captures what the server sent to the device).
    pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            match guard.try_io(|fd| {
                nix::unistd::read(fd.as_raw_fd(), buf)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            }) {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }
}

use std::os::fd::FromRawFd;

// ---------------------------------------------------------------------------
// TestServer — assembles server pipeline for integration testing
// ---------------------------------------------------------------------------

pub struct TestServer {
    pub socket_path: PathBuf,
    pub mock_serial: Arc<MockSerial>,
    pub log_path: Option<PathBuf>,
    cancel: CancellationToken,
    tmp_dir: PathBuf,
}

impl TestServer {
    /// Start a test server backed by a mock serial device.
    ///
    /// This assembles the server pipeline manually (broadcast, serial reader/writer,
    /// client acceptor) using generic I/O with PTY pairs, bypassing `open_serial()`.
    pub async fn start(name: &str) -> Self {
        let (mock_serial, slave_reader, slave_writer) =
            MockSerial::new().expect("failed to create mock serial");
        let mock_serial = Arc::new(mock_serial);

        let tmp_dir = std::env::temp_dir().join(format!(
            "rmux_integ_{}_{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let socket_path = tmp_dir.join(format!("{name}.sock"));

        let cancel = CancellationToken::new();

        // Set up channels
        let (broadcast_tx, _broadcast_rx) = broadcaster::new_broadcast();
        let (serial_tx, serial_rx) = mpsc::channel::<Vec<u8>>(256);

        // Spawn serial reader (slave reader → broadcast)
        let reader_cancel = cancel.clone();
        let reader_broadcast = broadcast_tx.clone();
        tokio::spawn(async move {
            serial::run_serial_reader(slave_reader, reader_broadcast, reader_cancel).await;
        });

        // Spawn serial writer (mpsc → slave writer)
        let writer_cancel = cancel.clone();
        tokio::spawn(async move {
            serial::run_serial_writer(slave_writer, serial_rx, writer_cancel).await;
        });

        // Bind Unix socket
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Shared client context
        let ctx = Arc::new(ClientContext {
            port: "/dev/mock".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });

        // Client acceptor loop
        let acceptor_cancel = cancel.clone();
        let acceptor_cancel2 = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = acceptor_cancel.cancelled() => break,
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _addr)) => {
                                let client_broadcast = broadcast_tx.clone();
                                let client_serial = serial_tx.clone();
                                let client_ctx = ctx.clone();
                                let client_cancel = acceptor_cancel2.clone();
                                tokio::spawn(async move {
                                    client_handler::handle_client(
                                        stream,
                                        client_broadcast,
                                        client_serial,
                                        client_ctx,
                                        client_cancel,
                                    ).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        // Wait for socket to be connectable
        for _ in 0..100 {
            if UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        TestServer {
            socket_path,
            mock_serial,
            log_path: None,
            cancel,
            tmp_dir,
        }
    }

    /// Start a test server with logging enabled.
    ///
    /// The log writer subscribes to the broadcast channel so serial data
    /// is written to a log file in the temp directory.
    pub async fn start_with_logging(name: &str) -> Self {
        let (mock_serial, slave_reader, slave_writer) =
            MockSerial::new().expect("failed to create mock serial");
        let mock_serial = Arc::new(mock_serial);

        let tmp_dir = std::env::temp_dir().join(format!(
            "rmux_integ_{}_{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let socket_path = tmp_dir.join(format!("{name}.sock"));
        let log_path = tmp_dir.join(format!("{name}.log"));

        let cancel = CancellationToken::new();

        // Set up channels
        let (broadcast_tx, _broadcast_rx) = broadcaster::new_broadcast();
        let (serial_tx, serial_rx) = mpsc::channel::<Vec<u8>>(256);

        // Spawn serial reader (slave reader → broadcast)
        let reader_cancel = cancel.clone();
        let reader_broadcast = broadcast_tx.clone();
        tokio::spawn(async move {
            serial::run_serial_reader(slave_reader, reader_broadcast, reader_cancel).await;
        });

        // Spawn serial writer (mpsc → slave writer)
        let writer_cancel = cancel.clone();
        tokio::spawn(async move {
            serial::run_serial_writer(slave_writer, serial_rx, writer_cancel).await;
        });

        // Spawn log writer
        let log_rx = broadcast_tx.subscribe();
        let log_cancel = cancel.clone();
        let log_path_clone = log_path.clone();
        tokio::spawn(async move {
            if let Err(e) =
                rmux::server::log_writer::run_log_writer(log_path_clone, log_rx, log_cancel).await
            {
                tracing::error!("log writer error: {e}");
            }
        });

        // Bind Unix socket
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Shared client context
        let ctx = Arc::new(ClientContext {
            port: "/dev/mock".into(),
            baudrate: 115_200,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });

        // Client acceptor loop
        let acceptor_cancel = cancel.clone();
        let acceptor_cancel2 = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = acceptor_cancel.cancelled() => break,
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _addr)) => {
                                let client_broadcast = broadcast_tx.clone();
                                let client_serial = serial_tx.clone();
                                let client_ctx = ctx.clone();
                                let client_cancel = acceptor_cancel2.clone();
                                tokio::spawn(async move {
                                    client_handler::handle_client(
                                        stream,
                                        client_broadcast,
                                        client_serial,
                                        client_ctx,
                                        client_cancel,
                                    ).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        // Wait for socket to be connectable
        for _ in 0..100 {
            if UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        TestServer {
            socket_path,
            mock_serial,
            log_path: Some(log_path),
            cancel,
            tmp_dir,
        }
    }

    /// Stop the server and clean up temp files.
    pub async fn stop(self) {
        self.cancel.cancel();
        // Give tasks time to wind down
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
    }

    /// Stop the server but keep temp files (for reading logs after shutdown).
    /// Returns the temp directory path so the caller can clean up later.
    pub async fn stop_keep_files(self) -> PathBuf {
        self.cancel.cancel();
        // Give tasks time to wind down (log writer flushes on cancel)
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.tmp_dir
    }
}

// ---------------------------------------------------------------------------
// TcpTestServer — uses a TCP listener as the mock backend
// ---------------------------------------------------------------------------

/// A mock TCP backend that simulates a device (e.g. Renode) exposed over TCP.
/// Holds the accepted `TcpStream` for reading/writing data.
pub struct MockTcpDevice {
    pub stream: TcpStream,
}

impl MockTcpDevice {
    pub async fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(data).await
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf).await
    }
}

/// A test server that uses TCP transport instead of serial.
/// The `tcp_listener_addr` can be used to restart the mock backend.
pub struct TcpTestServer {
    pub socket_path: PathBuf,
    pub tcp_listener_addr: std::net::SocketAddr,
    cancel: CancellationToken,
    tmp_dir: PathBuf,
}

impl TcpTestServer {
    /// Start a test server backed by a TCP connection.
    ///
    /// Returns `(TcpTestServer, MockTcpDevice)`. The mock device is the accepted
    /// TCP connection that the server connected to.
    pub async fn start(name: &str) -> (Self, MockTcpDevice) {
        // Bind a TCP listener on a random port
        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();

        let tmp_dir = std::env::temp_dir().join(format!(
            "rmux_tcp_integ_{}_{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let socket_path = tmp_dir.join(format!("{name}.sock"));

        let cancel = CancellationToken::new();

        // Set up channels
        let (broadcast_tx, _broadcast_rx) = broadcaster::new_broadcast();
        let (serial_tx, serial_rx) = mpsc::channel::<Vec<u8>>(256);

        // Spawn TCP connection task (will connect to our listener)
        let tcp_cancel = cancel.clone();
        let tcp_broadcast = broadcast_tx.clone();
        let addr_str = tcp_addr.to_string();
        tokio::spawn(async move {
            serial::run_tcp_connection(
                addr_str,
                tcp_broadcast,
                serial_rx,
                true, // reconnect enabled
                tcp_cancel,
            )
            .await;
        });

        // Accept the connection from the server
        let (tcp_stream, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tcp_listener.accept(),
        )
        .await
        .expect("timed out waiting for TCP connect")
        .expect("TCP accept failed");

        let mock_device = MockTcpDevice { stream: tcp_stream };

        // Give the connection a moment to stabilize
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Bind Unix socket for clients
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).unwrap();

        let ctx = Arc::new(ClientContext {
            port: format!("tcp://{tcp_addr}"),
            baudrate: 0,
            serial_connected: true,
            client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });

        // Client acceptor loop
        let acceptor_cancel = cancel.clone();
        let acceptor_cancel2 = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = acceptor_cancel.cancelled() => break,
                    result = listener.accept() => {
                        match result {
                            Ok((stream, _addr)) => {
                                let client_broadcast = broadcast_tx.clone();
                                let client_serial = serial_tx.clone();
                                let client_ctx = ctx.clone();
                                let client_cancel = acceptor_cancel2.clone();
                                tokio::spawn(async move {
                                    client_handler::handle_client(
                                        stream,
                                        client_broadcast,
                                        client_serial,
                                        client_ctx,
                                        client_cancel,
                                    ).await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        // Wait for socket to be connectable
        for _ in 0..100 {
            if UnixStream::connect(&socket_path).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        (
            TcpTestServer {
                socket_path,
                tcp_listener_addr: tcp_addr,
                cancel,
                tmp_dir,
            },
            mock_device,
        )
    }

    /// Accept a new TCP connection from the server after a reconnect.
    /// Binds a new listener on the same address and waits for the server to connect.
    pub async fn accept_reconnect(&self, timeout: Duration) -> MockTcpDevice {
        let tcp_listener = TcpListener::bind(self.tcp_listener_addr).await.unwrap();
        let (tcp_stream, _) = tokio::time::timeout(timeout, tcp_listener.accept())
            .await
            .expect("timed out waiting for TCP reconnect")
            .expect("TCP accept failed");
        MockTcpDevice { stream: tcp_stream }
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
    }
}

// ---------------------------------------------------------------------------
// TestClient — wraps a Unix socket connection to the server
// ---------------------------------------------------------------------------

pub struct TestClient {
    stream: UnixStream,
    parser: Parser,
}

impl TestClient {
    /// Connect to a test server.
    pub async fn connect(socket_path: &PathBuf) -> Self {
        let stream = UnixStream::connect(socket_path)
            .await
            .expect("failed to connect to test server");
        TestClient {
            stream,
            parser: Parser::new(),
        }
    }

    /// Read and decode data bytes from the server (timeout-protected).
    /// Returns decoded data bytes only (filters out messages).
    pub async fn read_data(&mut self, timeout: Duration) -> Vec<u8> {
        let mut buf = [0u8; 8192];
        let mut decoded = Vec::new();

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    let events = self.parser.feed_bytes(&buf[..n]);
                    for event in events {
                        if let ParseEvent::DataByte(b) = event {
                            decoded.push(b);
                        }
                    }
                    if !decoded.is_empty() {
                        // Give a tiny bit more time for any remaining bytes in flight
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        // Drain anything else available
                        match tokio::time::timeout(Duration::from_millis(100), self.stream.read(&mut buf)).await {
                            Ok(Ok(n)) if n > 0 => {
                                let events = self.parser.feed_bytes(&buf[..n]);
                                for event in events {
                                    if let ParseEvent::DataByte(b) = event {
                                        decoded.push(b);
                                    }
                                }
                            }
                            _ => {}
                        }
                        break;
                    }
                }
                _ => break,
            }
        }
        decoded
    }

    /// Send protocol-encoded data to the server.
    pub async fn send_data(&mut self, data: &[u8]) {
        let encoded = encode_data(data);
        self.stream.write_all(&encoded).await.expect("send_data failed");
    }

    /// Send a protocol-encoded control message to the server.
    pub async fn send_control(&mut self, msg: &ControlMessage) {
        let encoded = encode_control(msg);
        self.stream.write_all(&encoded).await.expect("send_control failed");
    }

    /// Read until a response message is received (timeout-protected).
    /// Returns the parsed JSON string from the response.
    pub async fn read_response(&mut self, timeout: Duration) -> Option<String> {
        let mut buf = [0u8; 8192];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, self.stream.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    let events = self.parser.feed_bytes(&buf[..n]);
                    for event in events {
                        if let ParseEvent::Message {
                            kind: MessageKind::Response,
                            json,
                        } = event
                        {
                            return Some(json);
                        }
                    }
                }
                _ => return None,
            }
        }
    }
}
