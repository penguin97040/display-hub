//! Signed firmware updates, as the reference display does them (`docs/firmware-updates.md`, "What a
//! display must do").
//!
//! - [`store`] is what the display keeps: two app slots, the OTA data, the trusted keys and floor,
//!   and the update record.
//! - [`install`] is steps 1 to 9 for one download, pushed a piece at a time.
//! - [`rules`] are the rules on trying (the attempt budget, the offer last acted on, the trial
//!   boot), as pure functions of the record and the display's running time.
//! - [`Firmware`] puts them together against a real hub: acting on an offer, the restart, the trial
//!   boot, rolling back and reporting.
//! - [`vectors`] turns the document's test list into language-neutral conformance vectors.
//!
//! Every wait is measured by an injected [`Uptime`], a monotonic timer that starts at zero at every
//! start, never by the wall clock or the hub's time. The two download time limits are [`Limits`].
//! Tests move both, so they never wait for real.

pub mod install;
pub mod rules;
pub mod store;
pub mod uptime;
pub mod vectors;

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use firmware_package::{Display, Manifest, PublicKey, Refusal};
use hub_client::ClientError;
use hub_protocol::firmware::{FirmwareReport, FirmwareUpdate, ReportState};
use hub_protocol::limits::{FIRMWARE_DOWNLOAD_STALL_SECONDS, FIRMWARE_DOWNLOAD_TIMEOUT_SECONDS};

use crate::paired;
use install::{Ended, Failure, Flash, FlashError, Installer, Progress};
use rules::{decide, CheckIn, Decision, Meter, Trial, TrialStep};
use store::{Attempt, NextStart, Outcome, Pending, SlotName, SlotState, Switches};
pub use uptime::{FakeUptime, SystemUptime, Uptime};

/// The two time limits on a download. Tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Give up on a download that sends nothing for this long.
    pub stall: Duration,
    /// Give up on a download that takes longer than this in all.
    pub total: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(FIRMWARE_DOWNLOAD_STALL_SECONDS),
            total: Duration::from_secs(FIRMWARE_DOWNLOAD_TIMEOUT_SECONDS),
        }
    }
}

/// What a start found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Started {
    /// The usual firmware, with nothing to settle.
    Normal { version: String },
    /// New firmware on its trial boot: it must reach the hub within five minutes.
    Trial { version: String },
    /// The previous firmware, back after an update that didn't prove itself.
    RolledBack { version: String },
    /// An attempt was cut short by a restart or power cut: reported as `failed` with `download`.
    Interrupted { version: String },
}

/// What came of an offer in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acted {
    /// The offer last acted on, or one refused before any request since this start.
    Ignored,
    /// No attempts left: the offer is left open and nothing is reported. `seconds` is how much more
    /// running earns the next attempt.
    Waiting {
        seconds: u64,
    },
    /// New firmware on its trial boot doesn't take offers, and an unanswered final report is sent
    /// before anything new is tried.
    Busy,
    Refused(Refusal),
    Failed(Failure),
    /// The power went off (a test switch). Call [`Firmware::boot`] again, as the next start.
    PowerCut,
    /// Installed and restarted.
    Restarted(Started),
}

/// What the trial boot did with a check-in or a tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrialOutcome {
    NotInTrial,
    Waiting,
    MarkedGood { version: String },
    RolledBack(Started),
}

pub struct Firmware {
    state: PathBuf,
    uptime: Arc<dyn Uptime>,
    /// The timer's reading at this start. Running time is counted from here, so it is zero at every
    /// start.
    started: Duration,
    /// Running time not yet saved as a credit hour. Lost at a restart.
    meter: Meter,
    pub limits: Limits,
    pub switches: Switches,
    trial: Option<Trial>,
    /// The firmware on trial can't reach the hub (a test switch).
    trial_no_hub: bool,
    /// The last offer refused before anything was requested (`no_trusted_keys`, `too_large`), and
    /// whether the hub has answered its report. Kept in memory only, until the next start: such a
    /// refusal spends nothing, so nothing about it is written.
    refused_early: Option<Outcome>,
}

