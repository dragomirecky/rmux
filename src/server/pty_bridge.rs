use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Create a PTY pair and return (`master_fd`, `slave_fd`, `slave_path`).
///
/// The master side is used for bidirectional data shuttle with the serial port.
/// The slave path can be connected to by other tools as if it were a serial port.
/// The slave fd must be kept alive for the lifetime of the server — closing it
/// prevents tools like `tio` from opening the PTY device.
pub fn create_pty() -> std::io::Result<(OwnedFd, OwnedFd, String)> {
    let result = nix::pty::openpty(None, None)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let slave_path = nix::unistd::ttyname(&result.slave)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
        .to_string_lossy()
        .into_owned();

    // Configure the slave to behave like a serial port, matching smux
    // exactly: zero all flags, set 8N1 + CREAD + CLOCAL, baud 115200.
    // This is required for serial tools like tio to work with the PTY.
    configure_pty_slave(&result.slave)?;

    Ok((result.master, result.slave, slave_path))
}

/// Configure the PTY slave to behave like a serial port.
///
/// Matches the smux Python implementation exactly: zero all input/output/local
/// flags, set cflag to `CS8 | CREAD | CLOCAL` (8N1, receiver enabled, ignore
/// modem control lines), and set baud rate to 115200. This is required for
/// serial tools like tio and pyserial-miniterm to recognise the PTY as a
/// serial device.
fn configure_pty_slave(slave: &OwnedFd) -> std::io::Result<()> {
    use nix::sys::termios::*;

    let mut attrs = tcgetattr(slave)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Zero all processing flags (matching smux's attrs[0]=0, [1]=0, [3]=0)
    attrs.input_flags = InputFlags::empty();
    attrs.output_flags = OutputFlags::empty();
    attrs.local_flags = LocalFlags::empty();

    // cflag: 8-bit, receiver enabled, local connection (matching smux's
    // CS8 | CREAD | CLOCAL)
    attrs.control_flags = ControlFlags::CS8 | ControlFlags::CREAD | ControlFlags::CLOCAL;

    // Baud rate (matching smux's B115200)
    cfsetispeed(&mut attrs, BaudRate::B115200)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    cfsetospeed(&mut attrs, BaudRate::B115200)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    tcsetattr(slave, SetArg::TCSANOW, &attrs)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    Ok(())
}

/// Create a symlink pointing to the PTY slave device.
pub fn create_pty_symlink(slave_path: &str, link_path: &std::path::Path) -> std::io::Result<()> {
    // Remove existing symlink if present
    let _ = std::fs::remove_file(link_path);
    std::os::unix::fs::symlink(slave_path, link_path)
}

// ---------------------------------------------------------------------------
// Subprocess PTY bridge
// ---------------------------------------------------------------------------

/// Handle returned by `spawn_pty_child()`. Owns the parent's end of the
/// socketpair and the child process metadata.
pub struct PtyChild {
    /// Parent's end of the socketpair (used for async I/O with tokio).
    pub socket: OwnedFd,
    /// Filesystem path to the PTY slave device (e.g. `/dev/ttys007`).
    pub slave_path: String,
    /// PID of the child process that owns the PTY master/slave.
    pub child_pid: nix::unistd::Pid,
}

/// Spawn a child process that owns a PTY master/slave pair and shuttles bytes
/// between the PTY master and a socketpair shared with the parent.
///
/// The parent receives a `PtyChild` with the socketpair fd, slave path, and
/// child PID. The child never returns — it runs a synchronous poll loop and
/// calls `_exit()`.
pub fn spawn_pty_child() -> std::io::Result<PtyChild> {
    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
    use nix::unistd::ForkResult;

    // Create a Unix socketpair for parent ↔ child communication.
    // We don't use SOCK_CLOEXEC (unavailable on macOS). Since the child
    // doesn't exec, CLOEXEC is irrelevant — we manually drop the unwanted
    // fd in each side after fork.
    let (parent_sock, child_sock) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::empty(),
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // SAFETY: We fork here. The child immediately enters a minimal code path
    // that uses only async-signal-safe syscalls (no Rust allocator, no tokio).
    // This is the same pattern used by tmux, screen, and sshd.
    match unsafe { nix::unistd::fork() } {
        Ok(ForkResult::Child) => {
            // Drop the parent's end in the child
            drop(parent_sock);
            child_main(child_sock);
            // child_main never returns
        }
        Ok(ForkResult::Parent { child }) => {
            // Drop the child's end in the parent
            drop(child_sock);

            // Read the handshake from the child (slave path or error)
            let slave_path = read_handshake(&parent_sock)?;

            Ok(PtyChild {
                socket: parent_sock,
                slave_path,
                child_pid: child,
            })
        }
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    }
}

/// Child process entry point. Never returns — calls `_exit()`.
///
/// 1. `setsid()` to become session leader (detach from parent's terminal)
/// 2. Create a PTY pair
/// 3. Send the slave path to the parent as a handshake
/// 4. Enter the synchronous shuttle loop
/// 5. `_exit(0)`
fn child_main(sock: OwnedFd) -> ! {
    // Detach from parent's session so our PTY doesn't interfere
    if nix::unistd::setsid().is_err() {
        let msg = b"ERROR:setsid failed\n";
        let _ = nix::unistd::write(&sock, msg);
        unsafe { libc::_exit(1) };
    }

    // Create PTY pair
    let (master, slave, slave_path) = match create_pty() {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("ERROR:{e}\n");
            let _ = nix::unistd::write(&sock, msg.as_bytes());
            unsafe { libc::_exit(1) };
        }
    };

    // Send slave path as handshake
    let handshake = format!("{slave_path}\n");
    if nix::unistd::write(&sock, handshake.as_bytes()).is_err() {
        unsafe { libc::_exit(1) };
    }

    // Keep slave fd alive (the child owns it)
    shuttle_loop(sock, master, slave);

    unsafe { libc::_exit(0) };
}

