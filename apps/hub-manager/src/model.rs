//! The manager's state and rules, without any GPUI, so they're tested without a window.
//!
//! The view turns clicks and keys into calls here. A call that needs the hub returns an [`Op`], which
//! the view hands to the worker; the worker's answer comes back through [`Model::finish`]. One
//! operation runs at a time, so changes reach the hub in the order they were made.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use hub_client::usb::UsbDisplay;
use hub_client::ClientError;
use hub_core::api::{
    CollectorInvite, CollectorView, DisplayView, EnrolmentGrant, HubStatus, ManualFigures, ManualMetric,
    NewCollector, NewSubscription, SubscriptionChange, SubscriptionView,
};
use hub_core::freshness::{short_duration, Freshness};
use hub_core::model::{clean_note, clean_percent, clean_title};
use hub_core::{Service, Source, Window};
use hub_protocol::limits::{MAX_NOTE_LEN, MAX_PAGES, MAX_TITLE_LEN};
use hub_protocol::pairing::PairingString;
use hub_protocol::provisioning::{Bundle, Wifi, MAX_HOSTS};
use hub_protocol::{DataQuality, Snapshot, Timestamp};
use zeroize::Zeroizing;

use crate::field::Field;
use crate::guidance::{self, Doing, Guidance, Next, Tone};
use crate::settings::SavedHub;

/// How often the open app checks the hub, and how often while waiting for a new display to appear.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);
pub const WATCH_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Pages,
    Displays,
    Settings,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Overview,
        Section::Pages,
        Section::Displays,
        Section::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Pages => "Pages",
            Section::Displays => "Displays",
            Section::Settings => "Settings",
        }
    }
}

/// Everything the hub said on the last refresh.
#[derive(Clone, Debug)]
pub struct Refresh {
    pub status: HubStatus,
    pub pages: Vec<SubscriptionView>,
    pub displays: Vec<DisplayView>,
    pub preview: Snapshot,
    pub helpers: Vec<CollectorView>,
}

/// A serial port a display might be on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub name: String,
    pub description: String,
}

/// Work for the worker. `Debug` never shows a secret.
#[derive(Debug)]
pub enum Op {
    Pair {
        pairing: PairingString,
        label: String,
    },
    Refresh,
    AddPage(NewSubscription),
    ChangePage {
        id: String,
        change: SubscriptionChange,
        figures: Option<ManualFigures>,
    },
    RemovePage {
        id: String,
    },
    Reorder {
        ids: Vec<String>,
    },
    RevokeDisplay {
        id: String,
    },
    NewEnrolment {
        name: String,
    },
    NewHelper(NewCollector),
    RevokeHelper {
        id: String,
    },
    ListPorts,
    SendUsb {
        port: String,
        bundle: Bundle,
    },
    SaveBundle {
        path: PathBuf,
        bundle: Bundle,
    },
    Forget,
}

#[derive(Debug)]
pub enum Outcome {
    Paired {
        hub: SavedHub,
        refresh: Refresh,
    },
    Refreshed(Refresh),
    Enrolment {
        grant: EnrolmentGrant,
        refresh: Refresh,
    },
    HelperInvite {
        invite: CollectorInvite,
        refresh: Refresh,
    },
    Ports(Vec<PortInfo>),
    Sent(UsbDisplay),
    Saved(PathBuf),
    Forgotten,
}

#[derive(Debug)]
pub enum Failure {
    Client(ClientError),
    Store(String),
    Settings(String),
    Usb(hub_client::usb::UsbError),
    File(String),
}

/// What is running, for the busy label and to know what a failure meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    Pairing,
    Refreshing,
    Saving { done: String },
    Enrolling,
    Ports,
    Sending { port: String },
    SavingFile,
    Forgetting,
}

impl Busy {
    pub fn label(&self) -> &'static str {
        match self {
            Busy::Pairing => "Pairing",
            Busy::Refreshing => "Checking the hub",
            Busy::Saving { .. } => "Saving",
            Busy::Enrolling => "Preparing setup details",
            Busy::Ports => "Looking for USB ports",
            Busy::Sending { .. } => "Sending over USB",
            Busy::SavingFile => "Saving the file",
            Busy::Forgetting => "Forgetting the hub",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    RemovePage { id: String, title: String },
    RevokeDisplay { id: String, name: String },
    RevokeHelper { id: String, label: String },
    Forget,
}

impl Confirm {
    pub fn question(&self) -> (String, String, &'static str) {
        match self {
            Confirm::RemovePage { title, .. } => (
                format!("Remove {title}?"),
                "Displays stop showing it on their next poll. Its stored figures are deleted.".into(),
                "Remove page",
            ),
            Confirm::RevokeDisplay { name, .. } => (
                format!("Remove {name}?"),
                "Its credential stops working straight away, and it shows that it needs setting up again. To use it \
                 again, factory-reset it and add it as a new display."
                    .into(),
                "Remove display",
            ),
            Confirm::RevokeHelper { label, .. } => (
                format!("Remove the helper on {label}?"),
                "Its credential stops working straight away. Pages it fed keep their last figures, and go stale. On \
                 that PC, hubctl logout tidies up."
                    .into(),
                "Remove helper",
            ),
            Confirm::Forget => (
                "Forget this hub?".into(),
                "This PC deletes its credential and the hub's details. The hub keeps working. To cut this PC off on \
                 the hub too, run hubd admin list, then hubd admin revoke with this PC's id, on the Pi."
                    .into(),
                "Forget hub",
            ),
        }
    }
}

/// Sources the manager offers.
pub const ADDABLE_SOURCES: [Source; 4] = [Source::Manual, Source::Collector, Source::Demo, Source::None];

/// The sources that make sense for a service: the PC helper can report Claude Max (Claude Code) and
/// ChatGPT Plus (Codex), but nothing reports OpenCode Go.
pub fn sources_for(service: Service) -> Vec<Source> {
    ADDABLE_SOURCES
        .into_iter()
        .filter(|s| *s != Source::Collector || matches!(service, Service::ClaudeMax | Service::ChatgptPlus))
        .collect()
}

pub fn source_label(s: Source) -> &'static str {
    match s {
        Source::Demo => "Demo figures",
        Source::Manual => "Typed in",
        Source::Collector => "PC helper",
        Source::None => "Nothing yet",
    }
}

pub fn quality_tag(q: DataQuality) -> &'static str {
    match q {
        DataQuality::Demo => "DEMO",
        DataQuality::ProviderReported => "REPORTED",
        DataQuality::Estimated => "ESTIMATE",
        DataQuality::Manual => "MANUAL",
        DataQuality::Unavailable | DataQuality::Unrecognised => "UNAVAILABLE",
    }
}