fn io_other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl Firmware {
    /// The display whose state is in `state`, with its test switches from `switches.json`, timing
    /// itself with `uptime`. Call [`Firmware::boot`] before anything else.
    pub fn open(state: &Path, uptime: Arc<dyn Uptime>) -> std::io::Result<Self> {
        let started = uptime.elapsed();
        Ok(Self {
            state: state.to_path_buf(),
            uptime,
            started,
            meter: Meter::at_start(),
            limits: Limits::default(),
            switches: store::switches(state)?,
            trial: None,
            trial_no_hub: false,
            refused_early: None,
        })
    }

    pub fn in_trial(&self) -> bool {
        self.trial.is_some()
    }

    /// Seconds since this start, by the monotonic timer.
    fn running_seconds(&self) -> u64 {
        self.uptime.elapsed().saturating_sub(self.started).as_secs()
    }

    /// Counts running time towards the attempt budget, saving each credit hour it adds. While the
    /// budget is full, nothing is counted or written.
    fn count_running_time(&mut self) -> std::io::Result<()> {
        let mut record = store::record(&self.state)?;
        if let Some((budget, _hours)) = self.meter.count(record.budget(), self.running_seconds()) {
            record.set_budget(budget);
            store::set_record(&self.state, &record)?;
        }
        Ok(())
    }

    fn save_switches(&self) -> std::io::Result<()> {
        store::set_switches(&self.state, &self.switches)
    }

    /// A start, as the bootloader and then the running firmware see it. The bootloader starts the
    /// boot slot: a new image gets one start on trial, and an image found still on trial (the board
    /// reset before it was marked good) is marked invalid and the other slot started instead. The
    /// firmware that runs then settles whatever the update record says it left unfinished.
    pub fn boot(&mut self) -> std::io::Result<Started> {
        // A start: the timer begins again at zero, and whatever was only in memory is gone.
        self.started = self.uptime.elapsed();
        self.meter = Meter::at_start();
        self.refused_early = None;
        self.trial = None;
        self.trial_no_hub = false;
        let mut ota = store::ota(&self.state)?;
        let mut on_trial = false;
        // Two slots, so at most a few steps: each step either starts a slot or rules one out.
        for _ in 0..4 {
            let boot = ota.boot;
            match ota.slot(boot).state {
                SlotState::New => {
                    ota.slot_mut(boot).state = SlotState::PendingVerify;
                    ota.running = boot;
                    store::set_ota(&self.state, &ota)?;
                    match self.switches.next_start {
                        NextStart::Fails => {
                            // It crashes before it can check in, and the board resets.
                            self.switches.next_start = NextStart::Normal;
                            self.save_switches()?;
                            continue;
                        }
                        NextStart::NoHub => {
                            self.switches.next_start = NextStart::Normal;
                            self.save_switches()?;
                            self.trial_no_hub = true;
                        }
                        NextStart::Normal => {}
                    }
                    on_trial = true;
                    break;
                }
                SlotState::PendingVerify => {
                    // Reset during its trial: the bootloader rolls back.
                    ota.slot_mut(boot).state = SlotState::Invalid;
                    ota.boot = boot.other();
                }
                SlotState::Valid => {
                    ota.running = boot;
                    store::set_ota(&self.state, &ota)?;
                    break;
                }
                SlotState::Empty | SlotState::Invalid => ota.boot = boot.other(),
            }
        }
        let running = ota.running;
        if ota.slot(running).state != SlotState::Valid && ota.slot(running).state != SlotState::PendingVerify
        {
            return Err(io_other("neither app slot holds firmware that can start"));
        }
        let version = store::running_version(&self.state)?;
        if on_trial {
            self.trial = Some(Trial {
                started_at: self.running_seconds(),
            });
            return Ok(Started::Trial { version });
        }

        let mut record = store::record(&self.state)?;
        if let Some(p) = record.pending.clone() {
            if p.slot != running {
                // The previous firmware, with an update it didn't finish: the new one didn't prove
                // itself. Its keys are dropped and the floor is left alone.
                record.pending = None;
                record.attempt = None;
                self.settle(&mut record, &p.offer_id, ReportState::RolledBack, None);
                store::set_record(&self.state, &record)?;
                return Ok(Started::RolledBack { version });
            }
            // The new firmware was marked good, and the start ended before it applied the rest.
            self.apply_good(&mut record, &p)?;
            return Ok(Started::Normal { version });
        }
        if let Some(a) = record.attempt.take() {
            // A restart or power cut during the attempt: the old firmware is still the one running.
            self.settle(
                &mut record,
                &a.offer_id,
                ReportState::Failed,
                Some(Failure::Download.reason()),
            );
            store::set_record(&self.state, &record)?;
            return Ok(Started::Interrupted { version });
        }
        Ok(Started::Normal { version })
    }

    /// Records a final result to report. It never changes the attempt budget.
    fn settle(
        &self,
        record: &mut store::Record,
        offer_id: &str,
        state: ReportState,
        reason: Option<hub_protocol::firmware::ReportReason>,
    ) {
        record.last_result = Some(Outcome {
            offer_id: offer_id.to_string(),
            state,
            reason,
            answered: false,
        });
    }

    /// Once marked good, and only then: raise the floor, take the release's keys, clear the
    /// pending parts, and report `installed`. Safe to repeat.
    fn apply_good(&self, record: &mut store::Record, p: &Pending) -> std::io::Result<()> {
        let keys = store::after_good(
            &store::keys(&self.state)?,
            p.security_counter,
            p.key_set.as_deref(),
        );
        store::set_keys(&self.state, &keys)?;
        record.pending = None;
        record.attempt = None;
        self.settle(record, &p.offer_id, ReportState::Installed, None);
        store::set_record(&self.state, record)
    }

    fn mark_good(&mut self) -> std::io::Result<TrialOutcome> {
        let mut ota = store::ota(&self.state)?;
        let running = ota.running;
        ota.slot_mut(running).state = SlotState::Valid;
        store::set_ota(&self.state, &ota)?;
        self.trial = None;
        self.trial_no_hub = false;
        let mut record = store::record(&self.state)?;
        if let Some(p) = record.pending.clone() {
            self.apply_good(&mut record, &p)?;
        }
        Ok(TrialOutcome::MarkedGood {
            version: store::running_version(&self.state)?,
        })
    }

    /// `esp_ota_mark_app_invalid_rollback_and_reboot`.
    fn roll_back(&mut self) -> std::io::Result<TrialOutcome> {
        let mut ota = store::ota(&self.state)?;
        let running = ota.running;
        ota.slot_mut(running).state = SlotState::Invalid;
        ota.boot = running.other();
        store::set_ota(&self.state, &ota)?;
        Ok(TrialOutcome::RolledBack(self.boot()?))
    }

    /// The answer to a trial boot's authenticated check-in (any request to the hub with the token,
    /// over pinned TLS: a poll, for the simulator).
    pub fn check_in(&mut self, result: CheckIn) -> std::io::Result<TrialOutcome> {
        let Some(trial) = self.trial else {
            return Ok(TrialOutcome::NotInTrial);
        };
        let result = if self.trial_no_hub {
            CheckIn::Failed
        } else {
            result
        };
        match trial.check_in(self.running_seconds(), result) {
            TrialStep::Waiting => Ok(TrialOutcome::Waiting),
            TrialStep::MarkGood => self.mark_good(),
            TrialStep::RollBack => self.roll_back(),
        }
    }

    /// A poll's result, as a check-in.
    pub fn check_in_with<T>(&mut self, result: &Result<T, ClientError>) -> std::io::Result<TrialOutcome> {
        self.check_in(match result {
            Ok(_) => CheckIn::Answered,
            Err(e) => CheckIn::from_error(e.code()),
        })
    }

    /// Time passing: running time is counted towards the attempt budget, and a trial boot that
    /// hasn't reached the hub in five minutes rolls back.
    pub fn tick(&mut self) -> std::io::Result<TrialOutcome> {
        self.count_running_time()?;
        let Some(trial) = self.trial else {
            return Ok(TrialOutcome::NotInTrial);
        };
        match trial.tick(self.running_seconds()) {
            TrialStep::RollBack => self.roll_back(),
            _ => Ok(TrialOutcome::Waiting),
        }
    }

    /// Sends the last final report if the hub hasn't answered it yet: the one in the update record,
    /// then a refusal made before any request. Any answer at all settles it; only a failure to reach
    /// the hub leaves it to send again. Returns whether one is still waiting.
    ///
    /// Nothing is sent while new firmware is on trial: a report names the running version, and that
    /// version hasn't been marked good yet.
    pub async fn send_report(&mut self, hub: &paired::State) -> std::io::Result<bool> {
        let mut record = store::record(&self.state)?;
        let waiting = record.last_result.clone().filter(|o| !o.answered);
        let early = self.refused_early.clone().filter(|o| !o.answered);
        if self.in_trial() {
            return Ok(waiting.is_some() || early.is_some());
        }
        if let Some(outcome) = waiting {
            if !self.report_final(hub, &outcome).await? {
                return Ok(true);
            }
            if let Some(o) = record.last_result.as_mut() {
                o.answered = true;
            }
            store::set_record(&self.state, &record)?;
        }
        if let Some(outcome) = early {
            if !self.report_final(hub, &outcome).await? {
                return Ok(true);
            }
            if let Some(o) = self.refused_early.as_mut() {
                o.answered = true;
            }
        }
        Ok(false)
    }

    /// Sends one final report. Returns whether the hub answered it.
    async fn report_final(&self, hub: &paired::State, outcome: &Outcome) -> std::io::Result<bool> {
        let report = FirmwareReport {
            offer_id: outcome.offer_id.clone(),
            state: outcome.state,
            reason: outcome.reason,
            firmware_version: Some(store::running_version(&self.state)?),
        };
        Ok(!matches!(paired::firmware_report(hub, &report).await, Err(e) if e.is_connection()))
    }

    /// Acts on the offer in a snapshot ("Checks before installing" and "Limits on trying").
    pub async fn on_offer(&mut self, hub: &paired::State, offer: &FirmwareUpdate) -> std::io::Result<Acted> {
        if self.in_trial() || self.send_report(hub).await? {
            return Ok(Acted::Busy);
        }
        if self
            .refused_early
            .as_ref()
            .is_some_and(|o| o.offer_id == offer.offer_id)
        {
            return Ok(Acted::Ignored);
        }
        self.count_running_time()?;
        let mut record = store::record(&self.state)?;
        let decision = decide(
            &record,
            &offer.offer_id,
            self.meter.uncounted(self.running_seconds()),
        );
        if decision == Decision::Ignore {
            return Ok(Acted::Ignored);
        }

        let keys = store::keys(&self.state)?;
        let trusted: Vec<PublicKey> = keys.keys();
        let board = store::board(&self.state)?;
        let display = Display {
            trusted_keys: &trusted,
            device_type: store::DEVICE_TYPE,
            hardware_revision: store::HARDWARE_REVISION,
            security_floor: keys.security_floor,
            slot_bytes: board.slot_bytes,
        };
        // Steps 1 and 2 need nothing from the hub. A refusal here spends nothing, and nothing is
        // written: it is remembered, and reported, only until the next start.
        if let Err(r) = Installer::before_request(&display, offer.size) {
            let (state, reason) = Ended::Refused(r).report().expect("not a power cut");
            self.refused_early = Some(Outcome {
                offer_id: offer.offer_id.clone(),
                state,
                reason: Some(reason),
                answered: false,
            });
            self.send_report(hub).await?;
            return Ok(Acted::Refused(r));
        }
        match decision {
            Decision::Wait { seconds } => return Ok(Acted::Waiting { seconds }),
            Decision::Ignore | Decision::Attempt => {}
        }
        // One attempt spent: the reduced budget and the offer id are saved in a single write, before
        // anything is requested, so a restart still knows about it.
        let Some(budget) = record.budget().spend() else {
            return Ok(Acted::Waiting { seconds: 0 });
        };
        record.set_budget(budget);
        record.last_offer_id = Some(offer.offer_id.clone());
        record.attempt = Some(Attempt {
            offer_id: offer.offer_id.clone(),
        });
        store::set_record(&self.state, &record)?;

        let result = self.download(hub, offer, display).await?;
        let manifest = match result {
            Ok(m) => m,
            Err(Ended::PowerCut) => return Ok(Acted::PowerCut),
            Err(ended) => {
                let (state, reason) = ended.report().expect("not a power cut");
                let mut record = store::record(&self.state)?;
                record.attempt = None;
                self.settle(&mut record, &offer.offer_id, state, Some(reason));
                store::set_record(&self.state, &record)?;
                self.send_report(hub).await?;
                return Ok(match ended {
                    Ended::Refused(r) => Acted::Refused(r),
                    Ended::Failed(f) => Acted::Failed(f),
                    Ended::PowerCut => unreachable!("handled above"),
                });
            }
        };

        // 9. Save the update record, set the new slot to boot, and restart.
        let mut ota = store::ota(&self.state)?;
        let spare = ota.spare();
        let mut record = store::record(&self.state)?;
        record.attempt = None;
        record.pending = Some(Pending {
            offer_id: offer.offer_id.clone(),
            slot: spare,
            firmware_version: manifest.firmware_version.clone(),
            security_counter: manifest.security_counter,
            key_set: manifest
                .key_set
                .as_ref()
                .map(|ks| ks.iter().map(PublicKey::to_hex).collect()),
        });
        store::set_record(&self.state, &record)?;
        let slot = ota.slot_mut(spare);
        slot.state = SlotState::New;
        slot.firmware_version = Some(manifest.firmware_version.clone());
        ota.boot = spare;
        store::set_ota(&self.state, &ota)?;
        let started = self.boot()?;
        // Firmware that rolled straight back reports it as soon as it is running again.
        self.send_report(hub).await?;
        Ok(Acted::Restarted(started))
    }

    /// Steps 2 to 9 over the network, within the two time limits.
    async fn download(
        &mut self,
        hub: &paired::State,
        offer: &FirmwareUpdate,
        display: Display<'_>,
    ) -> std::io::Result<Result<Manifest, Ended>> {
        let limits = self.limits;
        let started = tokio::time::Instant::now();
        let deadline = started + limits.total;
        let mut stream = match tokio::time::timeout(
            limits.stall.min(limits.total),
            paired::firmware_stream(hub, &offer.package_id),
        )
        .await
        {
            Ok(Ok(s)) => s,
            // No answer, an error from the hub, or nothing at all for too long.
            _ => return Ok(Err(Ended::Failed(Failure::Download))),
        };
        let mut installer = match Installer::start(display, offer.size, stream.content_length) {
            Ok(i) => i,
            Err(r) => return Ok(Err(Ended::Refused(r))),
        };
        let ota = store::ota(&self.state)?;
        let mut flash = SlotFlash::new(&self.state, ota.spare(), self.switches);
        let result = 'read: loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break 'read Err(installer.give_up(&mut flash));
            }
            let chunk = match tokio::time::timeout(limits.stall.min(left), stream.chunk()).await {
                Ok(Ok(Some(c))) => c,
                // The body ended, failed, or sent nothing for too long.
                _ => break 'read Err(installer.give_up(&mut flash)),
            };
            let mut piece = chunk.as_slice();
            while !piece.is_empty() {
                match installer.push(piece, &mut flash) {
                    Ok((n, progress)) => {
                        piece = &piece[n..];
                        match progress {
                            Progress::More => {}
                            Progress::Checked => self.report_downloading(hub, &offer.offer_id).await,
                            Progress::Complete(m) => break 'read Ok(m),
                        }
                    }
                    Err(e) => break 'read Err(e),
                }
            }
        };
        // A switch that fired applies once.
        if flash.used_switch {
            drop(flash);
            self.switches.power_cut_after_image_bytes = None;
            self.switches.flash_fails = false;
            self.save_switches()?;
        }
        Ok(result)
    }

    /// `downloading` is sent once, when the image starts arriving. Only a final report is repeated.
    async fn report_downloading(&self, hub: &paired::State, offer_id: &str) {
        let report = FirmwareReport {
            offer_id: offer_id.to_string(),
            state: ReportState::Downloading,
            reason: None,
            firmware_version: store::running_version(&self.state).ok(),
        };
        let _ = tokio::time::timeout(self.limits.stall, paired::firmware_report(hub, &report)).await;
    }
}

