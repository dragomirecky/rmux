use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "rmux", about = "Serial port multiplexer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start a server that owns a serial port
    Server(ServerArgs),

    /// Connect to a running server as a client
    Client(ClientArgs),

    /// List running servers
    List,

    /// Show status of a named server
    Status(StatusArgs),
}

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ServerArgs {
    /// Name for this server instance
    pub name: String,

    /// Serial port device path
    #[arg(short, long)]
    pub port: String,

    /// Baud rate
    #[arg(short, long, default_value_t = 115_200)]
    pub baudrate: u32,

    /// Enable logging to file
    #[arg(long, default_value_t = false)]
    pub log: bool,

    /// Directory for log files (default: runtime dir)
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Run in interactive mode with local console
    #[arg(short, long, default_value_t = false)]
    pub interactive: bool,

    /// Create a virtual serial port (PTY)
    #[arg(long, default_value_t = false)]
    pub pty: bool,

    /// Symlink path for the PTY device
    #[arg(long)]
    pub pty_link: Option<PathBuf>,

    /// Auto-reconnect on serial port errors
    #[arg(long, default_value_t = true)]
    pub reconnect: bool,
}

#[derive(Debug, clap::Args)]
pub struct ClientArgs {
    /// Name of the server to connect to
    pub name: String,

    /// Run in interactive terminal mode (default if TTY)
    #[arg(short, long)]
    pub interactive: Option<bool>,

    /// Show timestamps on output lines
    #[arg(short, long)]
    pub timestamps: Option<bool>,

    /// Send a string and exit
    #[arg(short, long)]
    pub send: Option<String>,

    /// Collect output for N seconds after sending
    #[arg(short, long)]
    pub collect: Option<f64>,

    /// Wait for a regex pattern before exiting
    #[arg(short, long)]
    pub wait_for: Option<String>,

    /// Show last N lines of history on connect
    #[arg(long)]
    pub history: Option<usize>,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Name of the server to query
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_server_minimal() {
        let cli = Cli::parse_from(["rmux", "server", "test1", "-p", "/dev/ttyUSB0"]);
        match cli.command {
            Command::Server(args) => {
                assert_eq!(args.name, "test1");
                assert_eq!(args.port, "/dev/ttyUSB0");
                assert_eq!(args.baudrate, 115_200);
                assert!(!args.log);
                assert!(!args.interactive);
                assert!(!args.pty);
                assert!(args.reconnect);
            }
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn parse_server_all_flags() {
        let cli = Cli::parse_from([
            "rmux",
            "server",
            "dev1",
            "-p",
            "/dev/ttyACM0",
            "-b",
            "9600",
            "--log",
            "--log-dir",
            "/var/log/rmux",
            "-i",
            "--pty",
            "--pty-link",
            "/tmp/vserial",
        ]);
        match cli.command {
            Command::Server(args) => {
                assert_eq!(args.name, "dev1");
                assert_eq!(args.port, "/dev/ttyACM0");
                assert_eq!(args.baudrate, 9600);
                assert!(args.log);
                assert_eq!(
                    args.log_dir,
                    Some(PathBuf::from("/var/log/rmux"))
                );
                assert!(args.interactive);
                assert!(args.pty);
                assert_eq!(
                    args.pty_link,
                    Some(PathBuf::from("/tmp/vserial"))
                );
            }
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn parse_client_interactive() {
        let cli = Cli::parse_from(["rmux", "client", "test1"]);
        match cli.command {
            Command::Client(args) => {
                assert_eq!(args.name, "test1");
                assert!(args.send.is_none());
                assert!(args.collect.is_none());
                assert!(args.wait_for.is_none());
            }
            _ => panic!("expected Client command"),
        }
    }

    #[test]
    fn parse_client_command_mode() {
        let cli = Cli::parse_from([
            "rmux", "client", "dev1", "--send", "AT\r\n", "--collect", "2.0",
            "--wait-for", "OK",
        ]);
        match cli.command {
            Command::Client(args) => {
                assert_eq!(args.name, "dev1");
                assert_eq!(args.send.as_deref(), Some("AT\r\n"));
                assert_eq!(args.collect, Some(2.0));
                assert_eq!(args.wait_for.as_deref(), Some("OK"));
            }
            _ => panic!("expected Client command"),
        }
    }

    #[test]
    fn parse_list() {
        let cli = Cli::parse_from(["rmux", "list"]);
        assert!(matches!(cli.command, Command::List));
    }

    #[test]
    fn parse_status() {
        let cli = Cli::parse_from(["rmux", "status", "mydev"]);
        match cli.command {
            Command::Status(args) => assert_eq!(args.name, "mydev"),
            _ => panic!("expected Status command"),
        }
    }

    #[test]
    fn parse_client_with_history() {
        let cli = Cli::parse_from(["rmux", "client", "test1", "--history", "50"]);
        match cli.command {
            Command::Client(args) => {
                assert_eq!(args.history, Some(50));
            }
            _ => panic!("expected Client command"),
        }
    }
}
