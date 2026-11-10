use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use display_simulator::render::screen;
use display_simulator::{ClockJitter, Device, Link};
use hub_client::HubClient;
use hub_core::{Clock, SystemClock};

/// Emulates a Display Hub display.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Connection {
    /// The hub's address. Until pairing exists, only a development hub on this computer.
    #[arg(long, default_value = "http://127.0.0.1:8080/")]
    hub: String,
    /// Where to keep the last good snapshot, as a display keeps it in flash.
    #[arg(long)]
    cache: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Poll once, draw the screen and exit. Exit status: 0 online, 3 showing saved data, 4 nothing to show.
    Once {
        #[command(flatten)]
        conn: Connection,
    },
    /// Poll repeatedly as a display does, redrawing after each poll. Stop with Ctrl+C.
    Poll {
        #[command(flatten)]
        conn: Connection,
        /// Stop after this many polls.
        #[arg(long)]
        max_polls: Option<u32>,
        /// Wait at most this many seconds between polls (to watch changes quickly while testing).
        #[arg(long)]
        max_wait: Option<u64>,
    },
    /// Check snapshot files against protocol v1 as a hub must follow it.
    Validate {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn client(conn: &Connection) -> Result<HubClient, ExitCode> {
    HubClient::loopback_dev(&conn.hub).map_err(|e| {
        eprintln!("display-simulator: {e}");
        ExitCode::from(2)
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let clock = SystemClock;
    match cli.command {
        Command::Once { conn } => {
            let hub = match client(&conn) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let mut device = Device::new(conn.cache.clone(), Box::new(ClockJitter::default()));
            device.accept(clock.now(), hub.device_snapshot(None).await);
            print!("{}", screen(&device, clock.now()));
            match (&device.link, &device.held) {
                (Link::Online { .. }, _) => ExitCode::SUCCESS,
                (_, Some(_)) => ExitCode::from(3),
                (_, None) => ExitCode::from(4),
            }
        }
        Command::Poll {
            conn,
            max_polls,
            max_wait,
        } => {
            let hub = match client(&conn) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let mut device = Device::new(conn.cache.clone(), Box::new(ClockJitter::default()));
            let mut polls = 0;
            loop {
                let mut wait = device.accept(clock.now(), hub.device_snapshot(None).await);
                if let Some(max) = max_wait {
                    wait = wait.min(std::time::Duration::from_secs(max));
                }
                print!("{}", screen(&device, clock.now()));
                println!("Next poll in {} s\n", wait.as_secs());
                polls += 1;
                if max_polls.is_some_and(|m| polls >= m) {
                    return ExitCode::SUCCESS;
                }
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = tokio::signal::ctrl_c() => return ExitCode::SUCCESS,
                }
            }
        }
        Command::Validate { files } => {
            let mut failed = false;
            for f in files {
                let result = std::fs::read(&f)
                    .map_err(|e| e.to_string())
                    .and_then(|b| {
                        serde_json::from_slice::<hub_protocol::Snapshot>(&b).map_err(|e| e.to_string())
                    })
                    .and_then(|s| {
                        s.validate()
                            .map_err(|v| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n  "))
                    });
                match result {
                    Ok(()) => println!("valid    {}", f.display()),
                    Err(e) => {
                        failed = true;
                        println!("INVALID  {}\n  {e}", f.display());
                    }
                }
            }
            if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}
