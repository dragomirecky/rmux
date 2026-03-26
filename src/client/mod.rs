pub mod command;
pub mod interactive;

use std::io::IsTerminal;

use crate::cli::ClientArgs;
use crate::runtime;
use crate::server::state;
use tokio::net::UnixStream;

/// Run the client with the given arguments.
pub async fn run_client(args: ClientArgs) -> anyhow::Result<()> {
    // Validate --since regex early (before connecting)
    let since_pattern = args
        .since
        .as_deref()
        .map(regex::Regex::new)
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid regex for --since: {e}"))?;

    let socket_path = runtime::socket_path(&args.name);
    if !socket_path.exists() {
        anyhow::bail!(
            "server '{}' is not running (socket not found: {})",
            args.name,
            socket_path.display()
        );
    }

    let mut stream = UnixStream::connect(&socket_path).await?;
    tracing::info!("connected to server '{}'", args.name);

    // Compute timestamps flag early so history display can use it
    let timestamps = if args.timestamps {
        true
    } else if args.no_timestamps {
        false
    } else {
        // Auto-detect: timestamps if piped, no timestamps if TTY
        !std::io::stdout().is_terminal()
    };

    // Show history from log file if requested
    let history_requested = args.last.is_some() || since_pattern.is_some();
    if history_requested {
        let log_path = match state::read_state_file(&args.name) {
            Ok(state_file) => state_file.log_file.map(std::path::PathBuf::from),
            Err(e) => {
                eprintln!("history unavailable: could not read state file: {e}");
                None
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
                    crate::log_reader::read_last_lines(path, n)?
                } else if let Some(ref pattern) = since_pattern {
                    crate::log_reader::read_lines_since_pattern(path, pattern)?
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

    // Determine mode
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
