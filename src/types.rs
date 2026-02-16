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
}
