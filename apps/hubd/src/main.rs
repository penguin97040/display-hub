use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use hub_core::SystemClock;
use hubd::dev::{self, DevState, Fault, Scenario};

/// Display Hub service.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Development mode: plain HTTP on this computer only, sample figures, no authentication.
    /// Never deploy it.
    Dev {
        /// Address to listen on. Only loopback addresses are accepted.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
        #[arg(long, value_enum, default_value_t = Scenario::Demo)]
        scenario: Scenario,
        /// Reproduce a fault, to try a display's error handling.
        #[arg(long, value_enum, default_value_t = Fault::None)]
        fault: Fault,
        /// Poll interval suggested to displays, in seconds (30-3600).
        #[arg(long, default_value_t = 300)]
        refresh: u32,
    },
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("HUBD_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging();
    match cli.command {
        Command::Dev {
            listen,
            scenario,
            fault,
            refresh,
        } => {
            dev::check_bind(listen)?;
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("listening on {listen}"))?;
            tracing::warn!(
                "DEVELOPMENT MODE: plain HTTP, no authentication, sample figures. Loopback only; never deploy."
            );
            tracing::info!(address = %listener.local_addr()?, ?scenario, ?fault, "serving");
            let state = DevState {
                clock: Arc::new(SystemClock),
                scenario,
                fault,
                refresh_after_seconds: refresh,
            };
            hubd::serve_plain(listener, dev::router(state), hubd::shutdown_signal()).await?;
            tracing::info!("stopped");
        }
    }
    Ok(())
}