pub struct AddPage {
    pub service: Service,
    pub source: Source,
    pub title: Field,
}

pub struct MetricFields {
    pub window: Window,
    pub percent: Field,
    pub resets_in: Field,
}

pub struct EditPage {
    pub id: String,
    pub service: Service,
    pub title: Field,
    pub source: Source,
    pub metrics: Vec<MetricFields>,
    pub note: Field,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnrolStage {
    Form,
    /// Setup details are ready to send.
    Ready,
    /// Sent over USB or saved; waiting for the display to check in.
    Waiting {
        how: String,
    },
    Joined {
        name: String,
    },
}

pub struct Enrol {
    pub open: bool,
    pub stage: EnrolStage,
    pub name: Field,
    pub ssid: Field,
    pub password: Field,
    pub other_port: Field,
    pub ports: Vec<PortInfo>,
    /// Whether the ports have been looked for since the setup details were made.
    pub scanned: bool,
    pub bundle: Option<Bundle>,
    pub known: Vec<String>,
}

impl Default for Enrol {
    fn default() -> Self {
        let mut name = Field::new(48);
        name.set("Desk");
        Self {
            open: false,
            stage: EnrolStage::Form,
            name,
            ssid: Field::new(32),
            password: Field::secret(63),
            other_port: Field::new(256),
            ports: Vec::new(),
            scanned: false,
            bundle: None,
            known: Vec::new(),
        }
    }
}

/// One page as the Pages list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct PageRow {
    pub id: String,
    pub title: String,
    pub service: &'static str,
    pub source: &'static str,
    pub quality: &'static str,
    pub freshness: String,
    pub stale: bool,
    pub note: Option<String>,
    pub metrics: Vec<MetricRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricRow {
    pub label: String,
    pub used_percent: Option<f64>,
    pub resets: String,
}

/// Setting up a PC helper (`hubctl`).
#[derive(Default)]
pub struct HelperForm {
    pub claude: bool,
    pub chatgpt: bool,
    /// The `dh1.` string for `hubctl login`, once made. A secret until used or expired.
    pub string: Option<Zeroizing<String>>,
    pub expires_at: Option<Timestamp>,
}

pub struct Model {
    pub hub: Option<SavedHub>,
    /// The saved credential stopped working; show the pairing screen with the hub's details kept.
    pub needs_pairing: bool,
    pub section: Section,
    pub last: Option<Refresh>,
    pub refreshed_at: Option<Instant>,
    /// Whether the last call reached the hub. `None` before the first.
    pub online: Option<bool>,
    pub notice: Option<Guidance>,
    pub busy: Option<Busy>,
    pub confirm: Option<Confirm>,
    pub pairing: Field,
    pub pc_name: Field,
    pub add: AddPage,
    pub edit: Option<EditPage>,
    pub enrol: Enrol,
    pub helper: HelperForm,
}

impl Model {
    pub fn new(hub: Option<SavedHub>, pc_name: &str) -> Self {
        let mut name = Field::new(48);
        name.set(pc_name);
        Self {
            hub,
            needs_pairing: false,
            section: Section::Overview,
            last: None,
            refreshed_at: None,
            online: None,
            notice: None,
            busy: None,
            confirm: None,
            pairing: Field::new(512),
            pc_name: name,
            add: AddPage {
                service: Service::ClaudeMax,
                source: Source::Manual,
                title: Field::new(MAX_TITLE_LEN),
            },
            edit: None,
            enrol: Enrol::default(),
            helper: HelperForm {
                claude: true,
                chatgpt: true,
                ..HelperForm::default()
            },
        }
    }

    pub fn connected(&self) -> bool {
        self.hub.is_some() && !self.needs_pairing
    }

    pub fn address(&self) -> String {
        self.hub
            .as_ref()
            .map(|h| format!("{}:{}", h.host, h.port))
            .unwrap_or_default()
    }

    /// The hub's clock now, from its last answer plus the time since.
    pub fn hub_now(&self, now: Instant) -> Option<Timestamp> {
        let r = self.last.as_ref()?;
        let since = now.saturating_duration_since(self.refreshed_at?).as_secs() as i64;
        Some(r.status.now.plus_seconds(since))
    }

    fn start(&mut self, busy: Busy, op: Op) -> Option<Op> {
        if self.busy.is_some() {
            return None;
        }
        self.busy = Some(busy);
        Some(op)
    }

    fn problem(&mut self, g: Guidance) -> Option<Op> {
        self.notice = Some(g);
        None
    }

    // ---- connecting ------------------------------------------------------------------------------