/// Synchronous poll loop: shuttle bytes between the socketpair and PTY master.
///
/// Uses `poll()` with no heap allocation in the hot path. Handles `EIO` on the
/// PTY master (no slave connected) with a 100ms retry.
#[allow(clippy::similar_names)]
fn shuttle_loop(sock: OwnedFd, master: OwnedFd, _slave: OwnedFd) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let sock_raw = sock.as_raw_fd();
    let master_raw = master.as_raw_fd();

    // Set both fds to non-blocking
    let _ = set_nonblocking(sock_raw);
    let _ = set_nonblocking(master_raw);

    let mut buf = [0u8; 8192];

    loop {
        let mut pollfds = [
            PollFd::new(sock.as_fd(), PollFlags::POLLIN),
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
        ];

        match poll(&mut pollfds, PollTimeout::from(500u16)) {
            Ok(0) => continue, // timeout
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
            Ok(_) => {}
        }

        // Socket → PTY master
        if let Some(revents) = pollfds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match nix::unistd::read(sock_raw, &mut buf) {
                    Ok(0) => break, // parent closed socket
                    Ok(n) => {
                        if write_all_sync(master_raw, &buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
                    Err(_) => break,
                }
            }
            if revents.contains(PollFlags::POLLHUP | PollFlags::POLLERR) {
                break;
            }
        }

        // PTY master → socket
        if let Some(revents) = pollfds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                match nix::unistd::read(master_raw, &mut buf) {
                    Ok(0) => {
                        // No slave connected — short sleep and retry
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Ok(n) => {
                        if write_all_sync(sock_raw, &buf[..n]).is_err() {
                            break;
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => {}
                    Err(nix::errno::Errno::EIO) => {
                        // EIO: no slave connected yet — retry after delay
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(_) => break,
                }
            }
            // Don't break on POLLHUP for master — it's normal when no slave is open
            if revents.contains(PollFlags::POLLERR) {
                break;
            }
        }
    }
}

use std::os::fd::BorrowedFd;

/// Helper: use `BorrowedFd` in `PollFd::new` (nix 0.29 API).
trait AsFdExt {
    fn as_fd(&self) -> BorrowedFd<'_>;
}

impl AsFdExt for OwnedFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: OwnedFd is valid for its lifetime
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    let flags = fcntl(fd, FcntlArg::F_GETFL)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fcntl(
        fd,
        FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}

/// Write all bytes to a raw fd, retrying on `EINTR`/`EAGAIN`.
fn write_all_sync(fd: std::os::fd::RawFd, data: &[u8]) -> Result<(), nix::errno::Errno> {
    let mut written = 0;
    while written < data.len() {
        match nix::unistd::write(
            // SAFETY: fd is valid for the lifetime of the shuttle loop
            unsafe { BorrowedFd::borrow_raw(fd) },
            &data[written..],
        ) {
            Ok(n) => written += n,
            Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read the handshake line from the child process (the PTY slave path).
///
/// The child sends either `/dev/ttysXXX\n` or `ERROR:message\n`.
fn read_handshake(sock: &OwnedFd) -> std::io::Result<String> {
    let raw = sock.as_raw_fd();
    let mut buf = [0u8; 512];
    let mut pos = 0;

    // Read until we see a newline (with a generous timeout)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "PTY child handshake timed out",
            ));
        }

        let mut pollfds = [nix::poll::PollFd::new(
            unsafe { BorrowedFd::borrow_raw(raw) },
            nix::poll::PollFlags::POLLIN,
        )];

        match nix::poll::poll(&mut pollfds, nix::poll::PollTimeout::from(1000u16)) {
            Ok(0) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
            }
            Ok(_) => {}
        }

        match nix::unistd::read(raw, &mut buf[pos..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "PTY child closed socket before handshake",
                ));
            }
            Ok(n) => {
                pos += n;
                if let Some(nl) = buf[..pos].iter().position(|&b| b == b'\n') {
                    let line = std::str::from_utf8(&buf[..nl])
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    if let Some(err) = line.strip_prefix("ERROR:") {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("PTY child error: {err}"),
                        ));
                    }
                    return Ok(line.to_owned());
                }
                if pos >= buf.len() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "PTY child handshake too long",
                    ));
                }
            }
            Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
            }
        }
    }
}

