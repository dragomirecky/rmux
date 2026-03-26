use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for a server instance, derived from CLI arguments.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ServerConfig {
    pub name: String,
    pub port: Option<String>,
    pub tcp: Option<String>,
    pub baudrate: u32,
    pub log: bool,
    pub log_dir: Option<PathBuf>,
    pub interactive: bool,
    pub pty: bool,
    pub pty_link: Option<PathBuf>,
    pub reconnect: bool,
    pub socket_path: Option<PathBuf>,
    pub timestamps: bool,
    pub listen: Option<String>,
}

/// Persisted state file written by a running server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStateFile {
    pub name: String,
    pub pid: u32,
    pub port: String,
    pub baudrate: u32,
    pub socket: String,
    pub log_file: Option<String>,
    pub pty_device: Option<String>,
    pub started_at: String,
    pub listen: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_backward_compat_no_listen() {
        // Old JSON without the "listen" field should deserialize with listen = None
        let json = r#"{
            "name": "test",
            "pid": 1234,
            "port": "/dev/ttyUSB0",
            "baudrate": 115200,
            "socket": "/tmp/rmux/test.sock",
            "log_file": null,
            "pty_device": null,
            "started_at": "2024-01-01T00:00:00Z"
        }"#;
        let state: ServerStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(state.name, "test");
        assert!(state.listen.is_none());
    }

    #[test]
    fn state_file_with_listen() {
        let json = r#"{
            "name": "test",
            "pid": 1234,
            "port": "/dev/ttyUSB0",
            "baudrate": 115200,
            "socket": "/tmp/rmux/test.sock",
            "log_file": null,
            "pty_device": null,
            "started_at": "2024-01-01T00:00:00Z",
            "listen": "0.0.0.0:5555"
        }"#;
        let state: ServerStateFile = serde_json::from_str(json).unwrap();
        assert_eq!(state.listen.as_deref(), Some("0.0.0.0:5555"));
    }
}