    pub fn submit_pairing(&mut self) -> Option<Op> {
        let pairing = match PairingString::decode(self.pairing.trimmed()) {
            Ok(p) => p,
            Err(e) => return self.problem(guidance::from_pairing(&e)),
        };
        let Some(label) = clean_title(self.pc_name.trimmed()) else {
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: "Give this PC a name".into(),
                detail: "The hub lists each paired PC by name, so you can tell them apart later.".into(),
                next: Next::None,
            });
        };
        self.notice = None;
        self.start(Busy::Pairing, Op::Pair { pairing, label })
    }

    pub fn refresh(&mut self) -> Option<Op> {
        if !self.connected() {
            return None;
        }
        self.start(Busy::Refreshing, Op::Refresh)
    }

    /// A background refresh, when one is due and nothing else is running.
    pub fn refresh_due(&mut self, now: Instant) -> Option<Op> {
        let every = if matches!(self.enrol.stage, EnrolStage::Waiting { .. }) {
            WATCH_EVERY
        } else {
            REFRESH_EVERY
        };
        let due = self
            .refreshed_at
            .is_none_or(|t| now.saturating_duration_since(t) >= every);
        if due && self.confirm.is_none() {
            self.refresh()
        } else {
            None
        }
    }

    pub fn forget(&mut self) -> Option<Op> {
        self.confirm = Some(Confirm::Forget);
        None
    }

    // ---- pages -----------------------------------------------------------------------------------

    pub fn pages(&self) -> &[SubscriptionView] {
        self.last.as_ref().map(|r| r.pages.as_slice()).unwrap_or_default()
    }

    pub fn can_add_page(&self) -> bool {
        self.pages().len() < MAX_PAGES
    }

    /// How soon displays pick up a change, for the confirmation message.
    fn next_poll_text(&self) -> String {
        let s = self
            .last
            .as_ref()
            .map_or(300, |r| r.preview.refresh_after_seconds);
        format!(
            "Displays show it on their next check, within about {}.",
            minutes(s)
        )
    }

    pub fn add_page(&mut self) -> Option<Op> {
        if !self.can_add_page() {
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: format!("Displays show at most {MAX_PAGES} pages"),
                detail: "Remove a page first.".into(),
                next: Next::None,
            });
        }
        let title = match self.add.title.trimmed() {
            "" => None,
            t => match clean_title(t) {
                Some(t) => Some(t),
                None => return self.problem(bad_title()),
            },
        };
        let shown = title
            .clone()
            .unwrap_or_else(|| self.add.service.default_title().into());
        let op = Op::AddPage(NewSubscription {
            service: self.add.service,
            source: self.add.source,
            title,
            id: None,
        });
        let done = format!("Added {shown}. {}", self.next_poll_text());
        let op = self.start(Busy::Saving { done }, op)?;
        self.add.title.clear();
        Some(op)
    }

    /// Moves a page up (`-1`) or down (`1`).
    pub fn move_page(&mut self, id: &str, delta: i32) -> Option<Op> {
        let mut ids: Vec<String> = self.pages().iter().map(|p| p.id.clone()).collect();
        let from = ids.iter().position(|p| p == id)?;
        let to = from as i64 + delta as i64;
        if to < 0 || to >= ids.len() as i64 {
            return None;
        }
        ids.swap(from, to as usize);
        // Shown at once, so repeated presses feel immediate; the hub's answer replaces it.
        if let Some(r) = self.last.as_mut() {
            r.pages.swap(from, to as usize);
        }
        let done = format!("Order saved. {}", self.next_poll_text());
        self.start(Busy::Saving { done }, Op::Reorder { ids })
    }

    pub fn ask_remove_page(&mut self, id: &str) {
        if let Some(p) = self.pages().iter().find(|p| p.id == id) {
            self.confirm = Some(Confirm::RemovePage {
                id: p.id.clone(),
                title: p.title.clone(),
            });
        }
    }

    pub fn begin_edit(&mut self, id: &str, now: Instant) {
        let Some(p) = self.pages().iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let hub_now = self.hub_now(now);
        let stored = p.data.as_ref().filter(|d| d.quality == DataQuality::Manual);
        let metrics = p
            .service
            .windows()
            .iter()
            .map(|&w| {
                let value = stored.and_then(|d| d.metrics.iter().find(|m| m.window == w));
                let mut percent = Field::new(6);
                let mut resets_in = Field::new(24);
                if let Some(v) = value {
                    if let Some(pct) = v.used_percent {
                        percent.set(&format_percent(pct));
                    }
                    if let (Some(at), Some(now)) = (v.resets_at, hub_now) {
                        let left = now.seconds_until(at);
                        if left > 0 {
                            resets_in.set(&short_duration(left));
                        }
                    }
                }
                MetricFields {
                    window: w,
                    percent,
                    resets_in,
                }
            })
            .collect();
        let mut title = Field::new(MAX_TITLE_LEN);
        title.set(&p.title);
        let mut note = Field::new(MAX_NOTE_LEN);
        if let Some(n) = stored.and_then(|d| d.note.as_deref()) {
            note.set(n);
        }
        self.edit = Some(EditPage {
            id: p.id,
            service: p.service,
            title,
            source: p.source,
            metrics,
            note,
        });
    }

    pub fn save_edit(&mut self, now: Instant) -> Option<Op> {
        let hub_now = self.hub_now(now);
        let e = self.edit.as_ref()?;
        let Some(title) = clean_title(e.title.trimmed()) else {
            return self.problem(bad_title());
        };
        let figures = if e.source == Source::Manual {
            let mut metrics = Vec::new();
            for m in &e.metrics {
                let used_percent = match parse_percent(m.percent.trimmed()) {
                    Ok(p) => p,
                    Err(()) => {
                        return self.problem(Guidance {
                            tone: Tone::Problem,
                            title: format!("{} isn't a share from 0 to 100", m.window.label()),
                            detail: "Enter a number such as 42 or 42.5, or leave it empty if you don't know. An empty \
                                     box shows as unknown, never as zero."
                                .into(),
                            next: Next::None,
                        })
                    }
                };
                let resets_at = match parse_duration(m.resets_in.trimmed()) {
                    Ok(None) => None,
                    Ok(Some(secs)) => hub_now.map(|now| now.plus_seconds(secs)),
                    Err(()) => return self.problem(Guidance {
                        tone: Tone::Problem,
                        title: "Reset times look like 3 h 20 min".into(),
                        detail:
                            "Enter how long until the allowance resets, using d, h and min, such as 2 d 4 h \
                                     or 45 min. Leave it empty if you don't know."
                                .into(),
                        next: Next::None,
                    }),
                };
                metrics.push(ManualMetric {
                    window: m.window,
                    used_percent,
                    resets_at,
                });
            }
            let note = match e.note.trimmed() {
                "" => None,
                n => clean_note(n),
            };
            Some(ManualFigures { metrics, note })
        } else {
            None
        };
        let change = SubscriptionChange {
            title: Some(title.clone()),
            source: Some(e.source),
        };
        let op = Op::ChangePage {
            id: e.id.clone(),
            change,
            figures,
        };
        let done = format!("Saved {title}. {}", self.next_poll_text());
        let op = self.start(Busy::Saving { done }, op)?;
        self.edit = None;
        Some(op)
    }

    pub fn page_rows(&self, now: Instant) -> Vec<PageRow> {
        let hub_now = self.hub_now(now);
        self.pages()
            .iter()
            .map(|p| {
                let data = p.data.as_ref();
                let quality = data.map_or(DataQuality::Unavailable, |d| d.quality);
                let (freshness, stale) = match (data.and_then(|d| d.updated_at), hub_now) {
                    _ if p.source == Source::Demo => {
                        ("Sample figures, made fresh on every check".into(), false)
                    }
                    (None, _) => ("No figures yet".into(), false),
                    (Some(at), Some(now)) => {
                        let age = at.seconds_until(now).max(0);
                        let stale = age > p.source.stale_after_seconds() as i64;
                        let text = format!("Updated {} ago", short_duration(age));
                        (if stale { format!("Stale. {text}") } else { text }, stale)
                    }
                    (Some(_), None) => (String::new(), false),
                };
                let metrics = data
                    .map(|d| {
                        d.metrics
                            .iter()
                            .map(|m| MetricRow {
                                label: m.window.label().into(),
                                used_percent: m.used_percent,
                                resets: resets_text(m.resets_at, hub_now),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                PageRow {
                    id: p.id.clone(),
                    title: p.title.clone(),
                    service: p.service.default_title(),
                    source: source_label(p.source),
                    quality: if p.source == Source::Demo {
                        "DEMO"
                    } else {
                        quality_tag(quality)
                    },
                    freshness,
                    stale,
                    note: data.and_then(|d| d.note.clone()),
                    metrics,
                }
            })
            .collect()
    }

    // ---- displays ----------------------------------------------------------------------------------

    pub fn displays(&self) -> &[DisplayView] {
        self.last
            .as_ref()
            .map(|r| r.displays.as_slice())
            .unwrap_or_default()
    }

    pub fn ask_revoke(&mut self, id: &str) {
        if let Some(d) = self.displays().iter().find(|d| d.id == id) {
            self.confirm = Some(Confirm::RevokeDisplay {
                id: d.id.clone(),
                name: d.name.clone(),
            });
        }
    }

    /// "Seen 3 min ago", "Never checked in".
    pub fn seen_text(&self, d: &DisplayView, now: Instant) -> String {
        match (d.last_seen_at, self.hub_now(now)) {
            (Some(at), Some(now)) => format!("Seen {} ago", short_duration(at.seconds_until(now).max(0))),
            (Some(_), None) => "Seen".into(),
            (None, _) => "Not checked in yet".into(),
        }
    }

    pub fn open_enrol(&mut self) -> Option<Op> {
        self.enrol = Enrol {
            open: true,
            ..Enrol::default()
        };
        None
    }

    pub fn create_setup(&mut self) -> Option<Op> {
        let Some(name) = clean_title(self.enrol.name.trimmed()) else {
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: "Give the display a name".into(),
                detail: "Such as Desk or Kitchen, so you can tell displays apart later.".into(),
                next: Next::None,
            });
        };
        if let Err(g) = wifi_of(&self.enrol.ssid, &self.enrol.password) {
            return self.problem(g);
        }
        self.notice = None;
        self.start(Busy::Enrolling, Op::NewEnrolment { name })
    }

    /// The addresses a display should try: the hub's own list, then the address this PC uses.
    pub fn bundle_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .last
            .as_ref()
            .map(|r| r.status.hosts.clone())
            .unwrap_or_default();
        if let Some(h) = &self.hub {
            if !hosts.iter().any(|x| x.eq_ignore_ascii_case(&h.host)) {
                hosts.push(h.host.clone());
            }
        }
        hosts.truncate(MAX_HOSTS);
        hosts
    }

    pub fn list_ports(&mut self) -> Option<Op> {
        self.start(Busy::Ports, Op::ListPorts)
    }

    /// Looks for ports once, as soon as setup details are ready.
    pub fn first_scan(&mut self) -> Option<Op> {
        if self.enrol.stage == EnrolStage::Ready && !self.enrol.scanned && self.busy.is_none() {
            self.enrol.scanned = true;
            return self.list_ports();
        }
        None
    }

    pub fn send_usb(&mut self, port: &str) -> Option<Op> {
        let bundle = self.enrol.bundle.clone()?;
        let port = port.trim().to_string();
        if port.is_empty() {
            return None;
        }
        self.notice = None;
        self.start(Busy::Sending { port: port.clone() }, Op::SendUsb { port, bundle })
    }

    pub fn save_bundle(&mut self, path: PathBuf) -> Option<Op> {
        let bundle = self.enrol.bundle.clone()?;
        self.start(Busy::SavingFile, Op::SaveBundle { path, bundle })
    }

    pub fn close_enrol(&mut self) {
        self.enrol = Enrol::default();
    }

    /// The setup details' expiry, by the hub's clock.
    pub fn setup_expires_in(&self, now: Instant) -> Option<i64> {
        let b = self.enrol.bundle.as_ref()?;
        Some(self.hub_now(now)?.seconds_until(b.enrolment.expires_at))
    }

    // ---- PC helpers -----------------------------------------------------------------------------------

    pub fn helpers(&self) -> &[CollectorView] {
        self.last
            .as_ref()
            .map(|r| r.helpers.as_slice())
            .unwrap_or_default()
    }

    pub fn create_helper(&mut self) -> Option<Op> {
        let services: Vec<Service> = [
            (self.helper.claude, Service::ClaudeMax),
            (self.helper.chatgpt, Service::ChatgptPlus),
        ]
        .into_iter()
        .filter_map(|(on, s)| on.then_some(s))
        .collect();
        if services.is_empty() {
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: "Choose what the helper reports".into(),
                detail: "Claude Max (from Claude Code), ChatGPT Plus (from Codex), or both.".into(),
                next: Next::None,
            });
        }
        let label = clean_title(self.pc_name.trimmed()).unwrap_or_else(|| "PC helper".into());
        self.helper.string = None;
        self.start(
            Busy::Saving { done: String::new() },
            Op::NewHelper(NewCollector { label, services }),
        )
    }

    pub fn ask_revoke_helper(&mut self, id: &str) {
        if let Some(h) = self.helpers().iter().find(|h| h.id == id) {
            self.confirm = Some(Confirm::RevokeHelper {
                id: h.id.clone(),
                label: h.label.clone(),
            });
        }
    }

    // ---- confirmations -------------------------------------------------------------------------------

    pub fn confirm_yes(&mut self) -> Option<Op> {
        let c = self.confirm.take()?;
        match c {
            Confirm::RemovePage { id, title } => {
                let done = format!("Removed {title}. {}", self.next_poll_text());
                if self.edit.as_ref().is_some_and(|e| e.id == id) {
                    self.edit = None;
                }
                self.start(Busy::Saving { done }, Op::RemovePage { id })
            }
            Confirm::RevokeDisplay { id, name } => {
                let done = format!("Removed {name}. Its credential no longer works.");
                self.start(Busy::Saving { done }, Op::RevokeDisplay { id })
            }
            Confirm::RevokeHelper { id, label } => {
                let done = format!("Removed the helper on {label}.");
                self.start(Busy::Saving { done }, Op::RevokeHelper { id })
            }
            Confirm::Forget => self.start(Busy::Forgetting, Op::Forget),
        }
    }

    pub fn confirm_no(&mut self) {
        self.confirm = None;
    }

    // ---- answers ------------------------------------------------------------------------------------

    fn take_refresh(&mut self, r: Refresh, now: Instant) {
        if let EnrolStage::Waiting { .. } = self.enrol.stage {
            if let Some(d) = r.displays.iter().find(|d| !self.enrol.known.contains(&d.id)) {
                self.enrol.stage = EnrolStage::Joined { name: d.name.clone() };
                self.enrol.bundle = None;
            }
        }
        self.last = Some(r);
        self.refreshed_at = Some(now);
        self.online = Some(true);
    }

    pub fn finish(&mut self, result: Result<Outcome, Failure>, now: Instant) {
        let busy = self.busy.take();
        match result {
            Ok(Outcome::Paired { hub, refresh }) => {
                self.pairing.clear();
                self.hub = Some(hub);
                self.needs_pairing = false;
                self.section = Section::Overview;
                self.take_refresh(refresh, now);
                self.notice = Some(Guidance::done(
                    "Paired",
                    "This PC's credential is in the system's credential store. Add pages, then add a display.",
                ));
            }
            Ok(Outcome::Refreshed(r)) => {
                self.take_refresh(r, now);
                if let Some(Busy::Saving { done }) =
                    busy.filter(|b| !matches!(b, Busy::Saving { done } if done.is_empty()))
                {
                    self.notice = Some(Guidance::done(done, ""));
                } else if self.notice.as_ref().is_some_and(|n| n.next == Next::Retry) {
                    // The hub is back.
                    self.notice = None;
                }
            }
            Ok(Outcome::Enrolment { grant, refresh }) => {
                self.take_refresh(refresh, now);
                let wifi = wifi_of(&self.enrol.ssid, &self.enrol.password).ok().flatten();
                self.enrol.password.clear();
                // The port this PC reaches the hub on is known to work; the hub's own idea is the fallback.
                let port = match (self.hub.as_ref(), self.last.as_ref().map(|r| r.status.port)) {
                    (Some(h), _) => h.port,
                    (None, Some(p)) if p != 0 => p,
                    _ => 8443,
                };
                match hub_client::provisioning::bundle(&grant, self.bundle_hosts(), port, wifi) {
                    Ok(b) => {
                        self.enrol.bundle = Some(b);
                        self.enrol.stage = EnrolStage::Ready;
                        self.enrol.known = self.displays().iter().map(|d| d.id.clone()).collect();
                    }
                    Err(e) => {
                        self.notice = Some(Guidance {
                            tone: Tone::Problem,
                            title: "The setup details aren't valid".into(),
                            detail: e,
                            next: Next::None,
                        })
                    }
                }
            }
            Ok(Outcome::HelperInvite { invite, refresh }) => {
                self.take_refresh(refresh, now);
                let string = self.hub.as_ref().and_then(|h| {
                    use base64::Engine;
                    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(&invite.secret)
                        .ok()?;
                    let secret: [u8; 16] = raw.try_into().ok()?;
                    Some(
                        PairingString {
                            host: h.host.clone(),
                            port: h.port,
                            spki_sha256: h.pin()?,
                            secret,
                        }
                        .encode(),
                    )
                });
                match string {
                    Some(s) => {
                        self.helper.string = Some(Zeroizing::new(s));
                        self.helper.expires_at = Some(invite.expires_at);
                    }
                    None => {
                        self.notice = Some(Guidance {
                            tone: Tone::Problem,
                            title: "The hub's answer wasn't understood".into(),
                            detail: "Update the hub and this app to the same release.".into(),
                            next: Next::None,
                        })
                    }
                }
            }
            Ok(Outcome::Ports(p)) => {
                self.enrol.ports = p;
                self.enrol.scanned = true;
            }
            Ok(Outcome::Sent(display)) => {
                let port = match busy {
                    Some(Busy::Sending { port }) => port,
                    _ => String::new(),
                };
                self.enrol.stage = EnrolStage::Waiting {
                    how: format!("Sent to {port}"),
                };
                self.notice = Some(Guidance::done(
                    format!("Sent to the display on {port}"),
                    format!(
                        "{} {} accepted the setup details. It joins Wi-Fi and checks in with the hub; this page \
                         updates when it does.",
                        display.device.device_type, display.device.firmware_version
                    ),
                ));
            }
            Ok(Outcome::Saved(path)) => {
                self.enrol.stage = EnrolStage::Waiting {
                    how: format!("Saved to {}", path.display()),
                };
                self.notice = Some(Guidance::done(
                    "Setup file saved",
                    "It holds a one-time secret and any Wi-Fi password. Delete it once the display has checked in.",
                ));
            }
            Ok(Outcome::Forgotten) => {
                let name = self.pc_name.text.clone();
                *self = Model::new(None, &name);
                self.notice = Some(Guidance::done(
                    "Forgot the hub",
                    "Paste a new pairing string to connect again.",
                ));
            }
            Err(f) => self.failed(f, busy, now),
        }
    }

    fn failed(&mut self, f: Failure, busy: Option<Busy>, now: Instant) {
        let doing = if busy == Some(Busy::Pairing) {
            Doing::Pairing
        } else {
            Doing::Other
        };
        let g = match &f {
            Failure::Client(e) => {
                let address = match (&busy, &self.hub) {
                    (_, Some(h)) => format!("{}:{}", h.host, h.port),
                    _ => PairingString::decode(self.pairing.trimmed())
                        .map(|p| format!("{}:{}", p.host, p.port))
                        .unwrap_or_default(),
                };
                if e.is_connection() {
                    self.online = Some(false);
                    // Don't retry straight away.
                    self.refreshed_at = Some(now);
                }
                guidance::from_client(e, doing, &address)
            }
            Failure::Store(d) => guidance::from_store(d),
            Failure::Settings(d) | Failure::File(d) => Guidance {
                tone: Tone::Problem,
                title: "Couldn't save on this PC".into(),
                detail: d.clone(),
                next: Next::None,
            },
            Failure::Usb(e) => guidance::from_usb(e),
        };
        if g.next == Next::PairAgain && self.hub.is_some() {
            self.needs_pairing = true;
        }
        // A reorder shown early is put back by the next refresh.
        if matches!(busy, Some(Busy::Saving { .. })) {
            self.refreshed_at = None;
        }
        self.notice = Some(g);
    }

    /// For Settings: a summary to paste into a support message. Never a credential or Wi-Fi details.
    pub fn diagnostics(&self, app_version: &str, os: &str) -> String {
        let mut out = vec![format!("Display Hub manager {app_version} on {os}")];
        if let Some(h) = &self.hub {
            out.push(format!("Hub: {}:{}", h.host, h.port));
            out.push(format!("Hub key: {}", h.fingerprint()));
        } else {
            out.push("Hub: not paired".into());
        }
        if let Some(r) = &self.last {
            let s = &r.status;
            out.push(format!(
                "Hub version {}; clock {}; {} pages, {} displays, {} admins",
                s.version,
                match s.clock_synchronised {
                    Some(true) => "synchronised",
                    Some(false) => "NOT synchronised",
                    None => "unknown",
                },
                s.subscriptions,
                s.displays,
                s.admins
            ));
        }
        out.push(format!(
            "Reachable: {}",
            match self.online {
                Some(true) => "yes",
                Some(false) => "no",
                None => "not checked",
            }
        ));
        if let Some(n) = &self.notice {
            out.push(format!("Last message: {}", n.title));
        }
        out.join("\n")
    }
}

