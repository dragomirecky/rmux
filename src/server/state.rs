use crate::runtime;
use crate::types::ServerStateFile;
use std::path::PathBuf;

/// Write the server state file to the runtime directory.
pub fn write_state_file(state: &ServerStateFile) -> std::io::Result<PathBuf> {
    runtime::ensure_runtime_dir()?;
    let path = runtime::state_path(&state.name);
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Read a server state file by name.
pub fn read_state_file(name: &str) -> std::io::Result<ServerStateFile> {
    let path = runtime::state_path(name);
    let json = std::fs::read_to_string(&path)?;
    serde_json::from_str(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Remove the state file and socket file for a server.
pub fn cleanup_state(name: &str) {
    let _ = std::fs::remove_file(runtime::state_path(name));
    let _ = std::fs::remove_file(runtime::socket_path(name));
}

/// Check if a process with the given PID is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    // signal 0 checks process existence without sending a signal
    let Some(pid) = libc::pid_t::try_from(pid).ok() else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state(name: &str) -> ServerStateFile {
        ServerStateFile {
            name: name.to_string(),
            pid: std::process::id(),
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            socket: runtime::socket_path(name).to_string_lossy().into_owned(),
            log_file: None,
            pty_device: None,
            started_at: "2024-01-01T00:00:00Z".into(),
            listen: None,
        }
    }

    #[test]
    fn write_read_state_file() {
        let state = test_state("test_write_read");
        let path = write_state_file(&state).unwrap();
        assert!(path.exists());

        let loaded = read_state_file("test_write_read").unwrap();
        assert_eq!(loaded.name, state.name);
        assert_eq!(loaded.pid, state.pid);
        assert_eq!(loaded.baudrate, 115_200);

        // Cleanup
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cleanup_removes_files() {
        let state = test_state("test_cleanup");
        let path = write_state_file(&state).unwrap();
        assert!(path.exists());

        cleanup_state("test_cleanup");
        assert!(!path.exists());
    }

    #[test]
    fn read_nonexistent_state_file() {
        let result = read_state_file("nonexistent_server_xyz");
        assert!(result.is_err());
    }

    #[test]
    fn pid_alive_self() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_bogus() {
        // PID 999_999 is very unlikely to exist
        assert!(!is_pid_alive(999_999));
    }
}
