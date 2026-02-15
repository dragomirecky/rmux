use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// Create a PTY pair and return (`master_fd`, `slave_path`).
///
/// The master side is used for bidirectional data shuttle with the serial port.
/// The slave path can be connected to by other tools as if it were a serial port.
pub fn create_pty() -> std::io::Result<(OwnedFd, String)> {
    let result = nix::pty::openpty(None, None)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let slave_path = nix::unistd::ttyname(&result.slave)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
        .to_string_lossy()
        .into_owned();

    // Close the slave fd — it will be opened by the connecting process
    drop(result.slave);

    Ok((result.master, slave_path))
}

/// Create a symlink pointing to the PTY slave device.
pub fn create_pty_symlink(slave_path: &str, link_path: &std::path::Path) -> std::io::Result<()> {
    // Remove existing symlink if present
    let _ = std::fs::remove_file(link_path);
    std::os::unix::fs::symlink(slave_path, link_path)
}

/// Run the PTY bridge: shuttle data between the broadcast channel / serial write
/// channel and the PTY master.
///
/// - Serial data (from broadcast) → PTY master (so the slave sees it)
/// - PTY master reads → `serial_tx` (so it goes to the real serial port)
pub async fn run_pty_bridge(
    master_fd: OwnedFd,
    mut broadcast_rx: broadcast::Receiver<Arc<Vec<u8>>>,
    serial_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let async_fd = AsyncFd::new(master_fd)?;

    loop {
        tokio::select! {
            () = cancel.cancelled() => break,

            // Write serial data to PTY master
            result = broadcast_rx.recv() => {
                match result {
                    Ok(data) => {
                        let mut written = 0;
                        while written < data.len() {
                            let mut guard = async_fd.writable().await?;
                            match guard.try_io(|fd| {
                                let n = nix::unistd::write(fd, &data[written..])
                                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                                Ok(n)
                            }) {
                                Ok(Ok(n)) => written += n,
                                Ok(Err(e)) => {
                                    tracing::error!("PTY write error: {e}");
                                    return Err(e);
                                }
                                Err(_would_block) => continue,
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("PTY bridge lagged, missed {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // Read from PTY master → serial
            result = async_fd.readable() => {
                let mut guard = result?;
                let mut buf = [0u8; 4096];
                match guard.try_io(|fd| {
                    let n = nix::unistd::read(fd.as_raw_fd(), &mut buf)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                    Ok(n)
                }) {
                    Ok(Ok(0)) => break, // PTY closed
                    Ok(Ok(n)) => {
                        let _ = serial_tx.send(buf[..n].to_vec()).await;
                    }
                    Ok(Err(e)) => {
                        tracing::error!("PTY read error: {e}");
                        break;
                    }
                    Err(_would_block) => continue,
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pty_pair() {
        let (master, slave_path) = create_pty().unwrap();
        assert!(master.as_raw_fd() >= 0);
        assert!(!slave_path.is_empty());
        assert!(slave_path.starts_with("/dev/"));
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
}