fn bad_title() -> Guidance {
    Guidance {
        tone: Tone::Problem,
        title: format!("Titles are 1 to {MAX_TITLE_LEN} characters"),
        detail: "Plain text only.".into(),
        next: Next::None,
    }
}

fn wifi_of(ssid: &Field, password: &Field) -> Result<Option<Wifi>, Guidance> {
    let name = ssid.trimmed();
    if name.is_empty() {
        if !password.text.is_empty() {
            return Err(Guidance {
                tone: Tone::Problem,
                title: "Enter the Wi-Fi network's name".into(),
                detail: "A password needs a network name. Leave both empty if the display already has Wi-Fi."
                    .into(),
                next: Next::None,
            });
        }
        return Ok(None);
    }
    let len = password.text.len();
    if name.len() > 32 || (len != 0 && !(8..=63).contains(&len)) {
        return Err(Guidance {
            tone: Tone::Problem,
            title: "Those Wi-Fi details won't work".into(),
            detail: "Network names are up to 32 bytes, and WPA2 passwords are 8 to 63 characters. Leave the password \
                     empty for an open network. Displays use 2.4 GHz networks."
                .into(),
            next: Next::None,
        });
    }
    Ok(Some(Wifi {
        ssid: name.into(),
        password: password.text.clone(),
    }))
}

