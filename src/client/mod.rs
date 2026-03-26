pub mod command;
pub mod interactive;

use std::io::IsTerminal;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::cli::ClientArgs;
use crate::runtime;
use crate::server::state;

/// Run the client with the given arguments.
pub async fn run_client(args: ClientArgs) -> anyhow::Result<()> {
    // Validate --since regex early (before connecting)
    let since_pattern = args
        .since
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid regex for --since: {e}"))?;

    // Compute timestamps flag early so history display can use it
    let timestamps = if args.timestamps {
        true
    } else if args.no_timestamps {
        false
    } else {
        // Auto-detect: timestamps if piped, no timestamps if TTY
        !std::io::stdout().is_terminal()
    };

    if let Some(ref remote_addr) = args.remote {
        // TCP remote connection
        let stream = tokio::net::TcpStream::connect(remote_addr)
            .await
            .map_err(|e| anyhow::anyhow!("failed to connect to remote server {remote_addr}: {e}"))?;
        tracing::info!("connected to remote server '{}' at {}", args.name, remote_addr);

        // History is not available for remote connections
        if args.last.is_some() || since_pattern.is_some() {
            eprintln!("history is not available for remote connections");
        }

        run_client_on_stream(stream, &args, timestamps).await
    } else {
        // Local Unix socket connection
        let socket_path = runtime::socket_path(&args.name);
        if !socket_path.exists() {
            anyhow::bail!(
                "server '{}' is not running (socket not found: {})",
                args.name,
                socket_path.display()
            );
        }

        let stream = tokio::net::UnixStream::connect(&socket_path).await?;
        tracing::info!("connected to server '{}'", args.name);

        // Show history from log file if requested
        let history_requested = args.last.is_some() || since_pattern.is_some();
        if history_requested {
            show_history(&args, since_pattern.as_ref(), timestamps);
        }

        run_client_on_stream(stream, &args, timestamps).await
    }
}

/// Show history from the local log file.
fn show_history(
    args: &ClientArgs,
    since_pattern: Option<&regex::Regex>,
    timestamps: bool,
) {
    let log_path = match state::read_state_file(&args.name) {
        Ok(state_file) => state_file.log_file.map(std::path::PathBuf::from),
        Err(e) => {
            eprintln!("history unavailable: could not read state file: {e}");
            return;
        }
    };

    match log_path {
        None => {
            eprintln!("history unavailable: server was started without --log");
        }
        Some(ref path) if !path.exists() => {
            eprintln!(
                "history unavailable: log file not found: {}",
                path.display()
            );
        }
        Some(ref path) => {
            let lines = if let Some(n) = args.last {
                match crate::log_reader::read_last_lines(path, n) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("history error: {e}");
                        return;
                    }
                }
            } else if let Some(pattern) = since_pattern {
                match crate::log_reader::read_lines_since_pattern(path, pattern) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("history error: {e}");
                        return;
                    }
                }
            } else {
                unreachable!()
            };

            if lines.is_empty() && since_pattern.is_some() {
                eprintln!("no lines matched the --since pattern");
            }

            for line in &lines {
                if timestamps {
                    println!("{line}");
                } else {
                    println!("{}", crate::log_reader::strip_timestamp(line));
                }
            }
        }
    }
}

/// Run the client on a connected stream (Unix socket or TCP).
async fn run_client_on_stream(
    stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    args: &ClientArgs,
    timestamps: bool,
) -> anyhow::Result<()> {
    let is_command_mode =
        args.command.is_some() || args.timeout.is_some() || args.wait_for.is_some();
    let is_interactive = if args.interactive {
        true
    } else if args.no_interactive {
        false
    } else {
        !is_command_mode && std::io::stdout().is_terminal()
    };

    if is_interactive {
        interactive::run_interactive_socket(stream, timestamps).await?;
    } else {
        let mut stream = stream;
        let output = command::run_command(
            &mut stream,
            args.command.as_deref(),
            args.timeout,
            args.wait_for.as_deref(),
            timestamps,
        )
        .await?;
        print!("{output}");
    }

    Ok(())
}
