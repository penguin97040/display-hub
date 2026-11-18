use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use display_simulator::firmware::store::NextStart;
use display_simulator::firmware::{self, Acted, Firmware, Started, SystemUptime, TrialOutcome};
use display_simulator::render::screen;
use display_simulator::{paired, ClockJitter, Device, Link};
use hub_client::HubClient;
use hub_core::{Clock, SystemClock};
use std::sync::Arc;

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

    /// Sends a heartbeat when paired with a state folder.
    async fn heartbeat(&self, dir: Option<&std::path::Path>) {
        if let (Target::Paired(s), Some(dir)) = (self, dir) {
            if let Err(e) = paired::heartbeat(s, &paired::device_info(dir)).await {
                eprintln!("display-simulator: heartbeat not sent: {e}");
            }
        }
    }
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ConformAction {
    Check,
    Write,
}

#[derive(Subcommand)]
enum FirmwareAction {
    /// Print the trusted keys, floor, app slots and update record.
    Show {
        #[arg(long)]
        state: PathBuf,
    },
    /// Change the simulated board, set keys on a display that has none, or set test switches.
    Set {
        #[arg(long)]
        state: PathBuf,
        /// The version the factory image reports, until an update replaces it.
        #[arg(long)]
        version: Option<String>,
        /// The size of each app partition, in bytes.
        #[arg(long)]
        slot_bytes: Option<u64>,
        /// A key to trust (repeat for up to three): 130 hex digits, or a key file's public half. Only
        /// for a display with no keys, as a provisioning bundle would set them.
        #[arg(long = "trust")]
        trust: Vec<String>,
        /// The starting security floor, with --trust.
        #[arg(long, requires = "trust")]
        floor: Option<u32>,
        /// How the next new firmware behaves on its trial boot.
        #[arg(long, value_enum)]
        next_start: Option<NextStart>,
        /// Lose power once this many bytes of the next image are written.
        #[arg(long)]
        power_cut_after: Option<u64>,
        /// Make the next write to the spare slot fail.
        #[arg(long)]
        flash_fails: bool,
    },
    /// Simulate the app's Install firmware: erase everything, write the package's image, then take
    /// the keys and floor the app's provisioning bundle would carry (the package's key_set, or the key
    /// that signed it, and its security counter).
    Install {
        #[arg(long)]
        state: PathBuf,
        /// The package (`.dhfw`). It needs no USB-install parts here.
        #[arg(long)]
        package: PathBuf,
    },
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
    /// Conformance vectors for display firmware (protocol/v1/vectors and protocol/v1/firmware):
    /// `check` proves the files match this reference display; `write` regenerates the snapshot vectors
    /// from protocol/v1, and writes any firmware vector that is missing or out of date (signatures are
    /// random, so the others are kept as they are).
    Conform {
        #[arg(value_enum)]
        action: ConformAction,
        /// The snapshot vectors folder.
        #[arg(long, default_value = "protocol/v1/vectors")]
        dir: PathBuf,
        /// The firmware update vectors folder.
        #[arg(long, default_value = "protocol/v1/firmware")]
        firmware_dir: PathBuf,
        /// The protocol folder the snapshot vectors are built from (for `write`).
        #[arg(long, default_value = "protocol/v1")]
        protocol: PathBuf,
    },
    /// This display's signed-update state: show it, change the board or test switches, or simulate
    /// the app's first install over USB.
    Firmware {
        #[command(subcommand)]
        action: FirmwareAction,
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
    // This process starting is the display starting: its running time begins at zero here.
    let uptime = SystemUptime::new();
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
        Command::Conform {
            action,
            dir,
            firmware_dir,
            protocol,
        } => match action {
            ConformAction::Write => {
                let snapshots = display_simulator::conform::write(&protocol, &dir).map_err(|e| e.to_string());
                let updates = firmware::vectors::write(&firmware_dir);
                match (snapshots, updates) {
                    (Ok(n), Ok((written, kept))) => {
                        println!("Wrote {n} vectors to {}.", dir.display());
                        println!(
                            "Wrote {written} firmware vectors to {}, and kept {kept} that are still right.",
                            firmware_dir.display()
                        );
                        ExitCode::SUCCESS
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        eprintln!("display-simulator: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            ConformAction::Check => {
                let results = [
                    display_simulator::conform::check(&dir),
                    firmware::vectors::check(&firmware_dir),
                ];
                let mut total = 0;
                let mut all = Vec::new();
                for r in results {
                    match r {
                        Ok((n, failures)) if n > 0 => {
                            total += n;
                            all.extend(failures);
                        }
                        Ok(_) => all.push("a vectors folder is empty".into()),
                        Err(e) => all.push(e.to_string()),
                    }
                }
                if all.is_empty() {
                    println!("All {total} vectors match the reference display.");
                    ExitCode::SUCCESS
                } else {
                    for f in &all {
                        eprintln!("FAIL {f}");
                    }
                    eprintln!("{} of {total} vectors failed", all.len());
                    ExitCode::FAILURE
                }
            }
        },
        Command::Firmware { action } => firmware_command(action),
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
            let mut updates = match (&hub, &conn.state) {
                (Target::Paired(_), Some(dir)) => match Firmware::open(dir, Arc::new(uptime)) {
                    Ok(mut fw) => match fw.boot() {
                        Ok(started) => {
                            println!("{}", started_text(&started));
                            Some(fw)
                        }
                        Err(e) => {
                            eprintln!("display-simulator: couldn't start: {e}");
                            return ExitCode::FAILURE;
                        }
                    },
                    Err(e) => {
                        eprintln!("display-simulator: couldn't read the firmware state: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                _ => None,
            };
            let mut polls = 0;
            let mut heartbeats = Heartbeats::default();
            loop {
                let result = hub.poll().await;
                let offer = result.as_ref().ok().and_then(|s| s.firmware_update.clone());
                if let (Some(fw), Target::Paired(state)) = (updates.as_mut(), &hub) {
                    if let Err(e) = act_on_updates(fw, state, &result, offer.as_ref()).await {
                        eprintln!("display-simulator: firmware update: {e}");
                    }
                }
                let mut wait = device.accept(clock.now(), result);
                if updates.as_ref().is_some_and(Firmware::in_trial) {
                    // On trial, check in often: it has five minutes to reach the hub.
                    wait = wait.min(std::time::Duration::from_secs(TRIAL_POLL_SECONDS));
                }
                // Not while on trial: the hub counts a heartbeat naming the new version as installed,
                // and the firmware hasn't been marked good yet.
                let attempts = conn
                    .state
                    .as_deref()
                    .and_then(|d| display_simulator::firmware::store::record(d).ok())
                    .map(|r| r.budget().attempts_left);
                let now = std::time::Instant::now();
                if matches!(device.link, Link::Online { .. })
                    && !updates.as_ref().is_some_and(Firmware::in_trial)
                    && heartbeats.due(now, attempts)
                {
                    hub.heartbeat(conn.state.as_deref()).await;
                    heartbeats.sent(now, attempts);
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

/// How often new firmware on trial polls, so it reaches the hub well within its five minutes.
const TRIAL_POLL_SECONDS: u64 = 15;

/// When `run` sends a heartbeat: once an hour, and once as soon as the attempts left change, so the
/// app doesn't show a pause late or keep showing it after the display has recovered. A heartbeat
/// that fails isn't tried again at the next poll: the hourly one carries the number, which keeps it
/// to a handful a day however long the hub is away.
#[derive(Default)]
struct Heartbeats {
    last: Option<std::time::Instant>,
    /// The attempts left that the last heartbeat carried, whether or not the hub took it.
    sent_attempts: Option<u32>,
}

impl Heartbeats {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(3600);

    fn due(&self, now: std::time::Instant, attempts: Option<u32>) -> bool {
        self.last.is_none_or(|t| now.duration_since(t) >= Self::EVERY)
            || (attempts.is_some() && attempts != self.sent_attempts)
    }

    fn sent(&mut self, now: std::time::Instant, attempts: Option<u32>) {
        self.last = Some(now);
        self.sent_attempts = attempts;
    }
}

fn started_text(s: &Started) -> String {
    match s {
        Started::Normal { version } => format!("Running firmware {version}."),
        Started::Trial { version } => {
            format!("Started new firmware {version} on trial: it must reach the hub within five minutes.")
        }
        Started::RolledBack { version } => {
            format!("The update didn't start properly, so this display went back to {version}.")
        }
        Started::Interrupted { version } => {
            format!("An update was cut short. Still running {version}; it will be reported as failed.")
        }
    }
}

/// One poll's part in signed updates: the trial boot's check-in, any unanswered report, and the
/// offer in the snapshot.
async fn act_on_updates(
    fw: &mut Firmware,
    state: &paired::State,
    result: &Result<hub_protocol::Snapshot, hub_client::ClientError>,
    offer: Option<&hub_protocol::firmware::FirmwareUpdate>,
) -> std::io::Result<()> {
    // The poll was the trial boot's check-in, if there is one. Then time passing: the trial's five
    // minutes, and running time towards the attempt budget.
    let checked_in = if fw.in_trial() {
        fw.check_in_with(result)?
    } else {
        TrialOutcome::NotInTrial
    };
    let outcome = match checked_in {
        TrialOutcome::Waiting | TrialOutcome::NotInTrial => fw.tick()?,
        other => other,
    };
    match outcome {
        TrialOutcome::MarkedGood { version } => {
            println!("Firmware {version} reached the hub and is now marked good.")
        }
        TrialOutcome::RolledBack(s) => println!("{}", started_text(&s)),
        TrialOutcome::Waiting | TrialOutcome::NotInTrial => {}
    }
    fw.send_report(state).await?;
    if let Some(offer) = offer {
        match fw.on_offer(state, offer).await? {
            Acted::Ignored | Acted::Busy => {}
            Acted::Waiting { seconds } => println!(
                "A firmware update is offered, but this display has no download attempts left. It earns the next after {} more minutes of running.",
                seconds.div_ceil(60)
            ),
            Acted::Refused(r) => println!("Refused the offered firmware ({r}). Nothing was written."),
            Acted::Failed(f) => println!(
                "The firmware update failed ({}). The old firmware is still running.",
                f.reason().as_str()
            ),
            Acted::PowerCut => {
                println!("The power went off during the update (a test switch). Starting again.");
                println!("{}", started_text(&fw.boot()?));
            }
            Acted::Restarted(s) => println!("Installed the update and restarted. {}", started_text(&s)),
        }
    }
    Ok(())
}

/// A key to trust: 130 hex digits, or a key file's public half.
fn trusted_key(arg: &str) -> Result<String, String> {
    if let Some(k) =
        firmware_package::PublicKey::from_hex(arg).filter(firmware_package::PublicKey::is_valid_point)
    {
        return Ok(k.to_hex());
    }
    let bytes = std::fs::read(arg)
        .map_err(|e| format!("{arg:?} is neither a public key nor a readable key file: {e}"))?;
    firmware_package::keyfile::public_key(&bytes)
        .map(|k| k.to_hex())
        .map_err(|e| format!("{arg} isn't a key file: {e}"))
}

fn firmware_command(action: FirmwareAction) -> ExitCode {
    let result = match action {
        FirmwareAction::Show { state } => show_firmware(&state),
        FirmwareAction::Set {
            state,
            version,
            slot_bytes,
            trust,
            floor,
            next_start,
            power_cut_after,
            flash_fails,
        } => (|| -> Result<(), String> {
            let mut board = firmware::store::board(&state).map_err(|e| e.to_string())?;
            if let Some(v) = version {
                firmware_package::manifest::check_firmware_version(&v)
                    .map_err(|p| format!("--version {p}"))?;
                board.factory_version = v;
            }
            if let Some(n) = slot_bytes {
                if n == 0 || n > firmware_package::limits::MAX_IMAGE_SIZE {
                    return Err(format!(
                        "--slot-bytes must be from 1 to {}",
                        firmware_package::limits::MAX_IMAGE_SIZE
                    ));
                }
                board.slot_bytes = n;
            }
            firmware::store::set_board(&state, &board).map_err(|e| e.to_string())?;
            if !trust.is_empty() {
                let keys = trust
                    .iter()
                    .map(|t| trusted_key(t))
                    .collect::<Result<Vec<_>, _>>()?;
                let section = hub_protocol::firmware::BundleFirmware {
                    trusted_keys: keys,
                    security_counter: floor.unwrap_or(0),
                };
                let problems = section.problems();
                if !problems.is_empty() {
                    return Err(problems.join("; "));
                }
                let store = firmware::store::keys(&state).map_err(|e| e.to_string())?;
                if !store.trusted_keys.is_empty() {
                    return Err(
                        "this display already trusts keys; only `firmware install` can change them".into(),
                    );
                }
                firmware::store::accept_bundle(&state, Some(&section))
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
            }
            let mut switches = firmware::store::switches(&state).map_err(|e| e.to_string())?;
            if let Some(n) = next_start {
                switches.next_start = n;
            }
            if power_cut_after.is_some() {
                switches.power_cut_after_image_bytes = power_cut_after;
            }
            if flash_fails {
                switches.flash_fails = true;
            }
            firmware::store::set_switches(&state, &switches).map_err(|e| e.to_string())?;
            show_firmware(&state)
        })(),
        FirmwareAction::Install { state, package } => (|| -> Result<(), String> {
            let checked =
                firmware_package::check_file(&package).map_err(|e| format!("{}: {e}", package.display()))?;
            let bytes = std::fs::read(&package).map_err(|e| e.to_string())?;
            firmware::store::install_firmware(&state, &checked, &bytes).map_err(|e| e.to_string())?;
            let section = firmware::store::bundle_for(&checked);
            firmware::store::accept_bundle(&state, Some(&section))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            println!(
                "Erased the display and installed {} {}, signed by {}.",
                checked.manifest.device_type,
                checked.manifest.firmware_version,
                checked.signer().fingerprint()
            );
            println!("Set it up with `usb` or `enrol`, as after a factory reset.");
            show_firmware(&state)
        })(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("display-simulator: {e}");
            ExitCode::FAILURE
        }
    }
}

fn show_firmware(state: &std::path::Path) -> Result<(), String> {
    use firmware::store;
    let text = |e: std::io::Error| e.to_string();
    let board = store::board(state).map_err(text)?;
    let keys = store::keys(state).map_err(text)?;
    let ota = store::ota(state).map_err(text)?;
    let record = store::record(state).map_err(text)?;
    let switches = store::switches(state).map_err(text)?;
    println!(
        "Firmware {} ({}, {})",
        store::running_version(state).map_err(text)?,
        store::DEVICE_TYPE,
        store::HARDWARE_REVISION
    );
    if keys.trusted_keys.is_empty() {
        println!("Trusted keys: none, so every update is refused");
    } else {
        println!("Trusted keys:");
        for k in keys.keys() {
            println!("  {}", k.fingerprint());
        }
    }
    println!("Security floor: {}", keys.security_floor);
    println!("App slots: {} bytes each", board.slot_bytes);
    for name in [store::SlotName::A, store::SlotName::B] {
        let slot = ota.slot(name);
        let marks = [
            (ota.running == name).then_some("running"),
            (ota.boot == name).then_some("boots next"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        println!(
            "  {}: {:?}, {}{}",
            name.as_str(),
            slot.state,
            match (slot.state, slot.firmware_version.as_deref()) {
                (store::SlotState::Empty, _) => "nothing to start",
                (_, Some(v)) => v,
                (_, None) => "factory image",
            },
            if marks.is_empty() {
                String::new()
            } else {
                format!(" ({marks})")
            }
        );
    }
    if let Some(id) = &record.last_offer_id {
        println!("Last offer acted on: {id}");
    }
    if let Some(p) = &record.pending {
        println!(
            "Waiting for its trial boot: {} (security counter {})",
            p.firmware_version, p.security_counter
        );
    }
    if let Some(r) = &record.last_result {
        println!(
            "Last result: {}{}{}",
            r.state.as_str(),
            r.reason.map(|x| format!(" ({})", x.as_str())).unwrap_or_default(),
            if r.answered { "" } else { ", not yet reported" }
        );
    }
    let budget = record.budget();
    if budget.is_full() {
        println!(
            "Download attempts left: {} of {}",
            budget.attempts_left,
            firmware::rules::FULL_BUDGET
        );
    } else {
        println!(
            "Download attempts left: {} of {} ({} of {} hours of running counted towards the next)",
            budget.attempts_left,
            firmware::rules::FULL_BUDGET,
            budget.credit_hours,
            firmware::rules::CREDIT_HOURS_PER_ATTEMPT
        );
    }
    let mut on = Vec::new();
    match switches.next_start {
        NextStart::Normal => {}
        NextStart::Fails => on.push("the next new firmware fails to start".to_string()),
        NextStart::NoHub => on.push("the next new firmware can't reach the hub".to_string()),
    }
    if let Some(n) = switches.power_cut_after_image_bytes {
        on.push(format!("the power goes off after {n} bytes of the next image"));
    }
    if switches.flash_fails {
        on.push("the next write to the spare slot fails".into());
    }
    if !on.is_empty() {
        println!("Test switches: {}", on.join("; "));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::Heartbeats;
    use std::time::{Duration, Instant};

    #[test]
    fn a_change_in_attempts_is_sent_once_and_then_hourly() {
        let start = Instant::now();
        let mut h = Heartbeats::default();
        assert!(h.due(start, Some(5)), "the first poll");
        h.sent(start, Some(5));
        assert!(!h.due(start + Duration::from_secs(300), Some(5)));
        // A change goes at once. Whether or not the hub took it, the next polls don't repeat it.
        assert!(h.due(start + Duration::from_secs(300), Some(4)));
        h.sent(start + Duration::from_secs(300), Some(4));
        for poll in 2..12 {
            assert!(
                !h.due(start + Duration::from_secs(300 * poll), Some(4)),
                "poll {poll}"
            );
        }
        // The hourly one carries it again.
        assert!(h.due(start + Duration::from_secs(300 + 3600), Some(4)));
        // A display without a budget to report is heard from hourly.
        let mut h = Heartbeats::default();
        h.sent(start, None);
        assert!(!h.due(start + Duration::from_secs(600), None));
        assert!(h.due(start + Duration::from_secs(3600), None));
    }
}