fn minutes(secs: u32) -> String {
    match secs {
        0..=89 => "a minute".into(),
        _ => format!("{} minutes", secs.div_ceil(60)),
    }
}

fn format_percent(p: f64) -> String {
    if p.fract() == 0.0 {
        format!("{p:.0}")
    } else {
        format!("{p:.1}")
    }
}

/// "42", "42.5" or "42 %". Empty means unknown.
pub fn parse_percent(s: &str) -> Result<Option<f64>, ()> {
    let s = s.trim().trim_end_matches('%').trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<f64>().ok().and_then(clean_percent).map(Some).ok_or(())
}

/// "3 h 20 min", "2d 4h", "45m". Empty means unknown. At most 31 days.
pub fn parse_duration(s: &str) -> Result<Option<i64>, ()> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Ok(None);
    }
    let mut total = 0i64;
    let mut rest = s.as_str();
    let mut any = false;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 || digits > 4 {
            return Err(());
        }
        let n: i64 = rest[..digits].parse().map_err(|_| ())?;
        rest = rest[digits..].trim_start();
        let unit_len = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = match &rest[..unit_len] {
            "d" | "day" | "days" => 86_400,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            _ => return Err(()),
        };
        rest = rest[unit_len..].trim_start_matches([' ', ',']);
        total += n * unit;
        any = true;
    }
    if !any || total <= 0 || total > 31 * 86_400 {
        return Err(());
    }
    Ok(Some(total))
}

