use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use hub_core::{Clock, SystemClock};
use hub_protocol::pairing::{fingerprint, PairingString};
use hubd::config::Config;
use hubd::dev::{self, DevState, Fault, Scenario};
use hubd::secret::Role;

const DEFAULT_CONFIG: &str = "/etc/display-hub/hubd.toml";

/// Display Hub service.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct Settings {
    /// Configuration file. Defaults to /etc/display-hub/hubd.toml if it exists, otherwise built-in defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Overrides `data_dir` from the configuration.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Overrides `listen` from the configuration.
    #[arg(long, global = true)]
    listen: Option<SocketAddr>,
}

impl Settings {
    fn load(&self) -> anyhow::Result<Config> {
        let mut config = match &self.config {
            Some(p) => Config::load(p)?,
            None if std::path::Path::new(DEFAULT_CONFIG).exists() => Config::load(DEFAULT_CONFIG.as_ref())?,
            None => Config::default(),
        };
        if let Some(d) = &self.data_dir {
            config.data_dir = d.clone();
        }
        if let Some(l) = self.listen {
            config.listen = l;
        }
        config.check().map_err(anyhow::Error::msg)?;
        Ok(config)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub: HTTPS for displays and the desktop manager on the local network.
    Serve {
        #[command(flatten)]
        settings: Settings,
    },
    /// Print a one-time pairing string for the desktop manager (valid 10 minutes).
    PairAdmin {
        #[command(flatten)]
        settings: Settings,
        /// Host name or address the manager should use, if not this machine's `.local` name.
        #[arg(long)]
        host: Option<String>,
        /// A name for this admin in `hubd admin list`.
        #[arg(long, default_value = "Desktop manager")]
        label: String,
    },
    /// List or revoke admin access.
    Admin {
        #[command(subcommand)]
        action: AdminAction,
        #[command(flatten)]
        settings: Settings,
    },
    /// Print the fingerprint of the hub's TLS key, to compare with what the manager shows.
    Fingerprint {
        #[command(flatten)]
        settings: Settings,
    },
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

#[derive(Subcommand)]
enum AdminAction {
    /// List every admin that has paired, and whether it is still allowed.
    List,
    /// Revoke one admin, by the id `list` shows.
    Revoke { id: String },
    /// Revoke every admin. Pair again afterwards with `pair-admin`.
    RevokeAll,
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
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    match cli.command {
        Command::Serve { settings } => {
            let config = settings.load()?;
            let listen = config.listen;
            let hub = hubd::open_hub(config, clock)?;
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("listening on {listen}"))?;
            tracing::info!(
                address = %listener.local_addr()?,
                fingerprint = %fingerprint(&hub.identity.spki_sha256),
                "serving HTTPS on the local network"
            );
            if hubd::api::clock_synchronised() == Some(false) {
                tracing::warn!(
                    "the system clock isn't synchronised yet; ages and reset times may be wrong until it is"
                );
            }
            hubd::serve_https(hub, listener, hubd::shutdown_signal()).await?;
            tracing::info!("stopped");
        }
        Command::PairAdmin {
            settings,
            host,
            label,
        } => {
            let config = settings.load()?;
            let port = config.listen.port();
            let host = host.unwrap_or_else(|| config.hosts().remove(0));
            let hub = hubd::open_hub(config, clock)?;
            let secret = hub.new_admin_pairing(&label)?;
            let pairing = PairingString {
                host,
                port,
                spki_sha256: hub.identity.spki_sha256,
                secret,
            };
            println!("Pairing string for Display Hub on your PC. It works once, within 10 minutes:\n");
            println!("  {}\n", pairing.encode());
            println!("Hub fingerprint: {}", fingerprint(&hub.identity.spki_sha256));
            println!("Paste the whole string into the desktop manager. Don't share it or save it anywhere.");
        }
        Command::Admin { action, settings } => {
            let hub = hubd::open_hub(settings.load()?, clock.clone())?;
            match action {
                AdminAction::List => {
                    let admins = hub.store.admins()?;
                    if admins.is_empty() {
                        println!("No admins have paired yet. Run `hubd pair-admin`.");
                    }
                    for a in admins {
                        println!(
                            "{}  {}  paired {}  {}",
                            a.subject,
                            if a.revoked { "REVOKED" } else { "active " },
                            a.created_at,
                            a.label
                        );
                    }
                }
                AdminAction::Revoke { id } => {
                    let n = hub.store.revoke(Role::Admin, &id, clock.now())?;
                    hub.store
                        .audit(clock.now(), "admin_revoked", &format!("{id} (command line)"))?;
                    if n == 0 {
                        anyhow::bail!("no active admin with id {id}; see `hubd admin list`");
                    }
                    println!("Revoked {id}. It can no longer change the hub.");
                }
                AdminAction::RevokeAll => {
                    let n = hub.store.revoke_all(Role::Admin, clock.now())?;
                    hub.store
                        .audit(clock.now(), "admin_revoked_all", &format!("{n} (command line)"))?;
                    println!("Revoked {n} admin(s). Run `hubd pair-admin` to pair the manager again.");
                }
            }
        }
        Command::Fingerprint { settings } => {
            let hub = hubd::open_hub(settings.load()?, clock)?;
            println!("{}", fingerprint(&hub.identity.spki_sha256));
        }
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
                clock,
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