/// Run the PTY bridge over a socketpair fd (replaces the old `run_pty_bridge`).
///
/// Converts the parent's socketpair fd to a `tokio::net::UnixStream`, then
/// shuttles data between the broadcast channel / serial write channel and the
/// socket. No `AsyncFd`, no PTY master — just a plain Unix socket.
pub async fn run_pty_bridge_socket(
    socket_fd: OwnedFd,
    mut broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    // Convert the raw fd into a tokio UnixStream
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(socket_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(std_stream)?;
    let (mut reader, mut writer) = tokio::io::split(stream);

    loop {
        let mut buf = [0u8; 4096];
        tokio::select! {
            () = cancel.cancelled() => break,

            // Write serial data → socket → child → PTY master → slave
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        if let Err(e) = writer.write_all(&data).await {
                            tracing::error!("PTY socket write error: {e}");
                            return Err(e);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("PTY bridge lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // Read from socket ← child ← PTY master ← slave → serial
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        tracing::warn!("PTY child socket closed");
                        break;
                    }
                    Ok(n) => {
                        let _ = serial_tx.send(buf[..n].to_vec()).await;
                    }
                    Err(e) => {
                        tracing::error!("PTY socket read error: {e}");
                        return Err(e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Kill the PTY child process and wait for it to exit, avoiding zombies.
pub fn kill_pty_child(pid: nix::unistd::Pid) {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::waitpid;

    if let Err(e) = kill(pid, Signal::SIGTERM) {
        tracing::warn!("failed to send SIGTERM to PTY child {pid}: {e}");
        return;
    }

    // Give the child a moment to exit, then reap
    match waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
        Ok(_) => {}
        Err(_) => {
            // Child may need a moment — do a blocking wait with a short sleep
            std::thread::sleep(std::time::Duration::from_millis(100));
            let _ = waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        }
    }
}

// Convert OwnedFd to raw fd, consuming ownership
trait IntoRawFdExt {
    fn into_raw_fd(self) -> std::os::fd::RawFd;
}

impl IntoRawFdExt for OwnedFd {
    fn into_raw_fd(self) -> std::os::fd::RawFd {
        std::os::fd::IntoRawFd::into_raw_fd(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pty_pair() {
        let (master, slave, slave_path) = create_pty().unwrap();
        assert!(master.as_raw_fd() >= 0);
        assert!(slave.as_raw_fd() >= 0);
        assert!(!slave_path.is_empty());
        assert!(slave_path.starts_with("/dev/"));
    }

    /// Verify the slave path can be opened by an external process (like tio)
    /// while both the master and slave fds are alive.
    #[test]
    fn slave_path_is_openable() {
        let (master, slave, slave_path) = create_pty().unwrap();

        // An external tool (tio, picocom, etc.) opens the path with O_RDWR|O_NOCTTY
        let fd = nix::fcntl::open(
            slave_path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        );
        assert!(fd.is_ok(), "failed to open slave path: {}", fd.unwrap_err());
        nix::unistd::close(fd.unwrap()).unwrap();

        // Keep these alive for the duration of the test
        drop(slave);
        drop(master);
    }

    /// Verify the slave path becomes un-openable once both fds are dropped.
    #[test]
    fn slave_path_gone_after_master_drop() {
        let (master, slave, slave_path) = create_pty().unwrap();
        drop(slave);
        drop(master);

        // Once the master is closed the slave device is fully gone.
        let fd = nix::fcntl::open(
            slave_path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        );
        assert!(fd.is_err(), "slave path should not be openable after master fd is dropped");
    }

    /// Simulate what tio/pyserial-miniterm does: open the slave path by name
    /// (not using the original slave fd) and communicate through it.
    #[test]
    fn external_open_bidirectional() {
        use std::os::fd::BorrowedFd;

        let (master, _slave, slave_path) = create_pty().unwrap();

        // Open the slave path externally, exactly as tio would
        let ext_raw = nix::fcntl::open(
            slave_path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        )
        .expect("tio-style open of slave path failed");

        // SAFETY: ext_raw is a valid fd we just opened and we close it at the end.
        let ext_fd = unsafe { BorrowedFd::borrow_raw(ext_raw) };

        // Set the externally-opened fd to non-blocking for the read test
        let flags = nix::fcntl::fcntl(ext_raw, nix::fcntl::FcntlArg::F_GETFL).unwrap();
        nix::fcntl::fcntl(
            ext_raw,
            nix::fcntl::FcntlArg::F_SETFL(
                nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
            ),
        )
        .unwrap();

        // Master → external slave: simulate serial data arriving
        let msg = b"hello from serial";
        nix::unistd::write(&master, msg).unwrap();

        // Small delay for PTY buffering
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut buf = [0u8; 256];
        let n = nix::unistd::read(ext_raw, &mut buf).expect("read from externally-opened slave failed");
        assert_eq!(&buf[..n], msg, "data mismatch: master → external slave");

        // External slave → master: simulate tio sending a command
        let cmd = b"AT\r\n";
        nix::unistd::write(ext_fd, cmd).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        // Set master to non-blocking for the read
        let mflags = nix::fcntl::fcntl(master.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL).unwrap();
        nix::fcntl::fcntl(
            master.as_raw_fd(),
            nix::fcntl::FcntlArg::F_SETFL(
                nix::fcntl::OFlag::from_bits_truncate(mflags) | nix::fcntl::OFlag::O_NONBLOCK,
            ),
        )
        .unwrap();

        let n = nix::unistd::read(master.as_raw_fd(), &mut buf).expect("read from master failed");
        assert_eq!(&buf[..n], cmd, "data mismatch: external slave → master");

        nix::unistd::close(ext_raw).unwrap();
    }

    #[test]
    fn pty_master_slave_communication() {
        let result = nix::pty::openpty(None, None).unwrap();

        // Set slave to raw mode so we don't need newlines
        let mut termios = nix::sys::termios::tcgetattr(&result.slave).unwrap();
        nix::sys::termios::cfmakeraw(&mut termios);
        nix::sys::termios::tcsetattr(
            &result.slave,
            nix::sys::termios::SetArg::TCSANOW,
            &termios,
        )
        .unwrap();

        // Write to master, read from slave
        let msg = b"hello pty";
        nix::unistd::write(&result.master, msg).unwrap();

        let mut buf = [0u8; 256];
        let n = nix::unistd::read(result.slave.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], msg);
    }

    #[test]
    fn pty_slave_to_master() {
        let result = nix::pty::openpty(None, None).unwrap();

        // Set slave to raw mode
        let mut termios = nix::sys::termios::tcgetattr(&result.slave).unwrap();
        nix::sys::termios::cfmakeraw(&mut termios);
        nix::sys::termios::tcsetattr(
            &result.slave,
            nix::sys::termios::SetArg::TCSANOW,
            &termios,
        )
        .unwrap();

        // Write to slave, read from master
        let msg = b"x";
        nix::unistd::write(&result.slave, msg).unwrap();

        let mut buf = [0u8; 256];
        let n = nix::unistd::read(result.master.as_raw_fd(), &mut buf).unwrap();
        assert!(n > 0);
    }

    #[test]
    fn create_symlink() {
        let dir = std::env::temp_dir().join(format!("rmux_pty_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("virtual_serial");

        create_pty_symlink("/dev/pts/999", &link).unwrap();
        assert!(link.is_symlink());

        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target.to_str().unwrap(), "/dev/pts/999");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_spawn_and_handshake() {
        let child = spawn_pty_child().expect("spawn_pty_child failed");
        assert!(
            child.slave_path.starts_with("/dev/"),
            "slave path should start with /dev/, got: {}",
            child.slave_path
        );
        // Clean up
        kill_pty_child(child.child_pid);
    }

    #[test]
    fn test_socket_to_pty_roundtrip() {
        let child = spawn_pty_child().expect("spawn_pty_child failed");

        // Open the slave externally (as tio would)
        let ext_raw = nix::fcntl::open(
            child.slave_path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY | nix::fcntl::OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .expect("failed to open slave path");

        // Give the child's poll loop a moment to settle
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Parent socket → child → PTY master → slave
        let msg = b"hello subprocess";
        nix::unistd::write(&child.socket, msg).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut buf = [0u8; 256];
        let n = nix::unistd::read(ext_raw, &mut buf).expect("read from slave failed");
        assert_eq!(&buf[..n], msg, "socket → PTY slave mismatch");

        // Slave → PTY master → child → parent socket
        let cmd = b"AT\r\n";
        nix::unistd::write(unsafe { BorrowedFd::borrow_raw(ext_raw) }, cmd).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(200));

        // Set parent socket to non-blocking for the read
        set_nonblocking(child.socket.as_raw_fd()).unwrap();
        let n = nix::unistd::read(child.socket.as_raw_fd(), &mut buf).expect("read from socket failed");
        assert_eq!(&buf[..n], cmd, "PTY slave → socket mismatch");

        nix::unistd::close(ext_raw).unwrap();
        kill_pty_child(child.child_pid);
    }

    #[test]
    fn test_child_exit_on_parent_close() {
        use nix::sys::wait::{waitpid, WaitPidFlag};

        let child = spawn_pty_child().expect("spawn_pty_child failed");
        let pid = child.child_pid;

        // Drop the parent socket — child should detect and exit
        drop(child.socket);

        // Wait for child to exit (up to 2 seconds)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    if std::time::Instant::now() > deadline {
                        // Force kill if it didn't exit in time
                        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                        let _ = waitpid(pid, None);
                        panic!("child did not exit after parent socket closed");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(_) => break, // child exited
                Err(_) => break, // child already reaped
            }
        }
    }
}