pub fn resets_text(at: Option<Timestamp>, now: Option<Timestamp>) -> String {
    match (at, now) {
        (Some(at), Some(now)) => {
            let left = now.seconds_until(at);
            if left <= 0 {
                "Reset due".into()
            } else {
                format!("Resets in {}", short_duration(left))
            }
        }
        _ => String::new(),
    }
}

/// How a preview page reads on the Overview: its freshness on the hub's clock.
pub fn preview_freshness(page: &hub_protocol::Page, snapshot: &Snapshot) -> (String, bool) {
    match Freshness::of(page, snapshot.generated_at, 0) {
        Freshness::Never => ("No figures yet".into(), false),
        Freshness::Fresh { age_seconds } => (format!("Updated {} ago", short_duration(age_seconds)), false),
        Freshness::Stale { age_seconds } => (
            format!("Stale. Updated {} ago", short_duration(age_seconds)),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_core::api::HubIdentity;
    use hub_core::model::{MetricValue, PageData};

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn page(id: &str, source: Source, data: Option<PageData>) -> SubscriptionView {
        SubscriptionView {
            id: id.into(),
            service: Service::ClaudeMax,
            title: id.into(),
            source,
            data,
        }
    }

    fn refresh(pages: Vec<SubscriptionView>) -> Refresh {
        Refresh {
            status: HubStatus {
                version: "0.1.0".into(),
                started_at: ts("2026-11-10T19:00:00Z"),
                now: ts("2026-11-10T20:00:00Z"),
                clock_synchronised: Some(true),
                identity: HubIdentity {
                    certificate_pem: String::new(),
                    spki_sha256: "ab".repeat(32),
                    server_name: String::new(),
                },
                hosts: vec!["raspberrypi.local".into()],
                port: 8443,
                subscriptions: pages.len(),
                displays: 0,
                admins: 1,
            },
            pages,
            displays: vec![],
            preview: Snapshot {
                protocol_version: 1,
                message_type: "dashboard.snapshot".into(),
                dashboard_id: "llm-usage".into(),
                generated_at: ts("2026-11-10T20:00:00Z"),
                refresh_after_seconds: 300,
                pages: vec![],
            },
            helpers: vec![],
        }
    }

    fn connected(pages: Vec<SubscriptionView>) -> (Model, Instant) {
        let hub = SavedHub::new(
            "192.168.1.20",
            8443,
            &[0xab; 32],
            "Desk PC",
            ts("2026-11-10T19:00:00Z"),
        );
        let mut m = Model::new(Some(hub), "Desk PC");
        let now = Instant::now();
        m.finish(Ok(Outcome::Refreshed(refresh(pages))), now);
        (m, now)
    }

    #[test]
    fn a_bad_pairing_string_explains_itself_and_sends_nothing() {
        let mut m = Model::new(None, "PC");
        m.pairing.set("hello");
        assert!(m.submit_pairing().is_none());
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .title
            .contains("isn't a pairing string"));
        let good = PairingString {
            host: "pi.local".into(),
            port: 8443,
            spki_sha256: [1; 32],
            secret: [2; 16],
        };
        m.pairing.set(&format!("  {}  ", good.encode()));
        let Some(Op::Pair { pairing, label }) = m.submit_pairing() else {
            panic!("pair")
        };
        assert_eq!((pairing, label.as_str()), (good, "PC"));
        assert!(m.submit_pairing().is_none(), "one operation at a time");
    }

    #[test]
    fn reordering_is_shown_at_once_and_sends_the_whole_order() {
        let (mut m, _) = connected(vec![
            page("a", Source::Demo, None),
            page("b", Source::Demo, None),
            page("c", Source::Demo, None),
        ]);
        assert!(m.move_page("a", -1).is_none(), "already first");
        let Some(Op::Reorder { ids }) = m.move_page("c", -1) else {
            panic!("reorder")
        };
        assert_eq!(ids, ["a", "c", "b"]);
        assert_eq!(
            m.pages().iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
        // A failure leaves the next refresh due, which puts the hub's order back.
        m.finish(Err(Failure::Client(ClientError::Timeout)), Instant::now());
        assert!(m.refresh_due(Instant::now()).is_some());
    }

    #[test]
    fn adding_respects_the_page_limit_and_reports_success() {
        let (mut m, now) = connected(vec![]);
        m.add.title.set("Work Claude");
        let Some(Op::AddPage(new)) = m.add_page() else {
            panic!("add")
        };
        assert_eq!(
            (new.title.as_deref(), new.source),
            (Some("Work Claude"), Source::Manual)
        );
        assert!(m.add.title.text.is_empty());
        m.finish(
            Ok(Outcome::Refreshed(refresh(vec![page("x", Source::Manual, None)]))),
            now,
        );
        let n = m.notice.clone().unwrap();
        assert_eq!(n.tone, Tone::Done);
        assert!(
            n.title.contains("Added Work Claude") && n.title.contains("5 minutes"),
            "{}",
            n.title
        );

        let (mut full, _) = connected(
            (0..MAX_PAGES)
                .map(|i| page(&format!("p{i}"), Source::Demo, None))
                .collect(),
        );
        assert!(full.add_page().is_none());
        assert!(full.notice.unwrap().title.contains("at most 10"));
    }

    #[test]
    fn manual_figures_parse_and_unknown_stays_unknown() {
        let data = PageData {
            quality: DataQuality::Manual,
            updated_at: Some(ts("2026-11-10T19:00:00Z")),
            metrics: vec![MetricValue {
                window: Window::FiveHour,
                used_percent: Some(42.0),
                resets_at: Some(ts("2026-11-10T22:00:00Z")),
            }],
            note: Some("From the website".into()),
        };
        let (mut m, now) = connected(vec![page("claude-max", Source::Manual, Some(data))]);
        m.begin_edit("claude-max", now);
        let e = m.edit.as_mut().unwrap();
        assert_eq!(e.metrics[0].percent.text, "42");
        assert_eq!(e.metrics[0].resets_in.text, "2 h");
        assert_eq!(e.metrics[1].percent.text, "", "weekly was never entered");
        e.metrics[1].percent.set("abc");
        assert!(m.save_edit(now).is_none());
        assert!(m.notice.as_ref().unwrap().title.contains("0 to 100"));
        let e = m.edit.as_mut().unwrap();
        e.metrics[1].percent.set("");
        e.metrics[1].resets_in.set("2 d 4 h");
        let Some(Op::ChangePage {
            figures: Some(f),
            change,
            ..
        }) = m.save_edit(now)
        else {
            panic!("save")
        };
        assert_eq!(change.source, Some(Source::Manual));
        assert_eq!(f.metrics[0].used_percent, Some(42.0));
        assert_eq!(f.metrics[1].used_percent, None, "empty is unknown, not zero");
        assert_eq!(f.metrics[1].resets_at, Some(ts("2026-11-13T00:00:00Z")));
        assert_eq!(f.note.as_deref(), Some("From the website"));
    }

    #[test]
    fn parsing_rules() {
        assert_eq!(parse_percent(""), Ok(None));
        assert_eq!(parse_percent("42.55 %"), Ok(Some(42.6)));
        assert!(
            parse_percent("101").is_err() && parse_percent("-1").is_err() && parse_percent("NaN").is_err()
        );
        assert_eq!(parse_duration("3h 20min"), Ok(Some(12_000)));
        assert_eq!(parse_duration("2 days, 1 hour"), Ok(Some(176_400)));
        assert_eq!(parse_duration("45m"), Ok(Some(2700)));
        for bad in ["3", "h", "3 weeks", "0 h", "40 d", "99999h"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn page_rows_label_quality_and_staleness() {
        let old = PageData {
            quality: DataQuality::Manual,
            updated_at: Some(ts("2026-11-08T20:00:00Z")),
            metrics: vec![MetricValue {
                window: Window::Weekly,
                used_percent: None,
                resets_at: Some(ts("2026-11-10T19:00:00Z")),
            }],
            note: None,
        };
        let (m, now) = connected(vec![
            page("m", Source::Manual, Some(old)),
            page("d", Source::Demo, None),
            page("n", Source::None, None),
        ]);
        let rows = m.page_rows(now);
        assert_eq!((rows[0].quality, rows[0].stale), ("MANUAL", true));
        assert_eq!(rows[0].freshness, "Stale. Updated 2 d ago");
        assert_eq!(rows[0].metrics[0].used_percent, None);
        assert_eq!(rows[0].metrics[0].resets, "Reset due");
        assert_eq!(rows[1].quality, "DEMO");
        assert_eq!(
            (rows[2].quality, rows[2].freshness.as_str()),
            ("UNAVAILABLE", "No figures yet")
        );
    }

    #[test]
    fn errors_map_to_screens() {
        let (mut m, now) = connected(vec![]);
        m.refreshed_at = None;
        m.refresh_due(now).unwrap();
        m.finish(Err(Failure::Client(ClientError::Unreachable("x".into()))), now);
        assert_eq!(m.online, Some(false));
        assert!(m.notice.as_ref().unwrap().detail.contains("192.168.1.20:8443"));
        assert!(m.refresh_due(now).is_none(), "no instant retry loop");
        m.refresh_due(now + REFRESH_EVERY).unwrap();
        m.finish(Ok(Outcome::Refreshed(refresh(vec![]))), now + REFRESH_EVERY);
        assert!(m.notice.is_none(), "back online clears the problem");

        m.refresh().unwrap();
        let revoked = ClientError::Status {
            status: 401,
            body: Some(hub_protocol::ErrorBody::new(
                hub_protocol::ErrorCode::TokenRevoked,
                "revoked",
            )),
        };
        m.finish(Err(Failure::Client(revoked)), now);
        assert!(
            m.needs_pairing && !m.connected(),
            "a revoked PC is asked to pair again"
        );
    }

    #[test]
    fn setting_up_a_display_builds_a_bundle_and_watches_for_it() {
        let (mut m, now) = connected(vec![]);
        m.open_enrol();
        m.enrol.ssid.set("Home");
        m.enrol.password.set("short");
        assert!(m.create_setup().is_none());
        m.enrol.password.set("long enough");
        let Some(Op::NewEnrolment { name }) = m.create_setup() else {
            panic!("enrol")
        };
        assert_eq!(name, "Desk");
        let grant = EnrolmentGrant {
            secret: "s".repeat(22),
            expires_at: ts("2026-11-10T20:10:00Z"),
            identity: HubIdentity {
                certificate_pem: "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n".into(),
                spki_sha256: "ab".repeat(32),
                server_name: hub_protocol::provisioning::HUB_SERVER_NAME.into(),
            },
        };
        m.finish(
            Ok(Outcome::Enrolment {
                grant,
                refresh: refresh(vec![]),
            }),
            now,
        );
        let b = m.enrol.bundle.as_ref().expect("bundle");
        assert_eq!(b.hub.hosts, ["raspberrypi.local", "192.168.1.20"]);
        assert_eq!(b.wifi.as_ref().unwrap().password, "long enough");
        assert!(m.enrol.password.text.is_empty(), "the password field is wiped");
        assert_eq!(m.setup_expires_in(now), Some(600));

        assert!(matches!(m.first_scan(), Some(Op::ListPorts)));
        m.finish(Ok(Outcome::Ports(vec![])), now);
        assert!(
            m.first_scan().is_none(),
            "no ports found is an answer, not a reason to look forever"
        );
        let Some(Op::SendUsb { port, .. }) = m.send_usb("COM3") else {
            panic!("send")
        };
        assert_eq!(port, "COM3");
        let display = UsbDisplay {
            device: display_simulator_like(),
            provisioned: false,
        };
        m.finish(Ok(Outcome::Sent(display)), now);
        assert!(matches!(m.enrol.stage, EnrolStage::Waiting { .. }));
        assert!(
            m.refresh_due(now + WATCH_EVERY).is_some(),
            "watches closely while waiting"
        );
        let mut r = refresh(vec![]);
        r.displays.push(DisplayView {
            id: "d-1".into(),
            name: "Desk".into(),
            enrolled_at: ts("2026-11-10T20:01:00Z"),
            last_seen_at: None,
            device: None,
        });
        m.finish(Ok(Outcome::Refreshed(r)), now + WATCH_EVERY);
        assert_eq!(m.enrol.stage, EnrolStage::Joined { name: "Desk".into() });
        assert!(m.enrol.bundle.is_none(), "the used secret is dropped");
    }

    fn display_simulator_like() -> hub_protocol::device::DeviceInfo {
        hub_protocol::device::DeviceInfo {
            device_type: "llm-usage-display".into(),
            firmware_version: "1.0.0".into(),
            hardware_revision: "x".into(),
            protocol_version: 1,
        }
    }

    #[test]
    fn diagnostics_hold_no_secrets() {
        let (mut m, _) = connected(vec![]);
        m.enrol.password.set("wifi-password-123");
        m.pairing.set("dh1.secretstuff.00000000");
        let d = m.diagnostics("0.1.0", "windows");
        assert!(d.contains("192.168.1.20:8443") && d.contains("synchronised"));
        assert!(!d.contains("wifi-password") && !d.contains("secretstuff"));
    }

    #[test]
    fn confirmations_guard_destructive_actions() {
        let (mut m, _) = connected(vec![page("a", Source::Demo, None)]);
        m.ask_remove_page("a");
        assert!(m.confirm.as_ref().unwrap().question().0.contains("Remove a?"));
        m.confirm_no();
        assert!(m.confirm_yes().is_none());
        m.ask_remove_page("a");
        assert!(matches!(m.confirm_yes(), Some(Op::RemovePage { .. })));
        m.busy = None;
        m.forget();
        assert!(matches!(m.confirm_yes(), Some(Op::Forget)));
        m.finish(Ok(Outcome::Forgotten), Instant::now());
        assert!(m.hub.is_none() && m.last.is_none());
        assert_eq!(m.pc_name.text, "Desk PC");
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn only_services_a_helper_can_report_offer_it() {
        assert!(sources_for(Service::ClaudeMax).contains(&Source::Collector));
        assert!(sources_for(Service::ChatgptPlus).contains(&Source::Collector));
        assert!(!sources_for(Service::OpencodeGoPlus).contains(&Source::Collector));
    }

    #[test]
    fn a_helper_needs_a_service() {
        let mut m = Model::new(None, "PC");
        m.helper.claude = false;
        m.helper.chatgpt = false;
        assert!(m.create_helper().is_none());
        m.helper.chatgpt = true;
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!((n.label.as_str(), n.services), ("PC", vec![Service::ChatgptPlus]));
    }
}

#[cfg(test)]
mod helper_string_tests {
    use super::*;
    use hub_core::api::HubIdentity;

    #[test]
    fn the_helper_string_names_this_hub_and_carries_the_invite() {
        let t = Timestamp::parse("2026-11-11T01:00:00Z").unwrap();
        let hub = SavedHub::new("raspberrypi.local", 8443, &[7; 32], "PC", t);
        let mut m = Model::new(Some(hub), "PC");
        let op = m.create_helper();
        assert!(matches!(op, Some(Op::NewHelper(_))));
        use base64::Engine;
        let secret = [9u8; 16];
        let invite = CollectorInvite {
            secret: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret),
            expires_at: t.plus_seconds(600),
            identity: HubIdentity {
                certificate_pem: String::new(),
                spki_sha256: String::new(),
                server_name: String::new(),
            },
        };
        let refresh = Refresh {
            status: HubStatus {
                version: "0".into(),
                started_at: t,
                now: t,
                clock_synchronised: None,
                identity: invite.identity.clone(),
                hosts: vec![],
                port: 8443,
                subscriptions: 0,
                displays: 0,
                admins: 1,
            },
            pages: vec![],
            displays: vec![],
            preview: Snapshot {
                protocol_version: 1,
                message_type: "dashboard.snapshot".into(),
                dashboard_id: "llm-usage".into(),
                generated_at: t,
                refresh_after_seconds: 300,
                pages: vec![],
            },
            helpers: vec![],
        };
        m.finish(Ok(Outcome::HelperInvite { invite, refresh }), Instant::now());
        let s = m.helper.string.clone().expect("a string");
        // As shown: grouped with spaces, which the decoder ignores.
        let shown: String = s
            .as_bytes()
            .chunks(24)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let p = PairingString::decode(&shown).unwrap();
        assert_eq!(
            (p.host.as_str(), p.port, p.spki_sha256, p.secret),
            ("raspberrypi.local", 8443, [7; 32], secret)
        );
        assert!(
            m.notice.is_none(),
            "no message for a new string; the panel shows it"
        );
    }
}
