//! DongLoRa USB Multiplexer — share one dongle with multiple applications.
//!
//! Owns the USB serial connection and exposes a Unix domain socket (and
//! optional TCP) that speaks the same COBS-framed protocol. Clients connect
//! and get solicited responses routed, RxPackets broadcast, and StartRx/StopRx
//! reference-counted.

mod daemon;
mod intercept;
mod session;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser)]
#[command(name = "donglora-mux", about = "DongLoRa USB Multiplexer")]
struct Args {
    /// Serial port (auto-detect by USB VID:PID if omitted)
    #[arg(short, long)]
    port: Option<String>,

    /// Unix socket path (default: XDG or /tmp/donglora-mux.sock)
    #[arg(short, long)]
    socket: Option<String>,

    /// TCP listen address [HOST:]PORT (default: 0.0.0.0:5741, "none" to disable)
    #[arg(short, long, default_value = "0.0.0.0:5741")]
    tcp: String,

    /// Enable debug logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Init tracing — daemon-style: wall-clock time, level, message
    let filter = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .init();

    // Resolve serial port
    let port = match args.port {
        Some(p) => p,
        None => tokio::task::spawn_blocking(|| {
            donglora_client::find_port().unwrap_or_else(|| {
                info!("waiting for DongLoRa device...");
                donglora_client::wait_for_device()
            })
        })
        .await
        .map_err(|e| anyhow::anyhow!("device discovery task failed: {e}"))?,
    };

    // Resolve socket path
    let socket_path = args.socket.unwrap_or_else(donglora_client::default_socket_path);

    // Parse TCP address
    let tcp_addr = parse_tcp_addr(&args.tcp);

    // Shutdown token
    let shutdown = CancellationToken::new();

    // Install signal handlers
    let sig_shutdown = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = signal_handler(sig_shutdown).await {
            tracing::error!("signal handler error: {e}");
        }
    });

    // Run the daemon
    let daemon = daemon::MuxDaemon::new(port, socket_path, tcp_addr, shutdown);
    daemon.run().await
}

fn parse_tcp_addr(tcp: &str) -> Option<(String, u16)> {
    if tcp.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Some((host, port_str)) = tcp.rsplit_once(':')
        && let Ok(port) = port_str.parse()
    {
        return Some((host.to_string(), port));
    }
    if let Ok(port) = tcp.parse() {
        return Some(("0.0.0.0".to_string(), port));
    }
    None
}

async fn signal_handler(shutdown: CancellationToken) -> anyhow::Result<()> {
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| anyhow::anyhow!("failed to register SIGINT handler: {e}"))?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| anyhow::anyhow!("failed to register SIGTERM handler: {e}"))?;

    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT"),
        _ = sigterm.recv() => info!("received SIGTERM"),
    }

    shutdown.cancel();
    Ok(())
}
