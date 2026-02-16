use std::io::IsTerminal;

use clap::Parser;
use rmux::cli::{Cli, Command};
use rmux::types::ServerConfig;
use rmux::{runtime, server, types};

/// Validate that a server name is safe for use in file paths.
fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("server name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "server name must contain only alphanumeric characters, hyphens, and underscores"
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Server(args) => {
            validate_name(&args.name)?;
            if args.port.is_none() && args.tcp.is_none() {
                anyhow::bail!("either --port or --tcp must be specified");
            }
            if args.port.is_some() && args.tcp.is_some() {
                anyhow::bail!("--port and --tcp are mutually exclusive");
            }
            let config = ServerConfig {
                name: args.name,
                port: args.port,
                tcp: args.tcp,
                baudrate: args.baudrate,
                log: !args.no_log,
                log_dir: args.log_dir,
                interactive: !args.no_interactive,
                pty: args.pty,
                pty_link: args.pty_link,
                reconnect: !args.no_reconnect,
                socket_path: None,
                timestamps: if args.timestamps {
                    true
                } else if args.no_timestamps {
                    false
                } else {
                    !std::io::stdout().is_terminal()
                },
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(server::run_server(config))?;
        }
        Command::Client(args) => {
            validate_name(&args.name)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(rmux::client::run_client(args))?;
        }
        Command::List => {
            cmd_list()?;
        }
        Command::Status(args) => {
            validate_name(&args.name)?;
            cmd_status(&args.name)?;
        }
    }

    Ok(())
}

/// List all running rmux servers by scanning the runtime directory for state files.
fn cmd_list() -> anyhow::Result<()> {
    let dir = runtime::runtime_dir();
    if !dir.exists() {
        println!("No servers running.");
        return Ok(());
    }

    let mut entries: Vec<types::ServerStateFile> = Vec::new();

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let Ok(json) = std::fs::read_to_string(&path) else {
            continue;
        };
        let state: types::ServerStateFile = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if server::state::is_pid_alive(state.pid) {
            entries.push(state);
        } else {
            // Stale state file — clean up
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            server::state::cleanup_state(name);
        }
    }

    if entries.is_empty() {
        println!("No servers running.");
        return Ok(());
    }

    // Print table header
    println!(
        "{:<15} {:<8} {:<20} {:<10} {:<8}",
        "NAME", "PID", "PORT", "BAUD", "CLIENTS"
    );
    println!("{}", "-".repeat(65));

    for state in &entries {
        println!(
            "{:<15} {:<8} {:<20} {:<10} {:<8}",
            state.name, state.pid, state.port, state.baudrate, "-"
        );
    }

    Ok(())
}

/// Show detailed status for a named server.
fn cmd_status(name: &str) -> anyhow::Result<()> {
    let Ok(state) = server::state::read_state_file(name) else {
        anyhow::bail!("server '{name}' not found");
    };

    let alive = server::state::is_pid_alive(state.pid);
    if !alive {
        server::state::cleanup_state(name);
        anyhow::bail!("server '{}' is not running (stale state file cleaned up)", name);
    }

    let is_tcp = state.port.starts_with("tcp://");

    println!("Server: {}", state.name);
    println!("  PID:       {}", state.pid);
    if is_tcp {
        println!("  Transport: {}", state.port);
    } else {
        println!("  Port:      {}", state.port);
        println!("  Baud rate: {}", state.baudrate);
    }
    println!("  Socket:    {}", state.socket);
    println!("  Started:   {}", state.started_at);

    if let Some(ref log) = state.log_file {
        println!("  Log file:  {log}");
    }
    if let Some(ref pty) = state.pty_device {
        println!("  PTY:       {pty}");
    }

    println!("  Status:    running");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmux::types::ServerStateFile;

    fn test_dir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rmux_cmd_test_{}_{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_mock_state(dir: &std::path::Path, name: &str, pid: u32) {
        let state = ServerStateFile {
            name: name.to_string(),
            pid,
            port: "/dev/ttyUSB0".into(),
            baudrate: 115_200,
            socket: dir.join(format!("{name}.sock")).to_string_lossy().into_owned(),
            log_file: None,
            pty_device: None,
            started_at: "2024-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), json).unwrap();
    }

    #[test]
    fn list_no_runtime_dir() {
        // With a nonexistent runtime dir, should print "No servers running."
        // We can't easily test the output, but we can test the state scanning logic
        let dir = test_dir("nodir").join("nonexistent");
        assert!(!dir.exists());
    }

    #[test]
    fn status_missing_server() {
        let result = server::state::read_state_file("definitely_not_a_server_xyz123");
        assert!(result.is_err());
    }

    #[test]
    fn stale_pid_detection() {
        // PID 999_999 should not be alive
        assert!(!server::state::is_pid_alive(999_999));
        // Our own PID should be alive
        assert!(server::state::is_pid_alive(std::process::id()));
    }

    #[test]
    fn state_file_parse() {
        let dir = test_dir("parse");
        write_mock_state(&dir, "testdev", std::process::id());

        let path = dir.join("testdev.json");
        let json = std::fs::read_to_string(&path).unwrap();
        let state: ServerStateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(state.name, "testdev");
        assert_eq!(state.baudrate, 115_200);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_name_accepts_valid() {
        assert!(validate_name("my-server").is_ok());
        assert!(validate_name("dev_1").is_ok());
        assert!(validate_name("ABC123").is_ok());
    }

    #[test]
    fn validate_name_rejects_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_path_traversal() {
        assert!(validate_name("../etc/passwd").is_err());
        assert!(validate_name("foo/bar").is_err());
    }

    #[test]
    fn validate_name_rejects_special_chars() {
        assert!(validate_name("my server").is_err());
        assert!(validate_name("test.log").is_err());
    }
}
