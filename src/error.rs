use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)] // Variants defined for completeness; not all constructed yet
pub enum RmuxError {
    #[error("serial port error: {0}")]
    Serial(#[from] tokio_serial::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("server '{name}' is not running")]
    ServerNotRunning { name: String },

    #[error("server '{name}' is already running (pid {pid})")]
    ServerAlreadyRunning { name: String, pid: u32 },

    #[error("state file not found: {0}")]
    StateFileNotFound(PathBuf),

    #[error("socket not found: {0}")]
    SocketNotFound(PathBuf),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("connection closed")]
    ConnectionClosed,

    #[error("{0}")]
    Other(String),
}
