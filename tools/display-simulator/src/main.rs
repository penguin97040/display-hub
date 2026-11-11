use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use display_simulator::render::screen;
use display_simulator::{paired, ClockJitter, Device, Link};
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
    /// A display set up with `enrol`: its folder of saved state. Polls over pinned HTTPS.
    #[arg(long, conflicts_with = "hub")]
    state: Option<PathBuf>,
    /// Or a development hub (`hubd dev`) on this computer, over plain HTTP without credentials.
    #[arg(long, default_value = "http://127.0.0.1:8080/")]
    hub: String,
    /// Where to keep the last good snapshot. Defaults to the state folder when there is one.
    #[arg(long)]
    cache: Option<PathBuf>,
}

/// Where polls go.
enum Target {
    Dev(HubClient),
    Paired(paired::State),
}

impl Target {
    async fn poll(&self) -> Result<hub_protocol::Snapshot, hub_client::ClientError> {
        match self {
            Target::Dev(c) => c.device_snapshot(None).await,
            Target::Paired(s) => paired::snapshot(s).await,
        }
    }

    async fn heartbeat(&self) {
        if let Target::Paired(s) = self {
            if let Err(e) = paired::heartbeat(s).await {
                eprintln!("display-simulator: heartbeat not sent: {e}");
            }
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Set this simulated display up from a provisioning bundle, as the manager does over USB.
    Enrol {
        /// The provisioning bundle (JSON) the desktop manager saved.
        #[arg(long)]
        bundle: PathBuf,
        /// Folder to keep the display's credentials and saved data in.
        #[arg(long)]
        state: PathBuf,
    },
    /// Wait on a pseudo-terminal for the desktop manager to send a bundle over "USB", then enrol
    /// with it. Prints the port's path for the manager. Unix only.
    Usb {
        /// Folder to keep the display's credentials and saved data in.
        #[arg(long)]
        state: PathBuf,
        /// Exit after one bundle instead of waiting for more (which are refused until `reset`).
        #[arg(long)]
        once: bool,
    },
    /// Erase the display's credentials and saved data, as a factory reset does.
    Reset {
        #[arg(long)]
        state: PathBuf,
    },
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

fn target(conn: &Connection) -> Result<(Target, Option<PathBuf>), ExitCode> {
    let fail = |e: String| {
        eprintln!("display-simulator: {e}");
        ExitCode::from(2)
    };
    match &conn.state {
        Some(dir) => {
            let state = paired::load(dir).map_err(fail)?;
            Ok((
                Target::Paired(state),
                Some(conn.cache.clone().unwrap_or_else(|| paired::cache_file(dir))),
            ))
        }
        None => Ok((
            Target::Dev(HubClient::loopback_dev(&conn.hub).map_err(|e| fail(e.to_string()))?),
            conn.cache.clone(),
        )),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let clock = SystemClock;
    match cli.command {
        Command::Enrol { bundle, state } => {
            let bundle = match std::fs::read(&bundle).map_err(|e| e.to_string()).and_then(|b| {
                serde_json::from_slice::<hub_protocol::provisioning::Bundle>(&b).map_err(|e| e.to_string())
            }) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("display-simulator: couldn't read the bundle: {e}");
                    return ExitCode::from(2);
                }
            };
            match paired::enrol(&bundle, &state).await {
                Ok(s) => {
                    println!(
                        "Enrolled as {} with the hub at {}.",
                        s.device_id,
                        s.hub.hosts.join(" or ")
                    );
                    println!("Delete the bundle file now: its code is used up, but it may hold your Wi-Fi password.");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("display-simulator: enrolment failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Usb { state, once } => usb(&state, once).await,
        Command::Reset { state } => match paired::reset(&state) {
            Ok(()) => {
                println!("Erased the display's credentials and saved data.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("display-simulator: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Once { conn } => {
            let (hub, cache) = match target(&conn) {
                Ok(t) => t,
                Err(code) => return code,
            };
            let mut device = Device::new(cache, Box::new(ClockJitter::default()));
            device.accept(clock.now(), hub.poll().await);
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
            let (hub, cache) = match target(&conn) {
                Ok(t) => t,
                Err(code) => return code,
            };
            let mut device = Device::new(cache, Box::new(ClockJitter::default()));
            let mut polls = 0;
            let mut last_heartbeat: Option<std::time::Instant> = None;
            loop {
                let mut wait = device.accept(clock.now(), hub.poll().await);
                if matches!(device.link, Link::Online { .. })
                    && last_heartbeat.is_none_or(|t| t.elapsed().as_secs() >= 3600)
                {
                    hub.heartbeat().await;
                    last_heartbeat = Some(std::time::Instant::now());
                }
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

#[cfg(unix)]
async fn usb(state: &std::path::Path, once: bool) -> ExitCode {
    use serialport::SerialPort as _;
    use std::io::Write;

    let (mut master, slave) = match serialport::TTYPort::pair() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("display-simulator: couldn't make a pseudo-terminal: {e}");
            return ExitCode::FAILURE;
        }
    };
    // The manager opens the other end by name, and wants it to itself, so this copy is closed.
    println!("Serial port: {}", slave.name().unwrap_or_default());
    drop(slave);
    println!("Waiting for the desktop manager. Stop with Ctrl+C.");
    let _ = std::io::stdout().flush();
    loop {
        match display_simulator::usb::answer(&mut master, state) {
            Ok(Some(bundle)) => {
                println!(
                    "Received setup details for the hub at {}. Enrolling.",
                    bundle.hub.hosts.join(" or ")
                );
                match paired::enrol(&bundle, state).await {
                    Ok(s) => println!("Enrolled as {}.", s.device_id),
                    Err(e) => eprintln!("display-simulator: enrolment failed: {e}"),
                }
                let _ = std::io::stdout().flush();
                if once {
                    return ExitCode::SUCCESS;
                }
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                eprintln!("display-simulator: the serial port failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
}

#[cfg(not(unix))]
async fn usb(_state: &std::path::Path, _once: bool) -> ExitCode {
    eprintln!("display-simulator: `usb` needs a pseudo-terminal, which only Linux and macOS have");
    ExitCode::from(2)
}
