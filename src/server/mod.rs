pub mod broadcaster;
pub mod client_handler;
pub mod console;
pub mod log_writer;
pub mod pty_bridge;
pub mod serial;
pub mod state;

use crate::types::{ServerConfig, ServerStateFile};
use client_handler::ClientContext;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Run the server with the given configuration.
#[allow(clippy::too_many_lines)]
pub async fn run_server(config: ServerConfig) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();

    // Set up signal handler
    let signal_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received Ctrl+C, shutting down...");
        signal_cancel.cancel();
    });

    // Ensure runtime directory exists
    crate::runtime::ensure_runtime_dir()?;

    // Set up channels
    let (broadcast_tx, _broadcast_rx) = broadcaster::new_broadcast();
    let (serial_tx, serial_rx) = mpsc::channel::<Vec<u8>>(256);

    // Determine log path
    let log_path = if config.log {
        let dir = config.log_dir.as_deref();
        if let Some(dir) = dir {
            std::fs::create_dir_all(dir)?;
            Some(dir.join(format!("{}.log", config.name)))
        } else {
            Some(crate::runtime::log_path(&config.name))
        }
    } else {
        None
    };

    // Set up PTY if requested. Spawns a child process that owns the PTY
    // master/slave and communicates with the parent via a socketpair.
    let pty_child = if config.pty {
        let child = pty_bridge::spawn_pty_child()?;
        tracing::info!("PTY slave: {} (child pid {})", child.slave_path, child.child_pid);

        // Always create a stable symlink in the runtime dir (like smux)
        let default_link = crate::runtime::pty_path(&config.name);
        pty_bridge::create_pty_symlink(&child.slave_path, &default_link)?;
        eprintln!("PTY: {}", default_link.display());
        tracing::info!("PTY symlink: {}", default_link.display());

        // Create an additional user-specified symlink if requested
        if let Some(ref link) = config.pty_link {
            pty_bridge::create_pty_symlink(&child.slave_path, link)?;
            eprintln!("PTY symlink: {}", link.display());
            tracing::info!("PTY extra symlink: {}", link.display());
        }

        Some(child)
    } else {
        None
    };

    // Determine socket path: use override from config or the default runtime path
    let socket_path = config
        .socket_path
        .clone()
        .unwrap_or_else(|| crate::runtime::socket_path(&config.name));

    // Write state file
    let state = ServerStateFile {
        name: config.name.clone(),
        pid: std::process::id(),
        port: config.port.clone(),
        baudrate: config.baudrate,
        socket: socket_path.to_string_lossy().into_owned(),
        log_file: log_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
        pty_device: pty_child.as_ref().map(|c| c.slave_path.clone()),
        started_at: chrono::Local::now().to_rfc3339(),
    };
    state::write_state_file(&state)?;

    // Clean up old socket if it exists
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!("listening on {}", socket_path.display());

    // Shared client context
    let ctx = Arc::new(ClientContext {
        port: config.port.clone(),
        baudrate: config.baudrate,
        serial_connected: true,
        client_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });

    // Open serial port once and split for reader/writer
    let serial_cancel = cancel.clone();
    let serial_broadcast_tx = broadcast_tx.clone();
    let serial_port = config.port.clone();
    let serial_baudrate = config.baudrate;
    let serial_reconnect = config.reconnect;

    match serial::open_serial(&serial_port, serial_baudrate) {
        Ok(serial_stream) => {
            let (read_half, write_half) = tokio::io::split(serial_stream);

            // Spawn serial reader
            let reader_cancel = serial_cancel.clone();
            tokio::spawn(async move {
                serial::run_serial_reader(read_half, serial_broadcast_tx, reader_cancel).await;
            });

            // Spawn serial writer
            let writer_cancel = serial_cancel;
            tokio::spawn(async move {
                serial::run_serial_writer(write_half, serial_rx, writer_cancel).await;
            });
        }
        Err(e) => {
            tracing::error!("failed to open serial port {}: {e}", serial_port);
            if serial_reconnect {
                // Fall back to reconnection mode for the reader
                tokio::spawn(async move {
                    serial::run_serial_reader_reconnect(
                        serial_port,
                        serial_baudrate,
                        serial_broadcast_tx,
                        serial_reconnect,
                        serial_cancel,
                    )
                    .await;
                });
                // Keep serial_rx alive so senders don't error
                let writer_cancel = cancel.clone();
                tokio::spawn(async move {
                    drop(serial_rx);
                    writer_cancel.cancelled().await;
                });
            } else {
                tracing::warn!("serial port unavailable, client writes disabled");
                let writer_cancel = cancel.clone();
                tokio::spawn(async move {
                    drop(serial_rx);
                    writer_cancel.cancelled().await;
                });
            }
        }
    }

    // Spawn log writer if enabled
    if let Some(ref lp) = log_path {
        let log_rx = broadcast_tx.subscribe();
        let log_cancel = cancel.clone();
        let lp = lp.clone();
        tokio::spawn(async move {
            if let Err(e) = log_writer::run_log_writer(lp, log_rx, log_cancel).await {
                tracing::error!("log writer error: {e}");
            }
        });
    }

    // Spawn PTY bridge if enabled. The child process owns the PTY master/slave;
    // we only need to shuttle data over the socketpair.
    let pty_child_pid = if let Some(child) = pty_child {
        let pid = child.child_pid;
        let pty_rx = broadcast_tx.subscribe();
        let pty_serial_tx = serial_tx.clone();
        let pty_cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                pty_bridge::run_pty_bridge_socket(child.socket, pty_rx, pty_serial_tx, pty_cancel)
                    .await
            {
                tracing::error!("PTY bridge error: {e}");
            }
        });
        Some(pid)
    } else {
        None
    };

    // Spawn interactive console if requested
    if config.interactive {
        let console_rx = broadcast_tx.subscribe();
        let console_serial_tx = serial_tx.clone();
        let console_cancel = cancel.clone();
        tokio::spawn(async move {
            if let Err(e) =
                console::run_console(console_rx, console_serial_tx, console_cancel).await
            {
                tracing::error!("console error: {e}");
            }
        });
    }

    // Client acceptor loop
    let acceptor_cancel = cancel.clone();
    loop {
        tokio::select! {
            () = acceptor_cancel.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        let client_broadcast = broadcast_tx.clone();
                        let client_serial = serial_tx.clone();
                        let client_ctx = ctx.clone();
                        let client_cancel = cancel.clone();
                        tokio::spawn(async move {
                            client_handler::handle_client(
                                stream,
                                client_broadcast,
                                client_serial,
                                client_ctx,
                                client_cancel,
                            ).await;
                        });
                    }
                    Err(e) => {
                        tracing::error!("accept error: {e}");
                    }
                }
            }
        }
    }

    // Cleanup
    tracing::info!("server shutting down");
    if let Some(pid) = pty_child_pid {
        tracing::info!("terminating PTY child process {pid}");
        pty_bridge::kill_pty_child(pid);
    }
    if config.pty {
        let _ = std::fs::remove_file(crate::runtime::pty_path(&config.name));
    }
    if let Some(ref link) = config.pty_link {
        let _ = std::fs::remove_file(link);
    }
    state::cleanup_state(&state.name);
    Ok(())
}
