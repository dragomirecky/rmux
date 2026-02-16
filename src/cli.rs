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
    pub port: Option<String>,

    /// TCP socket address (host:port)
    #[arg(long)]
    pub tcp: Option<String>,

    /// Baud rate
    #[arg(short, long, default_value_t = 115_200)]
    pub baudrate: u32,

    /// Disable logging to file (logging enabled by default)
    #[arg(long, default_value_t = false)]
    pub no_log: bool,

    /// Directory for log files (default: runtime dir)
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Disable interactive mode (interactive by default)
    #[arg(long, default_value_t = false)]
    pub no_interactive: bool,

    /// Create a virtual serial port (PTY)
    #[arg(long, default_value_t = false)]
    pub pty: bool,

    /// Symlink path for the PTY device
    #[arg(long)]
    pub pty_link: Option<PathBuf>,

    /// Disable auto-reconnect on serial port errors
    #[arg(long, default_value_t = false)]
    pub no_reconnect: bool,

    /// Show timestamps on output lines
    #[arg(short = 'T', long, overrides_with = "no_timestamps")]
    pub timestamps: bool,

    /// Disable timestamps on output lines
    #[arg(long, overrides_with = "timestamps")]
    pub no_timestamps: bool,
}

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct ClientArgs {
    /// Name of the server to connect to
    pub name: String,

    /// Run in interactive terminal mode (default if TTY)
    #[arg(short, long, overrides_with = "no_interactive")]
    pub interactive: bool,

    /// Disable interactive mode
    #[arg(long, overrides_with = "interactive")]
    pub no_interactive: bool,

    /// Show timestamps on output lines
    #[arg(short = 'T', long, overrides_with = "no_timestamps")]
    pub timestamps: bool,

    /// Disable timestamps on output lines
    #[arg(long, overrides_with = "timestamps")]
    pub no_timestamps: bool,

    /// Send a command string and exit
    #[arg(short, long)]
    pub command: Option<String>,

    /// Collect output for N seconds after sending
    #[arg(short, long)]
    pub timeout: Option<f64>,

    /// Wait for a regex pattern before exiting
    #[arg(short, long)]
    pub wait_for: Option<String>,

    /// Show last N lines of history on connect
    #[arg(short = 'n', long)]
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
                assert_eq!(args.port.as_deref(), Some("/dev/ttyUSB0"));
                assert!(args.tcp.is_none());
                assert_eq!(args.baudrate, 115_200);
                assert!(!args.no_log);
                assert!(!args.no_interactive);
                assert!(!args.pty);
                assert!(!args.no_reconnect);
            }
            _ => panic!("expected Server command"),
        }
    }

    #[test]
    fn parse_server_tcp() {
        let cli = Cli::parse_from(["rmux", "server", "test1", "--tcp", "localhost:4321"]);
        match cli.command {
            Command::Server(args) => {
                assert_eq!(args.name, "test1");
                assert!(args.port.is_none());
                assert_eq!(args.tcp.as_deref(), Some("localhost:4321"));
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
            "--no-log",
            "--log-dir",
            "/var/log/rmux",
            "--no-interactive",
            "--pty",
            "--pty-link",
            "/tmp/vserial",
            "--no-reconnect",
        ]);
        match cli.command {
            Command::Server(args) => {
                assert_eq!(args.name, "dev1");
                assert_eq!(args.port.as_deref(), Some("/dev/ttyACM0"));
                assert_eq!(args.baudrate, 9600);
                assert!(args.no_log);
                assert_eq!(
                    args.log_dir,
                    Some(PathBuf::from("/var/log/rmux"))
                );
                assert!(args.no_interactive);
                assert!(args.pty);
                assert_eq!(
                    args.pty_link,
                    Some(PathBuf::from("/tmp/vserial"))
                );
                assert!(args.no_reconnect);
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
                assert!(args.command.is_none());
                assert!(args.timeout.is_none());
                assert!(args.wait_for.is_none());
            }
            _ => panic!("expected Client command"),
        }
    }

    #[test]
    fn parse_client_command_mode() {
        let cli = Cli::parse_from([
            "rmux", "client", "dev1", "--command", "AT\r\n", "--timeout", "2.0",
            "--wait-for", "OK",
        ]);
        match cli.command {
            Command::Client(args) => {
                assert_eq!(args.name, "dev1");
                assert_eq!(args.command.as_deref(), Some("AT\r\n"));
                assert_eq!(args.timeout, Some(2.0));
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

    #[test]
    fn parse_client_short_flags() {
        let cli = Cli::parse_from([
            "rmux", "client", "dev1", "-c", "hello", "-t", "3.0", "-n", "10", "-T",
        ]);
        match cli.command {
            Command::Client(args) => {
                assert_eq!(args.command.as_deref(), Some("hello"));
                assert_eq!(args.timeout, Some(3.0));
                assert_eq!(args.history, Some(10));
                assert!(args.timestamps);
            }
            _ => panic!("expected Client command"),
        }
    }

    #[test]
    fn parse_client_timestamps_override() {
        // --no-timestamps after --timestamps should result in no_timestamps winning
        let cli = Cli::parse_from([
            "rmux", "client", "test1", "--timestamps", "--no-timestamps",
        ]);
        match cli.command {
            Command::Client(args) => {
                assert!(!args.timestamps);
                assert!(args.no_timestamps);
            }
            _ => panic!("expected Client command"),
        }

        // --timestamps after --no-timestamps should result in timestamps winning
        let cli = Cli::parse_from([
            "rmux", "client", "test1", "--no-timestamps", "--timestamps",
        ]);
        match cli.command {
            Command::Client(args) => {
                assert!(args.timestamps);
                assert!(!args.no_timestamps);
            }
            _ => panic!("expected Client command"),
        }
    }
}
