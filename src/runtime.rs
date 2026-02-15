use std::path::PathBuf;

/// Returns the runtime directory for rmux: `$XDG_RUNTIME_DIR/rmux` or `/tmp/rmux-$UID`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("rmux")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/rmux-{uid}"))
    }
}

/// Path to the Unix socket for a given server name.
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

/// Path to the state file for a given server name.
pub fn state_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.json"))
}

/// Path to the log file for a given server name.
pub fn log_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.log"))
}

/// Ensure the runtime directory exists.
pub fn ensure_runtime_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(runtime_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_contains_name() {
        let path = socket_path("test");
        assert!(path.to_str().unwrap().contains("test.sock"));
    }

    #[test]
    fn state_path_contains_name() {
        let path = state_path("myserver");
        assert!(path.to_str().unwrap().contains("myserver.json"));
    }

    #[test]
    fn log_path_contains_name() {
        let path = log_path("dev1");
        assert!(path.to_str().unwrap().contains("dev1.log"));
    }

    #[test]
    fn all_paths_share_parent() {
        let sock = socket_path("x");
        let state = state_path("x");
        let log = log_path("x");
        assert_eq!(sock.parent(), state.parent());
        assert_eq!(state.parent(), log.parent());
    }

    #[test]
    fn runtime_dir_is_absolute() {
        let dir = runtime_dir();
        assert!(dir.is_absolute());
    }
}