/// The spare slot as a file.
struct SlotFlash<'a> {
    state: &'a Path,
    slot: SlotName,
    file: Option<File>,
    written: u64,
    switches: Switches,
    used_switch: bool,
}

impl<'a> SlotFlash<'a> {
    fn new(state: &'a Path, slot: SlotName, switches: Switches) -> Self {
        Self {
            state,
            slot,
            file: None,
            written: 0,
            switches,
            used_switch: false,
        }
    }

    /// A file system error is a failed write: reported as `flash`, and logged.
    fn fail(&mut self, e: std::io::Error) -> FlashError {
        eprintln!(
            "display-simulator: writing app slot {} failed: {e}",
            self.slot.as_str()
        );
        FlashError::Failed
    }
}

impl Flash for SlotFlash<'_> {
    fn begin(&mut self) -> Result<(), FlashError> {
        let erase = || -> std::io::Result<File> {
            let mut ota = store::ota(self.state)?;
            let slot = ota.slot_mut(self.slot);
            slot.state = SlotState::Empty;
            slot.firmware_version = None;
            store::set_ota(self.state, &ota)?;
            let path = store::slot_image(self.state, self.slot);
            std::fs::create_dir_all(store::folder(self.state))?;
            File::create(path)
        };
        match erase() {
            Ok(f) => {
                self.file = Some(f);
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), FlashError> {
        let (bytes, cut) = match self.switches.power_cut_after_image_bytes {
            Some(n) if self.written + bytes.len() as u64 >= n => {
                self.used_switch = true;
                (&bytes[..(n - self.written.min(n)) as usize], true)
            }
            _ => (bytes, false),
        };
        let Some(f) = self.file.as_mut() else {
            return Err(FlashError::Failed);
        };
        if let Err(e) = f.write_all(bytes) {
            return Err(self.fail(e));
        }
        self.written += bytes.len() as u64;
        if cut {
            return Err(FlashError::PowerCut);
        }
        Ok(())
    }

    fn abort(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(store::slot_image(self.state, self.slot));
    }

    fn end(&mut self) -> Result<(), FlashError> {
        if self.switches.flash_fails {
            self.used_switch = true;
            return Err(FlashError::Failed);
        }
        match self.file.take().map(|f| f.sync_all()) {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(self.fail(e)),
            None => Err(FlashError::Failed),
        }
    }
}
