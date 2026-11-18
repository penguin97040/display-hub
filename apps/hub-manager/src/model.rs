//! The manager's state and rules, without any GPUI, so they're tested without a window.
//!
//! The view turns clicks and keys into calls here. A call that needs the hub returns an [`Op`], which
//! the view hands to the worker; the worker's answer comes back through [`Model::finish`]. One
//! operation runs at a time, so changes reach the hub in the order they were made.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hub_client::usb::UsbDisplay;
use hub_client::ClientError;
use hub_core::api::{
    DisplayView, EnrolmentGrant, HelperInvite, HelperView, HubStatus, NewHelper, NewPage, PageChange,
    PageContentInput, PageView,
};
use hub_core::freshness::{short_duration, Freshness};
use hub_core::model::{clean_title, feed_rule, valid_feed};
use hub_core::Source;
use hub_protocol::limits::{MAX_PAGES, MAX_TITLE_LEN};
use hub_protocol::pairing::PairingString;
use hub_protocol::provisioning::{Bundle, Wifi, MAX_HOSTS};
use hub_protocol::{DataQuality, Snapshot, Timestamp};
use zeroize::Zeroizing;

use hub_templates::Template;
use usage::{clean_percent, Service};

use crate::field::Field;
use crate::forms::{ContentEdit, HelperPageForm, TemplateForm};
use crate::guidance::{self, Doing, Guidance, Next, Tone};
use crate::settings::SavedHub;
use crate::shown::{kind_label, origin_text, Shown};

pub mod pi_install;
mod updates;
pub use updates::*;

/// The most pages the hub holds. Each display shows at most its first ten ticked ones. This is the
/// hub's own limit (`hubd::store::MAX_STORED_PAGES`); a test keeps the two equal, and the hub
/// refuses a page past its limit whatever this says.
pub const HUB_PAGE_LIMIT: usize = 50;

/// How often the open app checks the hub, and how often while waiting for a new display to appear.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30);
pub const WATCH_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Overview,
    Pages,
    Displays,
    Firmware,
    Settings,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::Overview,
        Section::Pages,
        Section::Displays,
        Section::Firmware,
        Section::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Pages => "Pages",
            Section::Displays => "Displays",
            Section::Firmware => "Firmware",
            Section::Settings => "Settings",
        }
    }
}

/// Everything the hub said on the last refresh.
#[derive(Clone, Debug)]
pub struct Refresh {
    pub status: HubStatus,
    pub pages: Vec<PageView>,
    pub displays: Vec<DisplayView>,
    pub preview: Snapshot,
    pub helpers: Vec<HelperView>,
    /// For each display (by id), the ids of the pages ticked on it. A display removed while this was
    /// being gathered has no entry, and isn't in `displays` either.
    pub ticks: HashMap<String, Vec<String>>,
    /// Firmware packages, signer names and publishers. `None` from a hub without signed updates.
    pub firmware: Option<Shelf>,
    /// Why the firmware lists couldn't be read from a hub that has them. The model then keeps the
    /// lists it had, rather than taking the hub for one without signed updates.
    pub firmware_error: Option<std::sync::Arc<ClientError>>,
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
    /// Adds these pages, one after another, in order.
    AddPages {
        pages: Vec<NewPage>,
    },
    /// Changes a page, then sets what it shows if the edit typed in new content.
    SaveEdit {
        id: String,
        change: PageChange,
        content: Option<PageContentInput>,
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
    /// Sets the pages ticked on one display, all at once.
    SetDisplayPages {
        display: String,
        ids: Vec<String>,
    },
    NewEnrolment {
        name: String,
    },
    NewHelper(NewHelper),
    /// Remembers whether the setup checklist was closed, in `manager.json`.
    SaveGuide {
        dismissed: bool,
    },
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
    /// Checks a firmware file here, then uploads it.
    UploadFirmware {
        path: PathBuf,
    },
    RemoveFirmware {
        id: String,
    },
    /// The Update button.
    OfferFirmware {
        display: String,
        package: String,
    },
    CancelOffer {
        display: String,
    },
    /// The owner's name for a signing key; `None` removes it.
    SetSignerLabel {
        key_id: String,
        label: Option<String>,
    },
    NewPublisher {
        label: String,
    },
    RevokePublisher {
        id: String,
    },
    /// Reads and checks a package for a USB install.
    LoadPackage {
        from: PackageSource,
    },
    /// Install firmware over USB, then send the setup details.
    Install(Box<InstallJob>),
}

#[derive(Debug)]
pub enum Outcome {
    Paired {
        hub: SavedHub,
        refresh: Option<Refresh>,
    },
    /// A refresh, or a change followed by one. After a change the refresh may be `None` (it failed
    /// but the change was made), and the next ordinary refresh brings the lists up to date.
    Refreshed(Option<Refresh>),
    /// Pages were added. `failed` is set when one of them wasn't, and the ones after it weren't tried.
    Added {
        /// `None` when the list couldn't be read again afterwards: the pages are on the hub, so the
        /// next ordinary refresh brings the list up to date.
        refresh: Option<Refresh>,
        failed: Option<AddFailure>,
    },
    Enrolment {
        grant: EnrolmentGrant,
        refresh: Option<Refresh>,
    },
    HelperInvite {
        invite: HelperInvite,
        refresh: Option<Refresh>,
    },
    GuideSaved,
    Ports(Vec<PortInfo>),
    Sent(UsbDisplay),
    Saved(PathBuf),
    Forgotten,
    Uploaded {
        package: Box<hub_core::api::FirmwarePackageView>,
        /// `false` when the hub already had it.
        new: bool,
        refresh: Option<Refresh>,
    },
    PublisherInvite {
        invite: hub_core::api::PublisherInvite,
        refresh: Option<Refresh>,
    },
    PackageLoaded(Box<crate::firmware::Loaded>),
    Installed {
        after: AfterFlash,
        refresh: Option<Refresh>,
        /// What became of the owner's name for the maker's key. The install counts either way.
        label: MakerName,
    },
}

/// The page that couldn't be added, and why.
#[derive(Debug)]
pub struct AddFailure {
    /// How many pages were added before it.
    pub added: usize,
    pub total: usize,
    pub title: String,
    pub error: ClientError,
}

#[derive(Debug)]
pub enum Failure {
    Client(ClientError),
    Store(String),
    Settings(String),
    Usb(hub_client::usb::UsbError),
    File(String),
    /// A file to read that couldn't be.
    ReadFile(String),
    /// A firmware package that failed this app's checks, in words.
    Package(String),
    Install(crate::firmware::InstallError),
}

/// What is running, for the busy label and to know what a failure meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    Pairing,
    Refreshing,
    Saving {
        done: String,
    },
    /// Offering an update to a display; `done` is said once it is made.
    Offering {
        done: String,
    },
    /// Saving a name for a signing key; `done` is said once it is saved.
    Naming {
        done: String,
    },
    /// Adding pages from a template, or a page for the maker's own helper.
    Adding {
        done: String,
        setup: Option<SetupNote>,
        feed: Option<FeedSteps>,
    },
    Enrolling,
    Ports,
    Sending {
        port: String,
        /// Whether keys from an install were waiting: sent with these details (`Some(true)`), or
        /// left out (`Some(false)`).
        keys: Option<bool>,
    },
    SavingFile,
    SavingGuide,
    Forgetting,
    Uploading,
    /// Reading a package for a USB install.
    Loading,
    Installing {
        port: String,
    },
}

impl Busy {
    pub fn label(&self) -> &'static str {
        match self {
            Busy::Pairing => "Pairing",
            Busy::Refreshing => "Checking the hub",
            Busy::Saving { .. } | Busy::Naming { .. } => "Saving",
            Busy::Offering { .. } => "Offering the update",
            Busy::Adding { .. } => "Adding",
            Busy::Enrolling => "Preparing setup details",
            Busy::Ports => "Looking for USB ports",
            Busy::Sending { .. } => "Sending over USB",
            Busy::SavingFile => "Saving the file",
            Busy::SavingGuide => "Saving",
            Busy::Forgetting => "Forgetting the hub",
            Busy::Uploading => "Uploading firmware",
            Busy::Loading => "Checking the package",
            Busy::Installing { .. } => "Installing firmware",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirm {
    RemovePage {
        id: String,
        title: String,
    },
    RevokeDisplay {
        id: String,
        name: String,
    },
    RevokeHelper {
        id: String,
        label: String,
    },
    Forget,
    RemovePackage {
        id: String,
        title: String,
        detail: String,
    },
    /// Update a display. `second` is the further question for an older version or a pre-release.
    Update {
        display: String,
        package: String,
        title: String,
        detail: String,
        says: PackageSays,
        second: Option<(String, String)>,
    },
    UpdateAgain {
        display: String,
        package: String,
        title: String,
        detail: String,
    },
    RevokePublisher {
        id: String,
        label: String,
    },
    /// Install firmware over USB, which erases the display.
    Install {
        port: String,
        title: String,
        detail: String,
        says: PackageSays,
    },
}

/// What a package says about itself, shown apart from the app's own sentences and marked as the
/// maker's: a package can't name itself or speak in the app's voice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackageSays {
    /// What each value is, and the maker's value: ("Device type", "llm-usage-display").
    pub facts: Vec<(String, String)>,
    /// The manifest's `summary`.
    pub note: Option<String>,
}

impl PackageSays {
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.note.is_none()
    }
}

impl Confirm {
    /// The package's own words, for the questions that show them.
    pub fn package_says(&self) -> Option<&PackageSays> {
        match self {
            Confirm::Update { says, .. } | Confirm::Install { says, .. } => {
                Some(says).filter(|s| !s.is_empty())
            }
            _ => None,
        }
    }

    pub fn question(&self) -> (String, String, &'static str) {
        match self {
            Confirm::RemovePage { title, .. } => (
                format!("Remove {title}?"),
                "Displays stop showing it on their next poll. What it holds is deleted.".into(),
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
                 that computer, usage-helper logout or hub-send logout tidies up."
                    .into(),
                "Remove helper",
            ),
            Confirm::RemovePackage { title, detail, .. } => {
                (format!("Remove {title}?"), detail.clone(), "Remove package")
            }
            Confirm::Update { title, detail, .. } => (title.clone(), detail.clone(), "Update"),
            Confirm::UpdateAgain { title, detail, .. } => (title.clone(), detail.clone(), "Install anyway"),
            Confirm::RevokePublisher { label, .. } => (
                format!("Remove the publisher {label}?"),
                "Its credential stops working straight away, so it can't add packages any more. Packages it added \
                 stay on the hub."
                    .into(),
                "Remove publisher",
            ),
            Confirm::Install { title, detail, .. } => (title.clone(), detail.clone(), "Erase and install"),
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

/// Where a page's content comes from, as the edit form words it.
pub fn source_label(s: Source) -> &'static str {
    match s {
        Source::Demo => "Demo",
        Source::Manual => "Typed in",
        Source::Helper => "Helper",
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

/// What to do once, by hand, after adding pages: a command to run on the hub's Pi, shown with a Copy
/// button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupNote {
    pub help: String,
    pub command: String,
    /// The command was put on the clipboard.
    pub copied: bool,
}

/// The longest name `display-hub-helper` takes for a helper on the Pi (it becomes part of a user
/// name). A longer feed is given to it with `--feed`, under a shorter name.
pub const PI_HELPER_NAME_LEN: usize = 20;

/// How to feed a page made for the maker's own helper, shown once it is added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedSteps {
    pub feed: String,
    pub kind: String,
    /// The helper's name on the Pi, which is also the program's file name in `pi_command`.
    pub pi_name: String,
    /// Sets the helper up on the hub's Pi, run in the folder that holds its program.
    pub pi_command: String,
    /// Sends one page from another computer once `hub-send login` has paired it, with the page's
    /// data as JSON on standard input.
    pub send_command: String,
    /// Data of the page's kind to try `send_command` with. `None` for a kind of the maker's own.
    pub example: Option<&'static str>,
    pub pi_copied: bool,
    pub send_copied: bool,
}

/// The Pi helper's name for a feed: the feed itself when it fits, else its start and a short hash of
/// the whole feed, so long feeds sharing a start get different names, the same each time.
fn pi_helper_name(feed: &str) -> String {
    if feed.len() <= PI_HELPER_NAME_LEN {
        return feed.to_string();
    }
    // FNV-1a, 32 bits, written as seven base-36 digits at most.
    let mut hash: u32 = 0x811c_9dc5;
    for b in feed.bytes() {
        hash = (hash ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    let mut tail = String::new();
    let mut rest = hash;
    while rest > 0 {
        tail.insert(0, char::from_digit(rest % 36, 36).unwrap_or('0'));
        rest /= 36;
    }
    if tail.is_empty() {
        tail.push('0');
    }
    // A feed is a slug, so its start is usable once a cut-off hyphen is dropped.
    let room = PI_HELPER_NAME_LEN - tail.len() - 1;
    let start = feed[..room].trim_end_matches('-');
    format!("{start}-{tail}")
}

impl FeedSteps {
    pub fn new(feed: &str, kind: &str) -> Self {
        let pi_name = pi_helper_name(feed);
        let mut pi_command = format!("sudo display-hub-helper add {pi_name} --run \"$PWD/{pi_name}\"");
        if pi_name != feed {
            pi_command.push_str(&format!(" --feed {feed}"));
        }
        Self {
            feed: feed.to_string(),
            kind: kind.to_string(),
            send_command: format!("hub-send page {feed} --kind {kind} --data -"),
            example: example_data(kind),
            pi_name,
            pi_command,
            pi_copied: false,
            send_copied: false,
        }
    }
}

/// Small, valid data for each of the four shapes, for a first try of a helper.
pub fn example_data(kind: &str) -> Option<&'static str> {
    use hub_protocol::shapes::{KIND_BIG_NUMBER, KIND_LIST, KIND_METER, KIND_TEXT};
    Some(match kind {
        KIND_METER => r#"{"items": [{"id": "tank", "label": "Tank", "percent": 80}]}"#,
        KIND_BIG_NUMBER => r#"{"value": 21.5, "decimals": 1, "unit": "°C"}"#,
        KIND_TEXT => r#"{"text": "Hello from my helper."}"#,
        KIND_LIST => r#"{"items": [{"text": "Bins", "detail": "Tuesday"}]}"#,
        _ => return None,
    })
}

/// Adding pages from a template, or a page for the maker's own helper: pick one, fill in its form,
/// then perhaps a setup step.
pub struct AddFlow {
    /// The templates that come with the app.
    pub templates: Vec<Template>,
    /// The chosen template's form. Without one (or `helper_page`), the picker shows.
    pub form: Option<TemplateForm>,
    /// The form for a page the maker's own helper fills.
    pub helper_page: Option<HelperPageForm>,
    /// What to do next, after pages were added.
    pub setup: Option<SetupNote>,
    /// How to feed the page just made for the maker's own helper.
    pub feed: Option<FeedSteps>,
}

/// A page being edited.
pub struct EditPage {
    pub id: String,
    pub kind: String,
    pub feed: Option<String>,
    /// The template the page was made from, if it has one.
    pub template_id: Option<String>,
    /// Its settings form, when the app has the template.
    pub form: Option<TemplateForm>,
    pub original_title: String,
    pub title: Field,
    /// Where the content comes from, for a page with no settings form to say.
    pub source: Source,
    /// Whether there are sample figures to offer.
    pub can_demo: bool,
    /// An editor for typed-in content, for the four shapes.
    pub content: Option<ContentEdit>,
    /// What the last save found wrong with the typed-in content, shown under the editor.
    pub content_error: Option<String>,
}

impl EditPage {
    /// Where the content will come from once saved: the template's answers decide, when it has a form.
    pub fn effective_source(&self) -> Source {
        self.form
            .as_ref()
            .and_then(|f| f.check().ok())
            .and_then(|made| pick_page(made, &self.kind, &self.feed, &self.original_title))
            .map_or(self.source, |p| p.source)
    }

    /// The sources to offer for a page with no settings form.
    pub fn source_choices(&self) -> Vec<Source> {
        let mut out = vec![Source::Manual];
        if self.can_demo {
            out.push(Source::Demo);
        }
        if self.feed.is_some() {
            out.push(Source::Helper);
        }
        out.push(Source::None);
        if !out.contains(&self.source) {
            out.push(self.source);
        }
        out
    }

    /// Whether the title is its own box. A template that asks for a title in its form has it there.
    pub fn has_title_box(&self) -> bool {
        self.form
            .as_ref()
            .is_none_or(|f| f.template.field("title").is_none())
    }
}

/// Why an edit can't be saved yet.
enum EditError {
    /// About the settings form: shown under it.
    Form(String),
    /// About typed-in content: shown under its editor.
    Content(String),
    /// About anything else: shown as a message.
    Notice(Guidance),
}

/// What saving an edit sends.
struct EditPlan {
    title: String,
    change: PageChange,
    content: Option<PageContentInput>,
}

const KIND_CHANGE_TEXT: &str =
    "These settings would change what kind of page this is, or the feed that fills it. \
     A page's kind and feed can't be changed, so remove this page and add it again from the template.";

/// The page the template's answers make for an existing page: the one of the same kind and feed.
/// Where a template makes several like it, the one with the page's title, else the first.
fn pick_page(made: Vec<NewPage>, kind: &str, feed: &Option<String>, title: &str) -> Option<NewPage> {
    let mut same: Vec<NewPage> = made
        .into_iter()
        .filter(|p| p.kind == kind && p.feed == *feed)
        .collect();
    // New pages are numbered when a title is taken ("Temp 2"), so match the title without that too.
    let unnumbered = title
        .rsplit_once(' ')
        .filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|(base, _)| base);
    let at = same
        .iter()
        .position(|p| p.title == title)
        .or_else(|| unnumbered.and_then(|base| same.iter().position(|p| p.title == base)));
    match at {
        Some(at) => Some(same.swap_remove(at)),
        None if same.is_empty() => None,
        None => Some(same.remove(0)),
    }
}

fn typed_title(typed: &str) -> Result<String, EditError> {
    if typed.chars().count() > MAX_TITLE_LEN {
        return Err(EditError::Notice(bad_title()));
    }
    clean_title(typed).ok_or_else(|| EditError::Notice(bad_title()))
}

/// Works out what saving the edit sends, or what is wrong with it.
fn plan_edit(e: &EditPage, page: &PageView, hub_now: Option<Timestamp>) -> Result<EditPlan, EditError> {
    let typed = e.title.trimmed();
    let mut change = PageChange::default();
    let (title, source) = match &e.form {
        Some(form) => {
            let made = form.check().map_err(EditError::Form)?;
            let mine = pick_page(made, &page.kind, &page.feed, &page.title)
                .ok_or_else(|| EditError::Form(KIND_CHANGE_TEXT.into()))?;
            let title = if e.has_title_box() && typed != page.title {
                typed_title(typed)?
            } else {
                // The title follows the settings only when they change it: a page that was renamed
                // by hand keeps its name through an edit that doesn't touch what names it.
                let before = form
                    .template
                    .make_pages(&page.settings.clone().unwrap_or_default())
                    .ok()
                    .and_then(|m| pick_page(m, &page.kind, &page.feed, &page.title))
                    .map(|p| p.title);
                if before.as_deref() == Some(mine.title.as_str()) {
                    page.title.clone()
                } else {
                    mine.title.clone()
                }
            };
            let settings = mine.settings.clone().unwrap_or_default();
            if page.settings.clone().unwrap_or_default() != settings {
                change.settings = Some(settings);
            }
            (title, mine.source)
        }
        None => (typed_title(typed)?, e.source),
    };
    change.title = Some(title.clone());
    if source != page.source {
        change.source = Some(source);
    }
    let mut content = None;
    if source == Source::Manual {
        if let Some(editor) = &e.content {
            let stored =
                page.source == Source::Manual && page.content.as_ref().is_some_and(|c| c.data.is_some());
            // Content is only rebuilt when something was typed (the boxes show rounded text, so
            // rebuilding an untouched page would alter it), or when the page's content now comes
            // from the editor. A page with nothing typed in yet and a blank editor stays as it is.
            let rebuild = editor.is_edited() || page.source != Source::Manual;
            if rebuild && (stored || !editor.is_blank()) {
                let built = editor
                    .build(hub_now)
                    .map_err(|p| EditError::Content(format!("{}. {}", p.title, p.detail)))?;
                let unchanged = page.source == Source::Manual
                    && page
                        .content
                        .as_ref()
                        .is_some_and(|c| c.data == built.data && c.note == built.note);
                if !unchanged {
                    content = Some(built);
                }
            }
        }
    }
    Ok(EditPlan {
        title,
        change,
        content,
    })
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
    /// "Big number", "Custom: nearby-flights/1".
    pub kind: String,
    /// Where the content comes from, in words.
    pub origin: String,
    pub quality: &'static str,
    pub freshness: String,
    pub stale: bool,
    pub note: Option<String>,
    pub shown: Shown,
    /// Which displays show it, in words. `None` when there are no displays yet.
    pub on_displays: Option<String>,
}

/// One page in a display's "Pages on this display" list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickRow {
    pub id: String,
    pub title: String,
    /// What the page is, in words ("Big number"), to tell pages with one title apart.
    pub kind: String,
    pub ticked: bool,
    /// Ticked, but past the first ten, so the display doesn't show it yet.
    pub waiting: bool,
    /// Whether the box can be changed: unticking always can, ticking only below the limit.
    pub can_change: bool,
}

/// A display's "Pages on this display" list, with any changes not yet saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickView {
    pub rows: Vec<TickRow>,
    pub ticked: usize,
    pub dirty: bool,
    /// Ten are ticked, so the rest can't be.
    pub full: bool,
    /// Whether this can be saved: at most ten ticked.
    pub can_save: bool,
}

impl TickView {
    /// "3 of 10 ticked".
    pub fn count_text(&self) -> String {
        format!("{} of {MAX_PAGES}", self.ticked)
    }

    /// The plain note under the list, if it has something to say.
    pub fn note(&self) -> Option<String> {
        if self.ticked > MAX_PAGES {
            let extra = self.ticked - MAX_PAGES;
            Some(format!(
                "A display shows at most {MAX_PAGES} pages. This one has {} ticked and shows only the first \
                 {MAX_PAGES}. Untick {extra} to save.",
                self.ticked
            ))
        } else if self.full && self.rows.len() > MAX_PAGES {
            Some(format!(
                "A display shows at most {MAX_PAGES} pages. Untick one to choose another."
            ))
        } else {
            None
        }
    }
}

/// Inviting a helper on another computer: `usage-helper` for the usage feeds, or `hub-send` for
/// any feed.
#[derive(Default)]
pub struct HelperForm {
    pub claude: bool,
    pub chatgpt: bool,
    /// Other feeds ticked, from the hub's helper pages.
    pub feeds: Vec<String>,
    /// More feed names typed in, separated by spaces or commas, for feeds no page has yet.
    pub other: Field,
    /// The `dh1.` string for `usage-helper login` or `hub-send login`, once made. A secret until
    /// used or expired.
    pub string: Option<Zeroizing<String>>,
    pub expires_at: Option<Timestamp>,
    /// The feeds the last string asked for, to say which program takes it.
    pub invited: Vec<String>,
    /// The title of the problem this form last showed, so a string made later clears that one
    /// notice and no other.
    pub problem: Option<String>,
}

/// A feed that a helper page on the hub takes, to tick when inviting a helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedChoice {
    pub feed: String,
    /// The titles of the pages it fills, to say what it is.
    pub pages: Vec<String>,
    pub ticked: bool,
}

/// Room for the "other feeds" field: the longest legal list (every page's feed, one space between)
/// with plenty to spare, so a legal list is never cut. Text that fills it is refused, not cut.
const HELPER_OTHER_MAX: usize = 4096;

/// The usage feeds, which have boxes of their own.
const USAGE_FEEDS: [Service; 2] = [Service::ClaudeMax, Service::ChatgptPlus];

/// Whether a feed is one of the usage helper's, which have their own boxes.
fn is_usage_feed(feed: &str) -> bool {
    USAGE_FEEDS.iter().any(|s| s.slug() == feed)
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
    pub add: AddFlow,
    pub edit: Option<EditPage>,
    /// Tick changes not yet saved: per display, page id to whether it should be ticked. Only what
    /// the person changed, so a page added meanwhile follows the hub.
    pub tick_changes: HashMap<String, HashMap<String, bool>>,
    pub enrol: Enrol,
    pub helper: HelperForm,
    /// The person closed the setup checklist (kept in `manager.json`).
    pub guide_dismissed: bool,
    /// A change to the above that couldn't be saved yet because another operation was running.
    guide_save_pending: Option<bool>,
    /// The install command was put on the clipboard.
    pub install_copied: bool,
    /// The Firmware screen.
    pub fw: FirmwareUi,
    /// The keys from an install whose setup details weren't sent, for the next Add a display to
    /// the same port.
    pub pending_firmware: Option<PendingKeys>,
    /// The owner has been shown that the hub's firmware lists couldn't be read, since they were
    /// last readable.
    lists_told: bool,
}

/// The one command that sets the hub up, run on the Pi from the folder the files were copied into.
/// `docs/deploy-raspberry-pi.md` shows the same line (a test keeps them together). A release build
/// shows [`pi_install::built_in`] instead, which fetches the files too.
pub const INSTALL_COMMAND: &str = "sudo bash display-hub-pi/install.sh";
/// For a pairing string that has expired or was lost.
pub const NEW_PAIRING_COMMAND: &str = "sudo -u display-hub hubd pair-admin";

/// One line of the setup checklist on the Overview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideItem {
    pub title: &'static str,
    pub help: &'static str,
    /// Ticked off by what the hub really has, not by clicking.
    pub done: bool,
    /// Where to go to do it.
    pub go: Section,
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
            add: AddFlow {
                templates: hub_templates::built_in(),
                form: None,
                helper_page: None,
                setup: None,
                feed: None,
            },
            edit: None,
            tick_changes: HashMap::new(),
            enrol: Enrol::default(),
            helper: HelperForm {
                claude: true,
                chatgpt: true,
                other: Field::new(HELPER_OTHER_MAX),
                ..HelperForm::default()
            },
            guide_dismissed: false,
            guide_save_pending: None,
            install_copied: false,
            fw: FirmwareUi::default(),
            pending_firmware: None,
            lists_told: false,
        }
    }

    /// Starts with the setup checklist already closed or not, as `manager.json` says.
    pub fn with_guide_dismissed(mut self, dismissed: bool) -> Self {
        self.guide_dismissed = dismissed;
        self
    }

    // ---- the setup guide -------------------------------------------------------------------------

    /// No hub has ever been paired here: show the whole first-run guide, not a bare pairing screen.
    pub fn first_run(&self) -> bool {
        self.hub.is_none()
    }

    /// The install step's command: the one line that downloads, checks and installs, in a release
    /// build; in a developer build, the command to run once the folder is on the Pi.
    pub fn install_command(&self) -> &'static str {
        pi_install::built_in().unwrap_or(INSTALL_COMMAND)
    }

    /// Whether the install step's command downloads the files itself (a release build).
    pub fn install_downloads(&self) -> bool {
        pi_install::built_in().is_some()
    }

    /// The text to put on the clipboard for the install step, and a note that it was copied.
    pub fn copy_install(&mut self) -> &'static str {
        self.install_copied = true;
        self.install_command()
    }

    /// The steps left after pairing, ticked from what the hub has. `None` when it shouldn't show:
    /// closed, not paired, or the hub hasn't answered yet.
    pub fn guide_items(&self) -> Option<Vec<GuideItem>> {
        if self.guide_dismissed || !self.connected() || self.last.is_none() {
            return None;
        }
        let shows = self
            .displays()
            .iter()
            .any(|d| self.shown_ids(&d.id).is_some_and(|ids| !ids.is_empty()));
        Some(vec![
            GuideItem {
                title: "Add a display",
                help: "Plug it into this PC with a USB cable. The app sets it up for you.",
                done: !self.displays().is_empty(),
                go: Section::Displays,
            },
            GuideItem {
                title: "Add a page",
                help: "A page is one thing to show, such as your usage limits. Pick one from a template.",
                done: !self.pages().is_empty(),
                go: Section::Pages,
            },
            GuideItem {
                title: "Choose what a display shows",
                help: "Tick the pages you want on each display.",
                done: shows,
                go: Section::Displays,
            },
        ])
    }

    /// The first checklist step not done yet, if any.
    pub fn guide_next(&self) -> Option<usize> {
        self.guide_items()?.iter().position(|i| !i.done)
    }

    /// Closes the checklist and remembers it.
    pub fn dismiss_guide(&mut self) -> Option<Op> {
        self.set_guide(true)
    }

    /// Settings: brings the guide back.
    pub fn show_guide_again(&mut self) -> Option<Op> {
        self.section = Section::Overview;
        self.set_guide(false)
    }

    /// Shows or hides the checklist at once and saves the choice: now, or as soon as the operation
    /// running has finished, so it is never lost.
    fn set_guide(&mut self, dismissed: bool) -> Option<Op> {
        self.guide_dismissed = dismissed;
        self.guide_save_pending = Some(dismissed);
        self.save_guide_if_pending()
    }

    /// Saves a checklist choice that had to wait, once nothing else is running.
    pub fn save_guide_if_pending(&mut self) -> Option<Op> {
        if self.busy.is_some() {
            return None;
        }
        let dismissed = self.guide_save_pending.take()?;
        self.start(Busy::SavingGuide, Op::SaveGuide { dismissed })
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
        } else if self.watching_offers(now) {
            FIRMWARE_WATCH_EVERY
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

    pub fn pages(&self) -> &[PageView] {
        self.last.as_ref().map(|r| r.pages.as_slice()).unwrap_or_default()
    }

    pub fn can_add_page(&self) -> bool {
        self.pages().len() < HUB_PAGE_LIMIT
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

    /// The hub is full, or adding this many pages would fill it past its limit.
    fn page_limit_problem(&self, adding: usize) -> Option<Guidance> {
        let have = self.pages().len();
        if have + adding <= HUB_PAGE_LIMIT {
            return None;
        }
        Some(Guidance {
            tone: Tone::Problem,
            title: format!("The hub holds at most {HUB_PAGE_LIMIT} pages"),
            detail: if adding > 1 {
                format!("This would add {adding} pages, and the hub has {have}. Remove some pages first.")
            } else {
                "Remove a page first.".into()
            },
            next: Next::None,
        })
    }

    /// Chooses a template from the picker, and shows its form.
    pub fn pick_template(&mut self, i: usize) {
        if let Some(g) = self.page_limit_problem(1) {
            self.notice = Some(g);
            return;
        }
        if let Some(t) = self.add.templates.get(i) {
            self.add.form = Some(TemplateForm::new(t.clone(), None));
            self.add.setup = None;
            self.add.feed = None;
            self.notice = None;
        }
    }

    /// Chooses "A page for your own helper" from the picker, and shows its form.
    pub fn open_helper_page(&mut self) {
        if let Some(g) = self.page_limit_problem(1) {
            self.notice = Some(g);
            return;
        }
        self.add.helper_page = Some(HelperPageForm::default());
        self.add.setup = None;
        self.add.feed = None;
        self.notice = None;
    }

    /// Back to the list of templates.
    pub fn leave_form(&mut self) {
        self.add.form = None;
        self.add.helper_page = None;
    }

    /// Checks the form for a page the maker's own helper fills, and adds it.
    pub fn submit_helper_page(&mut self) -> Option<Op> {
        let form = self.add.helper_page.as_mut()?;
        let mut page = match form.check() {
            Ok(p) => p,
            Err(e) => {
                form.error = Some(e);
                return None;
            }
        };
        form.error = None;
        let taken: Vec<String> = self.pages().iter().map(|p| p.title.clone()).collect();
        page.title = unique_title(&page.title, &taken);
        if let Some(g) = self.page_limit_problem(1) {
            self.notice = Some(g);
            return None;
        }
        let feed = page.feed.clone().unwrap_or_default();
        let done = format!(
            "Added {}. It stays empty until a helper sends to the feed {feed}.",
            page.title
        );
        let steps = FeedSteps::new(&feed, &page.kind);
        self.start(
            Busy::Adding {
                done,
                setup: None,
                feed: Some(steps),
            },
            Op::AddPages { pages: vec![page] },
        )
    }

    /// Notes that one of the commands for feeding the new page was copied.
    pub fn copy_feed_command(&mut self, pi: bool) {
        if let Some(f) = self.add.feed.as_mut() {
            if pi {
                f.pi_copied = true;
            } else {
                f.send_copied = true;
            }
        }
    }

    /// What Escape closes: the innermost thing shown on the current section, else the notice.
    pub fn escape(&mut self) {
        let idle = self.busy.is_none();
        if self.confirm.is_some() {
            self.confirm_no();
            return;
        }
        match self.section {
            Section::Pages => {
                if self.edit.is_some() {
                    self.edit = None;
                } else if self.add.setup.is_some() || self.add.feed.is_some() {
                    self.finish_setup();
                } else if self.add.form.is_some() || self.add.helper_page.is_some() {
                    self.leave_form();
                } else {
                    self.notice = None;
                }
            }
            Section::Displays if self.enrol.open && idle => self.close_enrol(),
            Section::Firmware => {
                if self.fw.label_edit.is_some() {
                    self.cancel_label();
                } else if self.fw.install.open && idle {
                    self.close_install();
                } else {
                    self.notice = None;
                }
            }
            _ => self.notice = None,
        }
    }

    /// Goes to Settings with the helper form ready to invite a helper for just this feed.
    pub fn invite_for_feed(&mut self, feed: &str) {
        self.helper.claude = feed == Service::ClaudeMax.slug();
        self.helper.chatgpt = feed == Service::ChatgptPlus.slug();
        // A usage feed has its own box, ticked above.
        self.helper.feeds = if is_usage_feed(feed) {
            Vec::new()
        } else {
            vec![feed.to_string()]
        };
        self.helper.other.clear();
        self.helper.string = None;
        self.section = Section::Settings;
        self.confirm = None;
    }

    /// Checks the form and adds the pages it makes.
    pub fn submit_add(&mut self) -> Option<Op> {
        let form = self.add.form.as_mut()?;
        let mut pages = match form.check() {
            Ok(p) => p,
            Err(e) => {
                form.error = Some(e);
                return None;
            }
        };
        form.error = None;
        let setup = form.template.setup.clone();
        // Pages with the same title can't be told apart in the lists, so a new one gets a number.
        let mut taken: Vec<String> = self.pages().iter().map(|p| p.title.clone()).collect();
        for p in &mut pages {
            p.title = unique_title(&p.title, &taken);
            taken.push(p.title.clone());
        }
        if let Some(g) = self.page_limit_problem(pages.len()) {
            self.notice = Some(g);
            return None;
        }
        let shown = match pages.as_slice() {
            [one] => one.title.clone(),
            many => format!("{} pages", many.len()),
        };
        let done = format!("Added {shown}. {}", self.next_poll_text());
        let setup = setup.map(|s| SetupNote {
            help: s.help,
            command: s.command,
            copied: false,
        });
        self.start(
            Busy::Adding {
                done,
                setup,
                feed: None,
            },
            Op::AddPages { pages },
        )
    }

    /// Notes that the setup command was copied (the button puts it on the clipboard).
    pub fn copy_setup(&mut self) {
        if let Some(s) = self.add.setup.as_mut() {
            s.copied = true;
        }
    }

    /// Done with the setup step: back to the list of templates.
    pub fn finish_setup(&mut self) {
        self.add.setup = None;
        self.add.feed = None;
        self.add.form = None;
        self.add.helper_page = None;
    }

    /// Clears messages about what was typed once someone types again.
    pub fn typing(&mut self) {
        if let Some(f) = self.add.form.as_mut() {
            f.error = None;
        }
        if let Some(f) = self.add.helper_page.as_mut() {
            f.error = None;
        }
        if let Some(f) = self.edit.as_mut().and_then(|e| e.form.as_mut()) {
            f.error = None;
        }
        if let Some(e) = self.edit.as_mut() {
            e.content_error = None;
        }
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
        // Sample figures only exist for usage pages.
        let can_demo = p.source == Source::Demo || usage::usage_service(&p).is_some();
        let template = p
            .template
            .as_deref()
            .and_then(|id| self.add.templates.iter().find(|t| t.id == id))
            .cloned();
        let form = template.map(|t| TemplateForm::new(t, Some(&p.settings.clone().unwrap_or_default())));
        let mut title = Field::new(MAX_TITLE_LEN * 4);
        title.set(&p.title);
        self.edit = Some(EditPage {
            content: ContentEdit::for_page(&p, hub_now),
            content_error: None,
            id: p.id,
            kind: p.kind,
            feed: p.feed,
            template_id: p.template,
            form,
            original_title: p.title,
            title,
            source: p.source,
            can_demo,
        });
    }

    /// Checks the edit and saves it: the title, where the content comes from, the template's
    /// settings, and typed-in content.
    pub fn save_edit(&mut self, now: Instant) -> Option<Op> {
        let hub_now = self.hub_now(now);
        let e = self.edit.as_ref()?;
        let Some(page) = self.pages().iter().find(|p| p.id == e.id).cloned() else {
            self.edit = None;
            return self.problem(Guidance {
                tone: Tone::Problem,
                title: "That page is no longer on the hub".into(),
                detail: "It may have been removed from another PC.".into(),
                next: Next::None,
            });
        };
        let plan = match plan_edit(e, &page, hub_now) {
            Ok(p) => p,
            Err(EditError::Form(msg)) => {
                if let Some(f) = self.edit.as_mut().and_then(|e| e.form.as_mut()) {
                    f.error = Some(msg);
                }
                return None;
            }
            Err(EditError::Content(msg)) => {
                if let Some(e) = self.edit.as_mut() {
                    e.content_error = Some(msg);
                }
                return None;
            }
            Err(EditError::Notice(g)) => return self.problem(g),
        };
        let done = format!("Saved {}. {}", plan.title, self.next_poll_text());
        let op = self.start(
            Busy::Saving { done },
            Op::SaveEdit {
                id: page.id,
                change: plan.change,
                content: plan.content,
            },
        )?;
        self.edit = None;
        Some(op)
    }

    /// The note under a page on the Overview. The hub's "no source" wording is for displays; here
    /// it says what the page is waiting for, by where its content comes from.
    pub fn preview_note(&self, page: &hub_protocol::Page) -> Option<String> {
        let note = page.note.clone()?;
        if note != hub_core::build::NO_SOURCE_NOTE {
            return Some(note);
        }
        let source = self.pages().iter().find(|p| p.id == page.id).map(|p| p.source);
        Some(
            match source {
                Some(Source::Manual) => "Nothing typed in yet",
                Some(Source::None) => "No source chosen yet",
                _ => return Some(note),
            }
            .to_string(),
        )
    }

    pub fn page_rows(&self, now: Instant) -> Vec<PageRow> {
        let hub_now = self.hub_now(now);
        self.pages()
            .iter()
            .map(|p| {
                let content = p.content.as_ref();
                let quality = content.map_or(DataQuality::Unavailable, |c| c.quality);
                let updated = content.and_then(|c| c.updated_at);
                let (freshness, stale) = match (p.source, updated, hub_now) {
                    (Source::Demo, _, _) => ("Sample content".into(), false),
                    (Source::Manual, None, _) => ("Nothing typed in yet".into(), false),
                    (Source::Helper, None, _) => ("Waiting for the helper's first update".into(), false),
                    (_, None, _) => ("No content yet".into(), false),
                    (_, Some(at), Some(now)) => {
                        let age = at.seconds_until(now).max(0);
                        let stale = age > i64::from(p.stale_after_seconds);
                        let text = format!("Updated {} ago", short_duration(age));
                        (if stale { format!("Stale. {text}") } else { text }, stale)
                    }
                    (_, Some(_), None) => (String::new(), false),
                };
                PageRow {
                    id: p.id.clone(),
                    title: p.title.clone(),
                    kind: kind_label(&p.kind),
                    origin: origin_text(p.source, p.feed.as_deref()),
                    quality: if p.source == Source::Demo {
                        "DEMO"
                    } else {
                        quality_tag(quality)
                    },
                    freshness,
                    stale,
                    note: content.and_then(|c| c.note.clone()),
                    shown: Shown::of_data(&p.kind, content.and_then(|c| c.data.as_ref()), hub_now),
                    on_displays: self.on_displays_text(&p.id),
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

    /// The ids of the pages a display really shows: the first ten ticked, in the shared order. `None`
    /// if the last refresh had nothing for it.
    fn shown_ids(&self, display: &str) -> Option<Vec<&str>> {
        let ticked = self.last.as_ref()?.ticks.get(display)?;
        Some(
            self.pages()
                .iter()
                .map(|p| p.id.as_str())
                .filter(|id| ticked.iter().any(|t| t == id))
                .take(MAX_PAGES)
                .collect(),
        )
    }

    /// Which displays show a page, as the Pages list words it. `None` when there are no displays.
    pub fn on_displays_text(&self, page: &str) -> Option<String> {
        // Nothing to say with no displays, or when the hub doesn't say what its displays show.
        if self.displays().is_empty() || self.displays().iter().all(|d| self.shown_ids(&d.id).is_none()) {
            return None;
        }
        let names: Vec<&str> = self
            .displays()
            .iter()
            .filter(|d| self.shown_ids(&d.id).is_some_and(|ids| ids.contains(&page)))
            .map(|d| d.name.as_str())
            .collect();
        Some(if names.is_empty() {
            "Not on any display".into()
        } else {
            format!("On: {}", names.join(", "))
        })
    }

    /// The pages ticked on a display now: what the hub has, with any unsaved changes on top.
    fn effective_ticks(&self, display: &str) -> Option<HashSet<String>> {
        let saved = self.last.as_ref()?.ticks.get(display)?;
        let mut on: HashSet<String> = saved.iter().cloned().collect();
        for (page, want) in self.tick_changes.get(display).into_iter().flatten() {
            if *want {
                on.insert(page.clone());
            } else {
                on.remove(page);
            }
        }
        Some(on)
    }

    /// A display's "Pages on this display" list. `None` if the hub's answer had nothing for it.
    pub fn tick_view(&self, display: &str) -> Option<TickView> {
        let on = self.effective_ticks(display)?;
        let ticked = self.pages().iter().filter(|p| on.contains(&p.id)).count();
        let mut seen = 0;
        let rows = self
            .pages()
            .iter()
            .map(|p| {
                let is_on = on.contains(&p.id);
                if is_on {
                    seen += 1;
                }
                TickRow {
                    id: p.id.clone(),
                    title: p.title.clone(),
                    kind: crate::shown::kind_label(&p.kind),
                    ticked: is_on,
                    waiting: is_on && seen > MAX_PAGES,
                    can_change: is_on || ticked < MAX_PAGES,
                }
            })
            .collect();
        Some(TickView {
            rows,
            ticked,
            dirty: self.tick_changes.get(display).is_some_and(|c| !c.is_empty()),
            full: ticked >= MAX_PAGES,
            can_save: ticked <= MAX_PAGES,
        })
    }

    /// Ticks or unticks a page on a display. Ticking past ten does nothing.
    pub fn toggle_tick(&mut self, display: &str, page: &str) {
        let Some(on) = self.effective_ticks(display) else {
            return;
        };
        if !self.pages().iter().any(|p| p.id == page) {
            return;
        }
        let want = !on.contains(page);
        let ticked = self.pages().iter().filter(|p| on.contains(&p.id)).count();
        if want && ticked >= MAX_PAGES {
            return;
        }
        let saved = self
            .last
            .as_ref()
            .and_then(|r| r.ticks.get(display))
            .is_some_and(|t| t.iter().any(|x| x == page));
        let changes = self.tick_changes.entry(display.to_string()).or_default();
        // Back to what the hub has: nothing to save for this page.
        if want == saved {
            changes.remove(page);
        } else {
            changes.insert(page.to_string(), want);
        }
        if changes.is_empty() {
            self.tick_changes.remove(display);
        }
    }

    /// Drops a display's unsaved tick changes.
    pub fn cancel_ticks(&mut self, display: &str) {
        self.tick_changes.remove(display);
    }

    /// Saves a display's ticks. Nothing is sent when nothing changed, or when more than ten are ticked.
    pub fn save_ticks(&mut self, display: &str) -> Option<Op> {
        let view = self.tick_view(display)?;
        if !view.dirty || !view.can_save {
            return None;
        }
        let name = self.displays().iter().find(|d| d.id == display)?.name.clone();
        let ids: Vec<String> = view
            .rows
            .iter()
            .filter(|r| r.ticked)
            .map(|r| r.id.clone())
            .collect();
        let secs = self
            .last
            .as_ref()
            .map_or(300, |r| r.preview.refresh_after_seconds);
        let done = format!(
            "Saved the pages on {name}. It shows them on its next check, within about {}.",
            minutes(secs)
        );
        self.start(
            Busy::Saving { done },
            Op::SetDisplayPages {
                display: display.to_string(),
                ids,
            },
        )
    }

    /// New pages the hub ticked on a display that already shows ten, so it doesn't show them yet.
    fn held_back(&self, old: &[String], new: &Refresh) -> String {
        let fresh: Vec<&str> = new
            .pages
            .iter()
            .map(|p| p.id.as_str())
            .filter(|id| !old.iter().any(|o| o == id))
            .collect();
        let mut out = Vec::new();
        for d in &new.displays {
            let Some(ticked) = new.ticks.get(&d.id) else {
                continue;
            };
            let shown: Vec<&str> = new
                .pages
                .iter()
                .map(|p| p.id.as_str())
                .filter(|id| ticked.iter().any(|t| t == id))
                .take(MAX_PAGES)
                .collect();
            if fresh
                .iter()
                .any(|id| ticked.iter().any(|t| t == id) && !shown.contains(id))
            {
                out.push(format!(
                    "Not shown on {} yet: it already shows {MAX_PAGES} pages.",
                    d.name
                ));
            }
        }
        out.join(" ")
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

    /// The port a display should use: the one this PC reaches the hub on is known to work; the
    /// hub's own idea is the fallback.
    pub fn bundle_port(&self) -> u16 {
        match (self.hub.as_ref(), self.last.as_ref().map(|r| r.status.port)) {
            (Some(h), _) => h.port,
            (None, Some(p)) if p != 0 => p,
            _ => 8443,
        }
    }

    /// The `dh1.` string that pairs a helper or publisher with this hub, from an invite's secret.
    fn invite_string(&self, secret: &str) -> Option<String> {
        use base64::Engine;
        let h = self.hub.as_ref()?;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(secret)
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
        if port.is_empty() || self.busy.is_some() {
            return None;
        }
        // Keys from an install go only to the port it was installed through: on any other port
        // they would make an unrelated display trust that maker, so they are dropped once the send
        // starts.
        if self.pending_firmware.as_ref().is_some_and(|p| p.port != port) {
            let op = self.send_usb_without_keys(port, bundle, Some(false))?;
            self.pending_firmware = None;
            return Some(op);
        }
        let keys = self.pending_firmware.as_ref().map(|p| p.include);
        let bundle = self.bundle_with_pending(bundle);
        self.send_usb_without_keys(port, bundle, keys)
    }

    fn send_usb_without_keys(&mut self, port: String, bundle: Bundle, keys: Option<bool>) -> Option<Op> {
        self.notice = None;
        self.start(
            Busy::Sending {
                port: port.clone(),
                keys,
            },
            Op::SendUsb { port, bundle },
        )
    }

    /// Saves the setup details to a file. Keys waiting from an install never go in it: they go
    /// only over USB, to the port the firmware was installed through, and a file could reach any
    /// display. Once it is saved they are dropped, as the details can't then be sent from here.
    pub fn save_bundle(&mut self, path: PathBuf) -> Option<Op> {
        let bundle = self.enrol.bundle.clone()?;
        self.start(Busy::SavingFile, Op::SaveBundle { path, bundle })
    }

    /// Cancels Add a display, or closes it once done. Keys waiting from an install are dropped
    /// with it, so they can't reach some later, unrelated display.
    pub fn close_enrol(&mut self) {
        self.enrol = Enrol::default();
        self.pending_firmware = None;
    }

    /// The setup details' expiry, by the hub's clock.
    pub fn setup_expires_in(&self, now: Instant) -> Option<i64> {
        let b = self.enrol.bundle.as_ref()?;
        Some(self.hub_now(now)?.seconds_until(b.enrolment.expires_at))
    }

    // ---- helpers -----------------------------------------------------------------------------------

    pub fn helpers(&self) -> &[HelperView] {
        self.last
            .as_ref()
            .map(|r| r.helpers.as_slice())
            .unwrap_or_default()
    }

    /// The feeds of the hub's helper pages, other than the usage feeds, to tick when inviting a
    /// helper. In page order, each once. A feed that is ticked stays offered (last) even when no
    /// listed page takes it, so it can still be sent or unticked.
    pub fn helper_feed_choices(&self) -> Vec<FeedChoice> {
        let mut out: Vec<FeedChoice> = Vec::new();
        for p in self.pages().iter().filter(|p| p.source == Source::Helper) {
            let Some(feed) = p.feed.as_deref() else { continue };
            if is_usage_feed(feed) {
                continue;
            }
            match out.iter_mut().find(|c| c.feed == feed) {
                Some(c) => c.pages.push(p.title.clone()),
                None => out.push(FeedChoice {
                    feed: feed.to_string(),
                    pages: vec![p.title.clone()],
                    ticked: self.helper.feeds.iter().any(|f| f == feed),
                }),
            }
        }
        for feed in &self.helper.feeds {
            if !out.iter().any(|c| &c.feed == feed) && !is_usage_feed(feed) {
                out.push(FeedChoice {
                    feed: feed.clone(),
                    pages: Vec::new(),
                    ticked: true,
                });
            }
        }
        out
    }

    /// Ticks or unticks one of the hub's helper-page feeds for the next invite.
    pub fn toggle_helper_feed(&mut self, feed: &str) {
        match self.helper.feeds.iter().position(|f| f == feed) {
            Some(at) => {
                self.helper.feeds.remove(at);
            }
            None => self.helper.feeds.push(feed.to_string()),
        }
    }

    /// The feeds the helper form asks for, checked by the hub's rules, or what is wrong.
    fn helper_feeds(&self) -> Result<Vec<String>, Guidance> {
        // Each service the usage helper reports is the feed of the same name.
        let mut feeds: Vec<String> = [
            (self.helper.claude, Service::ClaudeMax),
            (self.helper.chatgpt, Service::ChatgptPlus),
        ]
        .into_iter()
        .filter(|&(on, _)| on)
        .map(|(_, s)| s.slug().to_string())
        .collect();
        for c in self.helper_feed_choices().into_iter().filter(|c| c.ticked) {
            feeds.push(c.feed);
        }
        // A paste is cut to fit, back to a character boundary, so treat a box within one character
        // (at most 4 bytes) of full as cut.
        if self.helper.other.text.len() + 4 > self.helper.other.max_len {
            return Err(Guidance {
                tone: Tone::Problem,
                title: "That list of feeds is too long".into(),
                detail: "Type fewer feeds, or tick the ones your pages already have.".into(),
                next: Next::None,
            });
        }
        for typed in self
            .helper
            .other
            .text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
        {
            if !valid_feed(typed) {
                return Err(Guidance {
                    tone: Tone::Problem,
                    title: format!("{typed} can't be a feed's name"),
                    detail: feed_rule(),
                    next: Next::None,
                });
            }
            feeds.push(typed.to_string());
        }
        let mut seen = HashSet::new();
        feeds.retain(|f| seen.insert(f.clone()));
        if feeds.is_empty() {
            return Err(Guidance {
                tone: Tone::Problem,
                title: "Choose what the helper sends to".into(),
                detail: "Claude Max (from Claude Code), ChatGPT Plus (from Codex), or a feed of your own: tick one \
                         of your helper pages' feeds, or type its name."
                    .into(),
                next: Next::None,
            });
        }
        if feeds.len() > HUB_PAGE_LIMIT {
            return Err(Guidance {
                tone: Tone::Problem,
                title: format!("A helper can send to at most {HUB_PAGE_LIMIT} feeds"),
                detail: "Choose fewer.".into(),
                next: Next::None,
            });
        }
        Ok(feeds)
    }

    pub fn create_helper(&mut self) -> Option<Op> {
        let feeds = match self.helper_feeds() {
            Ok(f) => f,
            Err(g) => {
                self.helper.problem = Some(g.title.clone());
                return self.problem(g);
            }
        };
        let label = clean_title(self.pc_name.trimmed()).unwrap_or_else(|| "Helper".into());
        if self.busy.is_some() {
            return None;
        }
        self.helper.string = None;
        self.helper.invited = feeds.clone();
        self.start(
            Busy::Saving { done: String::new() },
            Op::NewHelper(NewHelper { label, feeds }),
        )
    }

    /// Whether the last helper string is for `usage-helper`, for `hub-send`, or for both: the usage
    /// feeds are usage-helper's, any other feed hub-send's.
    pub fn invite_programs(&self) -> (bool, bool) {
        let usage = |f: &String| is_usage_feed(f);
        (
            self.helper.invited.iter().any(usage),
            self.helper.invited.iter().any(|f| !usage(f)),
        )
    }

    /// The feeds the last helper string covers, other than the usage ones.
    pub fn invited_own_feeds(&self) -> Vec<&str> {
        self.helper
            .invited
            .iter()
            .filter(|f| !is_usage_feed(f))
            .map(String::as_str)
            .collect()
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
            Confirm::RemovePackage { id, title, .. } => {
                let done = format!("Removed {title} from the hub.");
                self.start(Busy::Saving { done }, Op::RemoveFirmware { id })
            }
            Confirm::Update {
                display,
                package,
                second,
                ..
            } => self.confirm_update(display, package, second),
            Confirm::UpdateAgain { display, package, .. } => self.offer(display, package),
            Confirm::RevokePublisher { id, label } => {
                let done = format!("Removed the publisher {label}.");
                self.start(Busy::Saving { done }, Op::RevokePublisher { id })
            }
            Confirm::Install { port, .. } => self.start_install(port),
        }
    }

    pub fn confirm_no(&mut self) {
        self.confirm = None;
    }

    // ---- answers ------------------------------------------------------------------------------------

    /// Takes a refresh that may be missing. If it is, one is due at once. True if it couldn't read
    /// the firmware lists, as for `take_refresh`.
    #[must_use = "whether the firmware lists were unreadable, for tell_unreadable_lists"]
    fn take_refresh_opt(&mut self, r: Option<Refresh>, now: Instant) -> bool {
        match r {
            Some(r) => self.take_refresh(r, now),
            None => {
                self.refreshed_at = None;
                false
            }
        }
    }

    /// Takes a refresh. True if it couldn't read the firmware lists, for `tell_unreadable_lists`.
    #[must_use = "whether the firmware lists were unreadable, for tell_unreadable_lists"]
    fn take_refresh(&mut self, mut r: Refresh, now: Instant) -> bool {
        let unreadable = r.firmware_error.is_some();
        if unreadable {
            // The hub has firmware lists, but they couldn't be read this time: keep the ones it had
            // rather than showing it as a hub without signed updates. The error stays, for
            // `tell_unreadable_lists`.
            r.firmware = Some(
                self.last
                    .as_ref()
                    .and_then(|l| l.firmware.clone())
                    .unwrap_or_default(),
            );
        } else {
            self.lists_told = false;
        }
        if let EnrolStage::Waiting { .. } = self.enrol.stage {
            if let Some(d) = r.displays.iter().find(|d| !self.enrol.known.contains(&d.id)) {
                self.enrol.stage = EnrolStage::Joined { name: d.name.clone() };
                self.enrol.bundle = None;
                self.pending_firmware = None;
            }
        }
        self.last = Some(r);
        self.refreshed_at = Some(now);
        self.online = Some(true);
        self.prune_tick_changes();
        self.prune_helper_feeds();
        unreadable
    }

    /// Once a helper string is made, the form's own problem from an earlier try, such as a bad feed
    /// name, no longer applies. Any other notice stays.
    fn clear_helper_problem(&mut self) {
        let Some(title) = self.helper.problem.take() else {
            return;
        };
        if self.notice.as_ref().is_some_and(|n| n.title == title) {
            self.notice = None;
        }
    }

    /// Drops ticked helper feeds that no page on the hub takes any more, so a page removed since
    /// isn't still granted to the next helper.
    fn prune_helper_feeds(&mut self) {
        let Some(last) = &self.last else { return };
        let taken: HashSet<&str> = last
            .pages
            .iter()
            .filter(|p| p.source == Source::Helper)
            .filter_map(|p| p.feed.as_deref())
            .collect();
        self.helper.feeds.retain(|f| taken.contains(f.as_str()));
    }

    /// Forgets unsaved tick changes that no longer mean anything: the display or page has gone, or
    /// the hub already has what was asked for.
    fn prune_tick_changes(&mut self) {
        let Some(r) = self.last.as_ref() else { return };
        self.tick_changes.retain(|display, changes| {
            let Some(saved) = r.ticks.get(display) else {
                return false;
            };
            changes.retain(|page, want| {
                r.pages.iter().any(|p| &p.id == page) && saved.iter().any(|t| t == page) != *want
            });
            !changes.is_empty()
        });
    }

    pub fn finish(&mut self, result: Result<Outcome, Failure>, now: Instant) {
        if self.take_outcome(result, now) {
            self.tell_unreadable_lists();
        }
    }

    /// Says the firmware lists couldn't be read by the refresh just taken: once each time they stop
    /// being readable, not at every refresh while they stay so, and never over another notice,
    /// including one the outcome just set. If one is showing, the next refresh that still can't
    /// read them says it instead, once that notice has been dismissed; an answer without a refresh
    /// never does, so an old failure isn't brought up over something unrelated.
    fn tell_unreadable_lists(&mut self) {
        if self.lists_told || self.notice.is_some() {
            return;
        }
        let Some(e) = self.last.as_ref().and_then(|r| r.firmware_error.clone()) else {
            return;
        };
        let why = guidance::from_client(&e, Doing::Other, &self.address());
        self.notice = Some(Guidance {
            tone: Tone::Problem,
            title: "Couldn't read the firmware lists".into(),
            detail: format!(
                "{}. {} The Firmware screen shows what the hub held when it last answered.",
                why.title, why.detail
            ),
            next: Next::None,
        });
        self.lists_told = true;
    }

    /// Takes an answer. True if it brought a refresh that couldn't read the firmware lists.
    #[must_use = "whether the firmware lists were unreadable, for tell_unreadable_lists"]
    fn take_outcome(&mut self, result: Result<Outcome, Failure>, now: Instant) -> bool {
        let busy = self.busy.take();
        let mut unreadable = false;
        match result {
            Ok(Outcome::Paired { hub, refresh }) => {
                self.pairing.clear();
                // A hub newly paired: its firmware lists haven't been told about yet.
                self.lists_told = false;
                self.hub = Some(hub);
                self.needs_pairing = false;
                self.section = Section::Overview;
                unreadable = self.take_refresh_opt(refresh, now);
                self.notice = Some(Guidance::done(
                    "Paired",
                    "This PC can now manage your hub. The checklist on the Overview shows what's left to do.",
                ));
            }
            Ok(Outcome::Refreshed(r)) => {
                unreadable = self.take_refresh_opt(r, now);
                let done = match busy {
                    Some(Busy::Saving { done } | Busy::Offering { done } | Busy::Naming { done }) => {
                        Some(done)
                    }
                    _ => None,
                };
                if let Some(done) = done.filter(|d| !d.is_empty()) {
                    self.notice = Some(Guidance::done(done, ""));
                } else if self.notice.as_ref().is_some_and(|n| n.next == Next::Retry) {
                    // The hub is back.
                    self.notice = None;
                }
            }
            Ok(Outcome::Added { refresh, failed }) => {
                let old: Vec<String> = self.pages().iter().map(|p| p.id.clone()).collect();
                let held = match refresh {
                    Some(r) => {
                        let held = self.held_back(&old, &r);
                        unreadable = self.take_refresh(r, now);
                        held
                    }
                    None => {
                        // The form closes either way, so pressing Add again can't double what was
                        // added; a refresh is due at once.
                        self.refreshed_at = None;
                        String::new()
                    }
                };
                self.add.form = None;
                self.add.helper_page = None;
                match (failed, busy) {
                    (None, Some(Busy::Adding { done, setup, feed })) => {
                        self.add.setup = setup;
                        self.add.feed = feed;
                        self.notice = Some(Guidance::done(done, held));
                    }
                    (None, _) => {}
                    (Some(f), _) => {
                        // What was added stays, so adding again would double it: start over from the
                        // list, and say what happened.
                        let address = self.address();
                        let why = guidance::from_client(&f.error, Doing::Other, &address);
                        self.notice = Some(Guidance {
                            tone: Tone::Problem,
                            title: format!("Added {} of {} pages", f.added, f.total),
                            detail: format!(
                                "{} wasn't added: {}. {} The pages added before it are on the list. {held}",
                                f.title,
                                why.title.to_lowercase(),
                                why.detail
                            )
                            .trim_end()
                            .to_string(),
                            next: why.next,
                        });
                    }
                }
            }
            Ok(Outcome::Enrolment { grant, refresh }) => {
                unreadable = self.take_refresh_opt(refresh, now);
                let wifi = wifi_of(&self.enrol.ssid, &self.enrol.password).ok().flatten();
                self.enrol.password.clear();
                let port = self.bundle_port();
                match hub_client::provisioning::bundle(&grant, self.bundle_hosts(), port, wifi) {
                    Ok(b) => {
                        // Keys waiting from an install are added when the details are sent, to the
                        // port it was installed through.
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
                unreadable = self.take_refresh_opt(refresh, now);
                match self.invite_string(&invite.secret) {
                    Some(s) => {
                        self.helper.string = Some(Zeroizing::new(s));
                        self.helper.expires_at = Some(invite.expires_at);
                        self.clear_helper_problem();
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
            Ok(Outcome::GuideSaved) => {}
            Ok(Outcome::Ports(p)) => {
                self.enrol.ports = p;
                self.enrol.scanned = true;
                self.fw.install.scanned = true;
            }
            Ok(Outcome::Sent(display)) => {
                // Any keys waiting from an install went with these setup details, or were left out:
                // either way they are done with.
                self.pending_firmware = None;
                let (port, keys) = match busy {
                    Some(Busy::Sending { port, keys }) => (port, keys),
                    _ => (String::new(), None),
                };
                self.enrol.stage = EnrolStage::Waiting {
                    how: format!("Sent to {port}"),
                };
                let keys = match keys {
                    Some(true) => " It now trusts the keys from the firmware just installed.",
                    Some(false) => " The keys from the firmware just installed weren't sent with them.",
                    None => "",
                };
                self.notice = Some(Guidance::done(
                    format!("Sent to the display on {port}"),
                    format!(
                        "{} {} accepted the setup details.{keys} It joins Wi-Fi and checks in with the hub; this \
                         page updates when it does.",
                        display.device.device_type, display.device.firmware_version
                    ),
                ));
            }
            Ok(Outcome::Saved(path)) => {
                // Keys waiting from an install were left out of the file, and nothing more can be
                // sent from here once it is saved: they are done with.
                let keys = match self.pending_firmware.take() {
                    Some(p) if p.include => format!(
                        " The keys from the firmware just installed on {} aren't in it, so the display keeps the keys \
                         it already trusts, if any. With none, it accepts no updates until it gets keys over USB.",
                        p.port
                    ),
                    _ => String::new(),
                };
                self.enrol.stage = EnrolStage::Waiting {
                    how: format!("Saved to {}", path.display()),
                };
                self.notice = Some(Guidance::done(
                    "Setup file saved",
                    format!(
                        "It holds a one-time secret and any Wi-Fi password. Delete it once the display has \
                         checked in.{keys}"
                    ),
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
            Ok(Outcome::Uploaded {
                package,
                new,
                refresh,
            }) => {
                unreadable = self.take_refresh_opt(refresh, now);
                self.uploaded(&package, new);
            }
            Ok(Outcome::PublisherInvite { invite, refresh }) => {
                unreadable = self.take_refresh_opt(refresh, now);
                self.take_publisher_invite(invite);
            }
            Ok(Outcome::PackageLoaded(loaded)) => self.take_loaded(*loaded),
            Ok(Outcome::Installed {
                after,
                refresh,
                label,
            }) => unreadable = self.take_installed(after, refresh, label, now),
            Err(f) => self.failed(f, busy, now),
        }
        unreadable
    }

    fn failed(&mut self, f: Failure, busy: Option<Busy>, now: Instant) {
        let doing = match busy {
            Some(Busy::Pairing) => Doing::Pairing,
            Some(Busy::Uploading) => Doing::Uploading,
            Some(Busy::Offering { .. }) => Doing::Offering,
            Some(Busy::Naming { .. }) => Doing::Naming,
            _ => Doing::Other,
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
            Failure::ReadFile(d) => Guidance {
                tone: Tone::Problem,
                title: "Couldn't read the file".into(),
                detail: d.clone(),
                next: Next::None,
            },
            Failure::Package(d) => guidance::from_package(d),
            Failure::Install(e) => {
                let port = match &busy {
                    Some(Busy::Installing { port }) => port.clone(),
                    _ => String::new(),
                };
                guidance::from_install(e, &port)
            }
        };
        if matches!(busy, Some(Busy::Installing { .. })) {
            self.install_failed();
        }
        if g.next == Next::PairAgain && self.hub.is_some() {
            self.needs_pairing = true;
        }
        // A reorder shown early is put back by the next refresh.
        if matches!(
            busy,
            Some(Busy::Saving { .. } | Busy::Offering { .. } | Busy::Naming { .. } | Busy::Adding { .. })
        ) {
            self.refreshed_at = None;
        } else if busy == Some(Busy::Refreshing) {
            // A refresh that failed for any reason waits its turn before the next, never every tick.
            self.refreshed_at = Some(now);
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
                s.pages,
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

pub fn format_percent(p: f64) -> String {
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

/// A title no page on the hub has yet: "Big number" becomes "Big number 2", then "Big number 3".
fn unique_title(title: &str, taken: &[String]) -> String {
    let used = |t: &str| taken.iter().any(|x| x.to_lowercase() == t.to_lowercase());
    if !used(title) {
        return title.to_string();
    }
    (2..)
        .map(|n| {
            let tail = format!(" {n}");
            let room = MAX_TITLE_LEN.saturating_sub(tail.chars().count());
            let base: String = title.chars().take(room).collect();
            format!("{}{tail}", base.trim_end())
        })
        .find(|t| !used(t))
        .expect("an endless count finds one")
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
    use hub_core::StoredContent;
    use serde_json::json;

    pub(super) fn ts(s: &str) -> Timestamp {
        Timestamp::parse(s).unwrap()
    }

    fn page(id: &str, source: Source) -> PageView {
        PageView {
            id: id.into(),
            title: id.into(),
            kind: "text".into(),
            source,
            feed: None,
            stale_after_seconds: source.stale_after_seconds(),
            content: None,
            template: None,
            settings: None,
        }
    }

    fn with_content(
        mut p: PageView,
        quality: DataQuality,
        updated: &str,
        data: serde_json::Value,
    ) -> PageView {
        p.content = Some(StoredContent {
            quality,
            updated_at: Some(ts(updated)),
            data: Some(data),
            note: None,
        });
        p
    }

    pub(super) fn refresh(pages: Vec<PageView>) -> Refresh {
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
                pages: pages.len(),
                displays: 0,
                admins: 1,
            },
            pages,
            displays: vec![],
            preview: Snapshot {
                protocol_version: 1,
                message_type: "dashboard.snapshot".into(),
                generated_at: ts("2026-11-10T20:00:00Z"),
                refresh_after_seconds: 300,
                pages: vec![],
                firmware_update: None,
            },
            helpers: vec![],
            ticks: Default::default(),
            firmware: None,
            firmware_error: None,
        }
    }

    pub(super) fn connected(pages: Vec<PageView>) -> (Model, Instant) {
        let hub = SavedHub::new(
            "192.168.1.20",
            8443,
            &[0xab; 32],
            "Desk PC",
            ts("2026-11-10T19:00:00Z"),
        );
        let mut m = Model::new(Some(hub), "Desk PC");
        let now = Instant::now();
        m.finish(Ok(Outcome::Refreshed(Some(refresh(pages)))), now);
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
            page("a", Source::Demo),
            page("b", Source::Demo),
            page("c", Source::Demo),
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

    fn pick(m: &mut Model, id: &str) {
        let i = m.add.templates.iter().position(|t| t.id == id).unwrap();
        m.pick_template(i);
        assert!(m.add.form.is_some());
    }

    fn set_text(m: &mut Model, key: &str, value: &str) {
        let form = m.add.form.as_mut().unwrap();
        let i = form.template.settings.iter().position(|s| s.key == key).unwrap();
        form.text_mut(i).unwrap().set(value);
    }

    #[test]
    fn the_hub_page_limit_is_the_hubs_own() {
        assert_eq!(HUB_PAGE_LIMIT, hubd::store::MAX_STORED_PAGES);
    }

    #[test]
    fn adding_from_a_template_sends_the_pages_and_reports_success() {
        let (mut m, now) = connected(vec![]);
        pick(&mut m, "big-number/1");
        set_text(&mut m, "title", "Water used");
        let Some(Op::AddPages { pages }) = m.submit_add() else {
            panic!("add")
        };
        assert_eq!(
            (pages[0].title.as_str(), pages[0].kind.as_str(), pages[0].source),
            ("Water used", "big_number", Source::Manual)
        );
        assert_eq!(pages[0].template.as_deref(), Some("big-number/1"));
        assert!(m.submit_add().is_none(), "one operation at a time");
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(refresh(vec![page("x", Source::Manual)])),
                failed: None,
            }),
            now,
        );
        let n = m.notice.clone().unwrap();
        assert_eq!(n.tone, Tone::Done);
        assert!(
            n.title.contains("Added Water used") && n.title.contains("5 minutes"),
            "{}",
            n.title
        );
        assert!(m.add.form.is_none(), "back to the list of templates");
        assert!(m.add.setup.is_none());
    }

    #[test]
    fn a_mistake_in_the_form_is_shown_with_the_form_and_sends_nothing() {
        let (mut m, _) = connected(vec![]);
        pick(&mut m, "short-text/1");
        set_text(&mut m, "title", &"w".repeat(80));
        assert!(m.submit_add().is_none());
        let msg = m.add.form.as_ref().unwrap().error.clone().unwrap();
        assert!(msg.contains("at most 48 characters"), "{msg}");
        assert!(
            m.notice.is_none(),
            "it belongs to the form, not a message along the bottom"
        );
        assert!(m.busy.is_none());
        m.typing();
        assert!(m.add.form.as_ref().unwrap().error.is_none());
    }

    #[test]
    fn the_hub_limit_is_50_pages_not_10() {
        // Eleven pages is fine: ten is how many one display shows, not how many the hub holds.
        let (mut m, _) = connected((0..11).map(|i| page(&format!("p{i}"), Source::Demo)).collect());
        assert!(m.can_add_page());
        pick(&mut m, "list/1");
        assert!(matches!(m.submit_add(), Some(Op::AddPages { .. })));

        let (mut full, _) = connected(
            (0..HUB_PAGE_LIMIT)
                .map(|i| page(&format!("p{i}"), Source::Demo))
                .collect(),
        );
        assert!(!full.can_add_page());
        let i = full.add.templates.iter().position(|t| t.id == "list/1").unwrap();
        full.pick_template(i);
        assert!(full.add.form.is_none());
        assert!(full.notice.as_ref().unwrap().title.contains("at most 50"));
    }

    #[test]
    fn pi_status_shows_its_command_afterwards_to_copy() {
        let (mut m, now) = connected(vec![]);
        pick(&mut m, "pi-status/1");
        let Some(Op::AddPages { pages }) = m.submit_add() else {
            panic!("add")
        };
        assert_eq!(
            (pages[0].source, pages[0].feed.as_deref()),
            (Source::Helper, Some("pi-status"))
        );
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(refresh(vec![page("pi-status", Source::Helper)])),
                failed: None,
            }),
            now,
        );
        let setup = m.add.setup.clone().expect("a setup step");
        assert!(setup.command.starts_with("sudo display-hub-helper add pi-status"));
        assert!(setup.help.contains("Raspberry Pi") && !setup.copied);
        m.copy_setup();
        assert!(m.add.setup.as_ref().unwrap().copied);
        m.finish_setup();
        assert!(m.add.setup.is_none() && m.add.form.is_none());
    }

    #[test]
    fn a_failure_part_way_is_reported_honestly() {
        let (mut m, now) = connected(vec![]);
        pick(&mut m, "usage-limits/1");
        assert!(m.submit_add().is_some());
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(refresh(vec![page("a", Source::Manual)])),
                failed: Some(AddFailure {
                    added: 1,
                    total: 3,
                    title: "Second".into(),
                    error: ClientError::Timeout,
                }),
            }),
            now,
        );
        let n = m.notice.clone().unwrap();
        assert_eq!(n.tone, Tone::Problem);
        assert_eq!(n.title, "Added 1 of 3 pages");
        assert!(n.detail.contains("Second wasn't added"), "{}", n.detail);
        assert!(m.add.form.is_none(), "start over, so nothing is added twice");
        // A failure on the first page keeps the form so it can be tried again.
        pick(&mut m, "list/1");
        assert!(m.submit_add().is_some());
        m.finish(Err(Failure::Client(ClientError::Timeout)), now);
        assert!(m.add.form.is_some());
    }

    #[test]
    fn pages_added_but_the_list_not_read_back_close_the_form_and_refresh_soon() {
        let (mut m, now) = connected(vec![]);
        pick(&mut m, "short-text/1");
        assert!(m.submit_add().is_some());
        m.finish(
            Ok(Outcome::Added {
                refresh: None,
                failed: None,
            }),
            now,
        );
        assert!(m.add.form.is_none(), "closed, so Add can't be pressed again");
        assert_eq!(m.notice.as_ref().map(|n| n.tone), Some(Tone::Done));
        assert!(m.refresh_due(now).is_some(), "the list catches up at once");
    }

    fn typed_in(id: &str, kind: &str, data: serde_json::Value) -> PageView {
        let mut p = with_content(
            page(id, Source::Manual),
            DataQuality::Manual,
            "2026-11-10T19:00:00Z",
            data,
        );
        p.kind = kind.into();
        p
    }

    #[test]
    fn editing_a_usage_meter_keeps_its_checks_and_reset_times() {
        let mut p = typed_in(
            "claude-max",
            "meter",
            json!({"items":[{"id":"five-hour","label":"5 hours","percent":42.0,"resets_at":"2026-11-10T22:00:00Z"}]}),
        );
        p.feed = Some("claude-max".into());
        let (mut m, now) = connected(vec![p]);
        m.begin_edit("claude-max", now);
        let e = m.edit.as_mut().unwrap();
        let Some(crate::forms::ContentEdit {
            shape: crate::forms::Shape::UsageMeter { rows, .. },
            ..
        }) = e.content.as_mut()
        else {
            panic!("usage editor")
        };
        assert_eq!(rows[0].percent.text, "42");
        assert_eq!(rows[0].resets_in.text, "2 h");
        rows[1].percent.set("abc");
        assert!(m.save_edit(now).is_none());
        assert!(
            m.edit
                .as_ref()
                .unwrap()
                .content_error
                .as_deref()
                .unwrap()
                .contains("0 to 100"),
            "shown under the editor, not along the bottom"
        );
        assert!(m.notice.is_none());
        m.typing();
        assert!(
            m.edit.as_ref().unwrap().content_error.is_none(),
            "clears on the next change"
        );
        m.save_edit(now);
        let Some(crate::forms::ContentEdit {
            shape: crate::forms::Shape::UsageMeter { rows, .. },
            ..
        }) = m.edit.as_mut().unwrap().content.as_mut()
        else {
            panic!()
        };
        rows[1].percent.set("");
        rows[1].resets_in.set("2 d 4 h");
        let Some(Op::SaveEdit {
            content: Some(c),
            change,
            ..
        }) = m.save_edit(now)
        else {
            panic!("save")
        };
        assert_eq!(change.source, None, "still typed in");
        let items = c.data.unwrap()["items"].clone();
        assert_eq!(items[0]["percent"], 42.0);
        assert!(items[1]["percent"].is_null(), "empty is unknown, not zero");
        assert_eq!(items[1]["resets_at"], "2026-11-13T00:00:00Z");
        assert_eq!(c.note, None);
    }

    #[test]
    fn a_title_only_edit_keeps_content_that_the_boxes_show_rounded_or_not_at_all() {
        let mut meter = typed_in(
            "tank",
            "meter",
            json!({"items":[
                {"id":"a","label":"Soon","percent":10.0,"resets_at":"2026-11-10T20:00:30Z"},
                {"id":"b","label":"Later","percent":20.0,"resets_at":"2026-11-10T22:00:45Z"},
                {"id":"c","label":"Gone","percent":30.0,"resets_at":"2026-11-10T10:00:00Z"}]}),
        );
        meter.title = "Tank".into();
        let big = typed_in(
            "big",
            "big_number",
            json!({"value": 1234.56789, "decimals": 2, "unit": "m"}),
        );
        let whole = typed_in("whole", "big_number", json!({"value": 5.0, "decimals": 0}));
        let (mut m, now) = connected(vec![meter, big, whole]);
        for id in ["tank", "big", "whole"] {
            m.begin_edit(id, now);
            m.edit.as_mut().unwrap().title.set("Renamed");
            let Some(Op::SaveEdit { content, .. }) = m.save_edit(now) else {
                panic!("{id}: save")
            };
            assert!(
                content.is_none(),
                "{id}: nothing typed, so the content isn't touched"
            );
            m.finish(Ok(Outcome::Refreshed(Some(refresh(m.pages().to_vec())))), now);
        }
        // A sub-minute reset reads as something the box takes back.
        m.begin_edit("tank", now);
        let Some(crate::forms::ContentEdit {
            shape: crate::forms::Shape::Meter { rows },
            ..
        }) = m.edit.as_mut().unwrap().content.as_mut()
        else {
            panic!()
        };
        assert_eq!(rows[0].resets_in.text, "1 min");
        assert!(rows[2].resets_in.text.is_empty(), "a past time can't be shown");
        rows[0].percent.set("11");
        let Some(Op::SaveEdit { content: Some(c), .. }) = m.save_edit(now) else {
            panic!("save")
        };
        let items = c.data.unwrap()["items"].clone();
        assert_eq!(items[0]["percent"], 11.0);
        assert_eq!(
            items[0]["resets_at"], "2026-11-10T20:00:30Z",
            "the stored time is kept"
        );
        assert_eq!(items[1]["resets_at"], "2026-11-10T22:00:45Z");
        assert!(
            items[2]["resets_at"].is_null(),
            "a reset already past means nothing, so it is dropped once the bar is edited"
        );
    }

    #[test]
    fn an_empty_typed_in_page_says_nothing_is_typed_in_on_the_overview() {
        let (m, _) = connected(vec![page("a", Source::Manual), page("b", Source::None)]);
        let hub_page = |id: &str, note: &str| hub_protocol::Page {
            id: id.into(),
            title: "T".into(),
            kind: "text".into(),
            data_quality: DataQuality::Unavailable,
            updated_at: None,
            stale_after_seconds: 3600,
            note: Some(note.into()),
            data: None,
        };
        let said = hub_core::build::NO_SOURCE_NOTE;
        assert_eq!(
            m.preview_note(&hub_page("a", said)).as_deref(),
            Some("Nothing typed in yet")
        );
        assert_eq!(
            m.preview_note(&hub_page("b", said)).as_deref(),
            Some("No source chosen yet")
        );
        assert_eq!(m.preview_note(&hub_page("a", "Other")).as_deref(), Some("Other"));
    }

    #[test]
    fn a_secret_from_the_hub_is_kept_when_the_refresh_after_it_failed() {
        let (mut m, now) = connected(vec![]);
        m.enrol.name.set("Desk");
        let Some(Op::NewEnrolment { .. }) = m.create_setup() else {
            panic!("enrol")
        };
        let grant = EnrolmentGrant {
            secret: "s".repeat(22),
            expires_at: ts("2026-11-10T20:10:00Z"),
            identity: HubIdentity {
                certificate_pem: "-----BEGIN CERTIFICATE-----\nx\n-----END CERTIFICATE-----\n".into(),
                spki_sha256: "ab".repeat(32),
                server_name: hub_protocol::provisioning::HUB_SERVER_NAME.into(),
            },
        };
        m.finish(Ok(Outcome::Enrolment { grant, refresh: None }), now);
        assert!(m.enrol.bundle.is_some(), "the one-time setup details are shown");
        assert!(m.refresh_due(now).is_some(), "and a refresh is due");
    }

    #[test]
    fn a_refresh_that_fails_for_any_reason_waits_before_the_next() {
        let (mut m, now) = connected(vec![]);
        m.refreshed_at = None;
        let Some(Op::Refresh) = m.refresh_due(now) else {
            panic!("due")
        };
        m.finish(Err(Failure::File("odd".into())), now);
        assert!(m.refresh_due(now + Duration::from_secs(1)).is_none(), "backs off");
        assert!(m.refresh_due(now + REFRESH_EVERY).is_some());
    }

    #[test]
    fn editing_a_numbered_page_matches_its_template_page() {
        let made = |title: &str| NewPage {
            title: title.into(),
            kind: "text".into(),
            source: Source::Manual,
            feed: None,
            id: None,
            stale_after_seconds: None,
            template: None,
            settings: None,
        };
        let pick = |on: &str| {
            pick_page(vec![made("Temp"), made("Humidity")], "text", &None, on)
                .unwrap()
                .title
        };
        assert_eq!(pick("Humidity"), "Humidity");
        assert_eq!(
            pick("Humidity 2"),
            "Humidity",
            "numbered when the title was taken"
        );
        assert_eq!(pick("Temp 12"), "Temp");
        assert_eq!(pick("Renamed"), "Temp", "else the first, as before");
    }

    #[test]
    fn a_new_page_with_a_title_already_on_the_hub_gets_a_number() {
        let mut existing = page("a", Source::Manual);
        existing.title = "Note".into();
        let (mut m, _) = connected(vec![existing]);
        pick(&mut m, "short-text/1");
        let Some(Op::AddPages { pages }) = m.submit_add() else {
            panic!("adds")
        };
        assert_eq!(pages[0].title, "Note 2");
        assert_eq!(unique_title("Tank", &["tank".into()]), "Tank 2");
        assert_eq!(unique_title("Tank", &["Tank".into(), "Tank 2".into()]), "Tank 3");
        assert_eq!(unique_title("Tank", &[]), "Tank");
        let long = "x".repeat(MAX_TITLE_LEN);
        assert_eq!(
            unique_title(&long, std::slice::from_ref(&long)).chars().count(),
            MAX_TITLE_LEN
        );
    }

    #[test]
    fn a_title_only_edit_of_typed_in_content_doesnt_resend_it() {
        let (mut m, now) = connected(vec![typed_in("t", "text", json!({"text": "Back at 3"}))]);
        m.begin_edit("t", now);
        m.edit.as_mut().unwrap().title.set("Note");
        let Some(Op::SaveEdit { change, content, .. }) = m.save_edit(now) else {
            panic!("save")
        };
        assert_eq!(change.title.as_deref(), Some("Note"));
        assert!(
            content.is_none(),
            "the words didn't change, so they keep their time"
        );
        assert!(m.edit.is_none());

        let again = refresh(vec![typed_in("t", "text", json!({"text": "Back at 3"}))]);
        m.finish(Ok(Outcome::Refreshed(Some(again))), now);
        m.begin_edit("t", now);
        let Some(crate::forms::ContentEdit {
            shape: crate::forms::Shape::Text { text },
            ..
        }) = m.edit.as_mut().unwrap().content.as_mut()
        else {
            panic!()
        };
        text.set("Back at 4");
        let Some(Op::SaveEdit { content: Some(c), .. }) = m.save_edit(now) else {
            panic!("save")
        };
        assert_eq!(c.data.unwrap(), json!({"text": "Back at 4"}));
    }

    #[test]
    fn helper_demo_and_custom_pages_have_no_content_editor() {
        let mut helper = page("h", Source::Helper);
        helper.feed = Some("pi-status".into());
        helper.kind = "big_number".into();
        let mut custom = page("c", Source::Helper);
        custom.kind = "nearby-flights/1".into();
        let (mut m, now) = connected(vec![helper, custom]);
        m.begin_edit("h", now);
        assert_eq!(m.edit.as_ref().unwrap().effective_source(), Source::Helper);
        // The editor exists for the shape, but only shows while the content is typed in.
        m.begin_edit("c", now);
        assert!(m.edit.as_ref().unwrap().content.is_none());
        // A helper page whose title changes sends the title and nothing about its content.
        m.begin_edit("h", now);
        m.edit.as_mut().unwrap().title.set("CPU");
        let Some(Op::SaveEdit { change, content, .. }) = m.save_edit(now) else {
            panic!()
        };
        assert_eq!(change.title.as_deref(), Some("CPU"));
        assert_eq!((change.source, change.settings, content), (None, None, None));
    }

    #[test]
    fn a_template_page_edits_through_its_form_and_follows_its_settings() {
        let t = hub_templates::built_in_template("usage-limits/1").unwrap();
        let settings = t
            .settings_from(
                &json!({"service": "claude-max", "source": "manual"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let mut p = page("claude-max", Source::Manual);
        p.kind = "meter".into();
        p.feed = Some("claude-max".into());
        p.template = Some("usage-limits/1".into());
        p.settings = Some(settings);
        let (mut m, now) = connected(vec![p]);
        m.begin_edit("claude-max", now);
        {
            let e = m.edit.as_mut().unwrap();
            assert!(e.form.is_some() && !e.has_title_box(), "the title is on the form");
            let form = e.form.as_mut().unwrap();
            let title = form
                .template
                .settings
                .iter()
                .position(|s| s.key == "title")
                .unwrap();
            form.text_mut(title).unwrap().set("Work Claude");
            let src = form
                .template
                .settings
                .iter()
                .position(|s| s.key == "source")
                .unwrap();
            form.pick(src, "demo");
        }
        let Some(Op::SaveEdit { change, .. }) = m.save_edit(now) else {
            panic!("save")
        };
        assert_eq!(change.title.as_deref(), Some("Work Claude"));
        assert_eq!(change.source, Some(Source::Demo));
        assert_eq!(change.settings.unwrap()["source"], "demo");
    }

    #[test]
    fn a_rename_by_hand_survives_an_edit_that_doesnt_touch_the_title() {
        let t = hub_templates::built_in_template("usage-limits/1").unwrap();
        let settings = t
            .settings_from(
                &json!({"service": "claude-max", "source": "manual"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let mut p = page("claude-max", Source::Manual);
        p.title = "My Claude".into();
        p.kind = "meter".into();
        p.feed = Some("claude-max".into());
        p.template = Some("usage-limits/1".into());
        p.settings = Some(settings);
        let (mut m, now) = connected(vec![p]);
        m.begin_edit("claude-max", now);
        let form = m.edit.as_mut().unwrap().form.as_mut().unwrap();
        let src = form
            .template
            .settings
            .iter()
            .position(|s| s.key == "source")
            .unwrap();
        form.pick(src, "none");
        let Some(Op::SaveEdit { change, .. }) = m.save_edit(now) else {
            panic!("save")
        };
        assert_eq!(change.title.as_deref(), Some("My Claude"));
        assert_eq!(change.source, Some(Source::None));
    }

    #[test]
    fn a_change_to_the_kind_or_feed_is_refused_in_plain_words() {
        let t = hub_templates::built_in_template("usage-limits/1").unwrap();
        let settings = t
            .settings_from(
                &json!({"service": "claude-max", "source": "manual"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
        let mut p = page("claude-max", Source::Manual);
        p.kind = "meter".into();
        p.feed = Some("claude-max".into());
        p.template = Some("usage-limits/1".into());
        p.settings = Some(settings);
        let (mut m, now) = connected(vec![p]);
        m.begin_edit("claude-max", now);
        let form = m.edit.as_mut().unwrap().form.as_mut().unwrap();
        let svc = form
            .template
            .settings
            .iter()
            .position(|s| s.key == "service")
            .unwrap();
        form.pick(svc, "chatgpt-plus");
        assert!(m.save_edit(now).is_none());
        let msg = m
            .edit
            .as_ref()
            .unwrap()
            .form
            .as_ref()
            .unwrap()
            .error
            .clone()
            .unwrap();
        assert!(msg.contains("remove this page and add it again"), "{msg}");
        assert!(m.edit.is_some(), "the form stays open");
    }

    #[test]
    fn a_page_from_an_unknown_template_still_edits_its_title() {
        let mut p = page("x", Source::Manual);
        p.kind = "list".into();
        p.template = Some("someone-elses/3".into());
        let (mut m, now) = connected(vec![p]);
        m.begin_edit("x", now);
        let e = m.edit.as_mut().unwrap();
        assert!(e.form.is_none() && e.has_title_box());
        e.title.set(&"t".repeat(49));
        assert!(m.save_edit(now).is_none(), "over the limit is refused, not cut");
        assert!(m.notice.as_ref().unwrap().title.contains("1 to 48"));
    }

    #[test]
    fn only_a_usage_page_is_offered_sample_figures() {
        let mut usage_page = page("u", Source::Manual);
        usage_page.kind = "meter".into();
        usage_page.feed = Some("claude-max".into());
        let (mut m, now) = connected(vec![usage_page, page("t", Source::Manual)]);
        m.begin_edit("u", now);
        assert!(m.edit.as_ref().unwrap().source_choices().contains(&Source::Demo));
        assert!(
            m.edit
                .as_ref()
                .unwrap()
                .source_choices()
                .contains(&Source::Helper),
            "it has a feed"
        );
        m.begin_edit("t", now);
        let choices = m.edit.as_ref().unwrap().source_choices();
        assert!(!choices.contains(&Source::Demo) && !choices.contains(&Source::Helper));
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
    fn page_rows_say_what_each_page_is_and_where_it_comes_from() {
        let old = with_content(
            page("m", Source::Manual),
            DataQuality::Manual,
            "2026-11-08T20:00:00Z",
            json!({"text": "Back at 3"}),
        );
        let mut helper = with_content(
            page("h", Source::Helper),
            DataQuality::ProviderReported,
            "2026-11-10T19:50:00Z",
            json!({"value": 41.0, "unit": "°C"}),
        );
        helper.kind = "big_number".into();
        helper.feed = Some("pi-status".into());
        let mut custom = page("c", Source::Helper);
        custom.kind = "nearby-flights/1".into();
        let mut list = page("l", Source::Manual);
        list.kind = "list".into();
        let (m, now) = connected(vec![
            old,
            helper,
            page("d", Source::Demo),
            custom,
            list,
            page("n", Source::None),
        ]);
        let rows = m.page_rows(now);
        assert_eq!(
            (rows[0].kind.as_str(), rows[0].origin.as_str()),
            ("Short text", "Typed in")
        );
        assert_eq!((rows[0].quality, rows[0].stale), ("MANUAL", true));
        assert_eq!(rows[0].freshness, "Stale. Updated 2 d ago");
        assert_eq!(rows[0].shown, Shown::Text("Back at 3".into()));
        assert_eq!(rows[1].kind, "Big number");
        assert_eq!(rows[1].origin, "Filled by a helper (feed pi-status)");
        assert_eq!(
            (rows[1].freshness.as_str(), rows[1].quality),
            ("Updated 10 min ago", "REPORTED")
        );
        assert_eq!(
            rows[1].shown,
            Shown::BigNumber {
                value: "41 °C".into(),
                caption: None
            }
        );
        assert_eq!((rows[2].origin.as_str(), rows[2].quality), ("Demo", "DEMO"));
        assert_eq!(rows[3].kind, "Custom: nearby-flights/1");
        assert_eq!(rows[3].freshness, "Waiting for the helper's first update");
        assert_eq!(rows[4].freshness, "Nothing typed in yet");
        assert_eq!(
            (rows[5].quality, rows[5].freshness.as_str(), &rows[5].shown),
            ("UNAVAILABLE", "No content yet", &Shown::Empty)
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
        m.finish(Ok(Outcome::Refreshed(Some(refresh(vec![])))), now + REFRESH_EVERY);
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
                refresh: Some(refresh(vec![])),
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
            pages: None,
            firmware: None,
        });
        m.finish(Ok(Outcome::Refreshed(Some(r))), now + WATCH_EVERY);
        assert_eq!(m.enrol.stage, EnrolStage::Joined { name: "Desk".into() });
        assert!(m.enrol.bundle.is_none(), "the used secret is dropped");
    }

    fn display_simulator_like() -> hub_protocol::device::DeviceInfo {
        hub_protocol::device::DeviceInfo {
            device_type: "llm-usage-display".into(),
            firmware_version: "1.0.0".into(),
            hardware_revision: "x".into(),
            protocol_version: 1,
            firmware: None,
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
        let (mut m, _) = connected(vec![page("a", Source::Demo)]);
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
    // ---- ticks ----

    fn display(id: &str, name: &str) -> DisplayView {
        DisplayView {
            id: id.into(),
            name: name.into(),
            enrolled_at: ts("2026-11-10T19:00:00Z"),
            last_seen_at: None,
            device: None,
            pages: None,
            firmware: None,
        }
    }

    fn names(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("p{i:02}")).collect()
    }

    /// A connected model with `pages` pages and the given displays, each with its ticked page ids.
    fn with_ticks(pages: usize, shows: &[(&str, &str, Vec<String>)]) -> (Model, Instant) {
        let (mut m, now) = connected(vec![]);
        let mut r = refresh(names(pages).iter().map(|n| page(n, Source::Manual)).collect());
        for (id, name, ticked) in shows {
            r.displays.push(display(id, name));
            r.ticks.insert((*id).into(), ticked.clone());
        }
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        (m, now)
    }

    fn ticked_ids(m: &Model, d: &str) -> Vec<String> {
        m.tick_view(d)
            .unwrap()
            .rows
            .into_iter()
            .filter(|r| r.ticked)
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn ticks_list_every_page_in_order_and_change_only_when_saved() {
        let (mut m, _) = with_ticks(3, &[("d1", "Kitchen", vec!["p01".into(), "p03".into()])]);
        let v = m.tick_view("d1").unwrap();
        assert_eq!(
            v.rows
                .iter()
                .map(|r| (r.id.as_str(), r.ticked))
                .collect::<Vec<_>>(),
            [("p01", true), ("p02", false), ("p03", true)]
        );
        assert!(!v.dirty);
        assert_eq!(
            m.save_ticks("d1").map(|_| ()),
            None,
            "nothing changed, nothing sent"
        );

        m.toggle_tick("d1", "p02");
        m.toggle_tick("d1", "p01");
        let v = m.tick_view("d1").unwrap();
        assert!(v.dirty);
        assert_eq!(ticked_ids(&m, "d1"), ["p02", "p03"]);
        // The hub still has the old ticks until the save is answered.
        assert_eq!(m.last.as_ref().unwrap().ticks["d1"], ["p01", "p03"]);

        match m.save_ticks("d1") {
            Some(Op::SetDisplayPages { display, ids }) => {
                assert_eq!(display, "d1");
                assert_eq!(ids, ["p02", "p03"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(m.busy.is_some(), "one operation at a time");
    }

    #[test]
    fn changing_a_tick_back_is_not_a_change() {
        let (mut m, _) = with_ticks(2, &[("d1", "Kitchen", vec!["p01".into()])]);
        m.toggle_tick("d1", "p02");
        assert!(m.tick_view("d1").unwrap().dirty);
        m.toggle_tick("d1", "p02");
        assert!(!m.tick_view("d1").unwrap().dirty);
        assert!(m.tick_changes.is_empty());
    }

    #[test]
    fn cancel_drops_unsaved_changes() {
        let (mut m, _) = with_ticks(2, &[("d1", "Kitchen", vec!["p01".into()])]);
        m.toggle_tick("d1", "p02");
        m.cancel_ticks("d1");
        assert_eq!(ticked_ids(&m, "d1"), ["p01"]);
        assert!(!m.tick_view("d1").unwrap().dirty);
    }

    #[test]
    fn at_most_ten_can_be_ticked() {
        let (mut m, _) = with_ticks(12, &[("d1", "Kitchen", names(9))]);
        assert!(!m.tick_view("d1").unwrap().full);
        m.toggle_tick("d1", "p10");
        let v = m.tick_view("d1").unwrap();
        assert!(v.full && v.can_save);
        assert_eq!(v.count_text(), "10 of 10");
        assert_eq!(
            v.note().as_deref(),
            Some("A display shows at most 10 pages. Untick one to choose another.")
        );
        // The rest can't be ticked, and trying does nothing; ticked ones can still be unticked.
        assert!(v.rows.iter().filter(|r| !r.ticked).all(|r| !r.can_change));
        assert!(v.rows.iter().filter(|r| r.ticked).all(|r| r.can_change));
        m.toggle_tick("d1", "p11");
        assert_eq!(ticked_ids(&m, "d1").len(), 10);
        m.toggle_tick("d1", "p01");
        m.toggle_tick("d1", "p11");
        assert_eq!(ticked_ids(&m, "d1").len(), 10);
        assert!(!ticked_ids(&m, "d1").contains(&"p01".to_string()));
    }

    #[test]
    fn no_note_when_every_page_fits() {
        let (m, _) = with_ticks(10, &[("d1", "Kitchen", names(10))]);
        let v = m.tick_view("d1").unwrap();
        assert!(v.full);
        assert_eq!(v.note(), None, "nothing is left to choose");
    }

    #[test]
    fn a_display_with_more_than_ten_ticked_can_only_save_once_it_is_down_to_ten() {
        // The hub ticks a new page everywhere, so a full display can end up with eleven.
        let (mut m, _) = with_ticks(11, &[("d1", "Kitchen", names(11))]);
        let v = m.tick_view("d1").unwrap();
        assert_eq!(v.ticked, 11);
        assert!(!v.can_save);
        assert!(v.rows[10].waiting && !v.rows[9].waiting);
        assert!(v.note().unwrap().contains("Untick 1 to save"));
        assert!(m.save_ticks("d1").is_none(), "no change");
        m.toggle_tick("d1", "p11");
        let op = m.save_ticks("d1");
        assert!(matches!(op, Some(Op::SetDisplayPages { ref ids, .. }) if ids.len() == 10));
    }

    #[test]
    fn saving_shows_the_hubs_answer_and_keeps_changes_when_it_fails() {
        let (mut m, now) = with_ticks(2, &[("d1", "Kitchen", vec!["p01".into()])]);
        m.toggle_tick("d1", "p02");
        m.save_ticks("d1").unwrap();
        m.finish(Err(Failure::Client(ClientError::Timeout)), now);
        assert!(m.tick_view("d1").unwrap().dirty, "still there to try again");

        m.save_ticks("d1").unwrap();
        let mut r = refresh(vec![page("p01", Source::Manual), page("p02", Source::Manual)]);
        r.displays.push(display("d1", "Kitchen"));
        r.ticks.insert("d1".into(), vec!["p01".into(), "p02".into()]);
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        let v = m.tick_view("d1").unwrap();
        assert!(!v.dirty, "the hub now has what was asked for");
        assert_eq!(ticked_ids(&m, "d1"), ["p01", "p02"]);
        let n = m.notice.as_ref().unwrap();
        assert!(n.title.starts_with("Saved the pages on Kitchen."), "{}", n.title);
    }

    #[test]
    fn a_refresh_keeps_changes_for_pages_still_there_and_drops_the_rest() {
        let (mut m, now) = with_ticks(
            3,
            &[
                ("d1", "Kitchen", vec!["p01".into()]),
                ("d2", "Desk", vec!["p01".into()]),
            ],
        );
        m.toggle_tick("d1", "p02");
        m.toggle_tick("d1", "p03");
        m.toggle_tick("d2", "p02");
        // p03 is removed, and d2 is gone; a new page, p04, arrives ticked on d1.
        let mut r = refresh(vec![
            page("p01", Source::Manual),
            page("p02", Source::Manual),
            page("p04", Source::Manual),
        ]);
        r.displays.push(display("d1", "Kitchen"));
        r.ticks.insert("d1".into(), vec!["p01".into(), "p04".into()]);
        m.finish(Ok(Outcome::Refreshed(Some(r))), now);
        assert_eq!(m.tick_changes.len(), 1);
        assert_eq!(m.tick_changes["d1"].len(), 1);
        assert_eq!(
            ticked_ids(&m, "d1"),
            ["p01", "p02", "p04"],
            "the new page follows the hub"
        );
        assert!(m.tick_view("d2").is_none());
    }

    #[test]
    fn a_display_with_no_ticks_answer_has_no_list() {
        let (mut m, _) = with_ticks(2, &[]);
        m.last.as_mut().unwrap().displays.push(display("d9", "Hall"));
        assert!(m.tick_view("d9").is_none());
        m.toggle_tick("d9", "p01");
        assert!(m.tick_changes.is_empty());
        assert!(m.save_ticks("d9").is_none());
        // With no display saying what it shows, the Pages list doesn't claim "Not on any display".
        assert_eq!(m.on_displays_text("p01"), None);
    }

    #[test]
    fn each_page_says_which_displays_show_it() {
        let (m, now) = with_ticks(
            3,
            &[
                ("d1", "Kitchen", vec!["p01".into(), "p02".into()]),
                ("d2", "Desk", vec!["p01".into()]),
            ],
        );
        let rows = m.page_rows(now);
        assert_eq!(rows[0].on_displays.as_deref(), Some("On: Kitchen, Desk"));
        assert_eq!(rows[1].on_displays.as_deref(), Some("On: Kitchen"));
        assert_eq!(rows[2].on_displays.as_deref(), Some("Not on any display"));

        let (m, now) = with_ticks(1, &[]);
        assert_eq!(
            m.page_rows(now)[0].on_displays,
            None,
            "no displays, nothing to say"
        );
    }

    #[test]
    fn a_ticked_page_past_the_first_ten_is_not_on_the_display() {
        let (m, now) = with_ticks(11, &[("d1", "Kitchen", names(11))]);
        let rows = m.page_rows(now);
        assert_eq!(rows[9].on_displays.as_deref(), Some("On: Kitchen"));
        assert_eq!(rows[10].on_displays.as_deref(), Some("Not on any display"));
    }

    #[test]
    fn adding_a_page_says_which_displays_are_already_full() {
        let (mut m, now) = with_ticks(10, &[("d1", "Kitchen", names(10)), ("d2", "Desk", names(3))]);
        let mut r = refresh(names(11).iter().map(|n| page(n, Source::Manual)).collect());
        r.displays.push(display("d1", "Kitchen"));
        r.displays.push(display("d2", "Desk"));
        // The hub ticks the new page on every display.
        r.ticks.insert("d1".into(), names(11));
        let mut desk = names(3);
        desk.push("p11".into());
        r.ticks.insert("d2".into(), desk);
        m.busy = Some(Busy::Adding {
            done: "Added Page. Displays show it soon.".into(),
            setup: None,
            feed: None,
        });
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(r),
                failed: None,
            }),
            now,
        );
        let n = m.notice.as_ref().unwrap();
        assert_eq!(n.detail, "Not shown on Kitchen yet: it already shows 10 pages.");
    }

    #[test]
    fn adding_a_page_says_nothing_when_no_display_is_full() {
        let (mut m, now) = with_ticks(2, &[("d1", "Kitchen", names(2))]);
        let mut r = refresh(names(3).iter().map(|n| page(n, Source::Manual)).collect());
        r.displays.push(display("d1", "Kitchen"));
        r.ticks.insert("d1".into(), names(3));
        m.busy = Some(Busy::Adding {
            done: "Added Page.".into(),
            setup: None,
            feed: None,
        });
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(r),
                failed: None,
            }),
            now,
        );
        assert_eq!(m.notice.as_ref().unwrap().detail, "");
    }

    fn done_flags(m: &Model) -> Vec<bool> {
        m.guide_items()
            .expect("the checklist shows")
            .iter()
            .map(|i| i.done)
            .collect()
    }

    #[test]
    fn with_no_hub_the_first_run_guide_shows_and_there_is_no_checklist() {
        let m = Model::new(None, "PC");
        assert!(m.first_run() && m.guide_items().is_none());
        // A hub whose credential stopped working is not a first run: it just needs pairing again.
        let (mut m, _) = connected(vec![]);
        m.needs_pairing = true;
        assert!(!m.first_run() && m.guide_items().is_none());
    }

    #[test]
    fn the_checklist_ticks_itself_from_the_hubs_state() {
        // Nothing yet.
        let (m, _) = with_ticks(0, &[]);
        assert_eq!(done_flags(&m), [false, false, false]);
        assert_eq!(m.guide_next(), Some(0));
        // A page but no display.
        let (m, _) = with_ticks(2, &[]);
        assert_eq!(done_flags(&m), [false, true, false]);
        assert_eq!(m.guide_next(), Some(0));
        // A display that shows nothing.
        let (m, _) = with_ticks(2, &[("d1", "Desk", vec![])]);
        assert_eq!(done_flags(&m), [true, true, false]);
        assert_eq!(m.guide_next(), Some(2));
        // A display with no pages made yet can't be showing one.
        let (m, _) = with_ticks(0, &[("d1", "Desk", vec![])]);
        assert_eq!(done_flags(&m), [true, false, false]);
        assert_eq!(m.guide_next(), Some(1));
        // A display showing a page: all done.
        let (m, _) = with_ticks(2, &[("d1", "Desk", vec!["p01".into()])]);
        assert_eq!(done_flags(&m), [true, true, true]);
        assert_eq!(m.guide_next(), None);
        // A tick for a page that no longer exists doesn't count.
        let (m, _) = with_ticks(1, &[("d1", "Desk", vec!["gone".into()])]);
        assert_eq!(done_flags(&m), [true, true, false]);
    }

    #[test]
    fn the_checklist_waits_for_the_hub_and_never_shows_when_closed() {
        let hub = SavedHub::new(
            "192.168.1.20",
            8443,
            &[0xab; 32],
            "Desk PC",
            ts("2026-11-10T19:00:00Z"),
        );
        let m = Model::new(Some(hub), "Desk PC");
        assert!(m.guide_items().is_none(), "no answer from the hub yet");
        let (mut m, _) = with_ticks(1, &[]);
        assert!(m.guide_items().is_some());
        m.guide_dismissed = true;
        assert!(m.guide_items().is_none());
    }

    #[test]
    fn closing_the_checklist_saves_it_and_settings_brings_it_back() {
        let (mut m, now) = with_ticks(0, &[]);
        let Some(Op::SaveGuide { dismissed: true }) = m.dismiss_guide() else {
            panic!("saves the choice")
        };
        assert!(m.guide_dismissed && m.guide_items().is_none());
        m.finish(Ok(Outcome::GuideSaved), now);
        assert!(m.busy.is_none() && m.notice.is_none());
        m.section = Section::Settings;
        let Some(Op::SaveGuide { dismissed: false }) = m.show_guide_again() else {
            panic!("saves the choice")
        };
        assert!(!m.guide_dismissed && m.section == Section::Overview && m.guide_items().is_some());
    }

    #[test]
    fn closing_the_checklist_while_something_runs_applies_at_once_and_saves_afterwards() {
        let (mut m, now) = with_ticks(0, &[]);
        m.busy = Some(Busy::Refreshing);
        assert!(m.dismiss_guide().is_none(), "one operation at a time");
        assert!(
            m.guide_dismissed && m.guide_items().is_none(),
            "hidden straight away"
        );
        assert!(m.save_guide_if_pending().is_none(), "still busy");
        m.finish(Ok(Outcome::Refreshed(Some(refresh(vec![])))), now);
        let Some(Op::SaveGuide { dismissed: true }) = m.save_guide_if_pending() else {
            panic!("the choice is saved once the refresh is done")
        };
        m.finish(Ok(Outcome::GuideSaved), now);
        assert!(m.save_guide_if_pending().is_none(), "saved once");
        // Changing your mind while busy saves only the last choice.
        m.busy = Some(Busy::Refreshing);
        m.dismiss_guide();
        m.show_guide_again();
        m.busy = None;
        assert!(matches!(
            m.save_guide_if_pending(),
            Some(Op::SaveGuide { dismissed: false })
        ));
    }

    #[test]
    fn a_failed_save_of_the_choice_says_so() {
        let (mut m, now) = with_ticks(0, &[]);
        m.dismiss_guide();
        m.finish(Err(Failure::Settings("disk full".into())), now);
        assert!(m.notice.as_ref().is_some_and(|n| n.detail.contains("disk full")));
    }

    #[test]
    fn forgetting_the_hub_brings_the_whole_guide_back() {
        let (mut m, now) = with_ticks(0, &[]);
        m.guide_dismissed = true;
        m.finish(Ok(Outcome::Forgotten), now);
        assert!(m.first_run() && !m.guide_dismissed);
    }

    #[test]
    fn the_install_command_is_the_one_in_the_pi_guide() {
        let docs = include_str!("../../../docs/deploy-raspberry-pi.md");
        assert!(docs.contains(INSTALL_COMMAND) && docs.contains(NEW_PAIRING_COMMAND));
        let mut m = Model::new(None, "PC");
        assert!(!m.install_copied);
        let expected = pi_install::built_in().unwrap_or(INSTALL_COMMAND);
        assert_eq!(m.install_downloads(), pi_install::built_in().is_some());
        assert_eq!(m.copy_install(), expected);
        assert!(m.install_copied);
    }
}

#[cfg(test)]
mod helper_tests {
    use super::*;

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
        assert_eq!(
            (n.label.as_str(), n.feeds),
            ("PC", vec!["chatgpt-plus".to_string()])
        );
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
        let invite = HelperInvite {
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
                pages: 0,
                displays: 0,
                admins: 1,
            },
            pages: vec![],
            displays: vec![],
            preview: Snapshot {
                protocol_version: 1,
                message_type: "dashboard.snapshot".into(),
                generated_at: t,
                refresh_after_seconds: 300,
                pages: vec![],
                firmware_update: None,
            },
            helpers: vec![],
            ticks: Default::default(),
            firmware: None,
            firmware_error: None,
        };
        // A problem left from an earlier try, such as a bad feed name, goes once a string is made.
        m.helper.problem = Some("Bad_Name can't be a feed's name".into());
        m.notice = Some(Guidance {
            tone: Tone::Problem,
            title: "Bad_Name can't be a feed's name".into(),
            detail: String::new(),
            next: Next::None,
        });
        m.finish(
            Ok(Outcome::HelperInvite {
                invite,
                refresh: Some(refresh),
            }),
            Instant::now(),
        );
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

#[cfg(test)]
mod own_helper_tests {
    //! Pages for a maker's own helper, and helper invites for any feed.

    use super::tests::{connected, refresh};
    use super::*;

    fn helper_page(id: &str, feed: &str, title: &str) -> PageView {
        PageView {
            id: id.into(),
            title: title.into(),
            kind: "text".into(),
            source: Source::Helper,
            feed: Some(feed.into()),
            stale_after_seconds: Source::Helper.stale_after_seconds(),
            content: None,
            template: None,
            settings: None,
        }
    }

    /// Opens the form and fills it in: a shape's kind, or a kind of the maker's own.
    fn fill(m: &mut Model, title: &str, kind: Result<&'static str, &str>, feed: &str) {
        m.open_helper_page();
        let f = m.add.helper_page.as_mut().expect("the form");
        f.title.set(title);
        match kind {
            Ok(shape) => f.choose_kind(Some(shape)),
            Err(custom) => {
                f.choose_kind(None);
                f.custom_kind.set(custom);
            }
        }
        f.feed.set(feed);
    }

    fn error_after_submit(m: &mut Model) -> String {
        assert!(m.submit_helper_page().is_none(), "nothing is sent");
        assert!(m.busy.is_none());
        assert!(m.notice.is_none(), "it belongs to the form");
        m.add
            .helper_page
            .as_ref()
            .unwrap()
            .error
            .clone()
            .expect("a message")
    }

    #[test]
    fn a_page_for_your_own_helper_is_added_with_its_feed_and_then_says_how_to_feed_it() {
        let (mut m, now) = connected(vec![]);
        fill(&mut m, "  Bus times ", Ok("list"), " bus-times ");
        let Some(Op::AddPages { pages }) = m.submit_helper_page() else {
            panic!("add")
        };
        let p = &pages[0];
        assert_eq!(
            (p.title.as_str(), p.kind.as_str(), p.source, p.feed.as_deref()),
            ("Bus times", "list", Source::Helper, Some("bus-times"))
        );
        assert!(p.template.is_none() && p.settings.is_none() && p.id.is_none());
        assert!(m.submit_helper_page().is_none(), "one operation at a time");
        m.finish(
            Ok(Outcome::Added {
                refresh: Some(refresh(vec![helper_page("bus-times", "bus-times", "Bus times")])),
                failed: None,
            }),
            now,
        );
        assert!(m.add.helper_page.is_none(), "the form closes");
        let n = m.notice.clone().unwrap();
        assert_eq!(n.tone, Tone::Done);
        assert!(
            n.title.contains("Added Bus times") && n.title.contains("bus-times"),
            "{}",
            n.title
        );
        let steps = m.add.feed.clone().expect("how to feed it");
        assert_eq!(
            steps.pi_command,
            r#"sudo display-hub-helper add bus-times --run "$PWD/bus-times""#
        );
        assert_eq!(steps.send_command, "hub-send page bus-times --kind list --data -");
        assert!(steps.example.is_some());
        m.copy_feed_command(true);
        assert!(m.add.feed.as_ref().is_some_and(|f| f.pi_copied && !f.send_copied));
        m.copy_feed_command(false);
        assert!(m.add.feed.as_ref().unwrap().send_copied);
        m.finish_setup();
        assert!(m.add.feed.is_none(), "back to the list");
    }

    #[test]
    fn each_field_is_checked_by_the_hubs_rules_before_anything_is_sent() {
        let (mut m, _) = connected(vec![]);
        fill(&mut m, "", Ok("text"), "bus-times");
        assert!(error_after_submit(&mut m).contains("title"));
        fill(&mut m, &"w".repeat(MAX_TITLE_LEN + 1), Ok("text"), "bus-times");
        assert!(error_after_submit(&mut m).contains("at most 48 characters"));
        fill(&mut m, "\u{200b}", Ok("text"), "bus-times");
        assert!(error_after_submit(&mut m).contains("printable"));
        for bad in ["Bus-Times", "bus_times", "bus--times", "-bus", &"a".repeat(49)] {
            fill(&mut m, "Bus times", Ok("text"), bad);
            assert!(
                error_after_submit(&mut m).contains("lower-case letters and digits"),
                "{bad}"
            );
        }
        fill(&mut m, "Bus times", Ok("text"), "");
        assert!(error_after_submit(&mut m).contains("Name the feed"));
        // The message clears once someone types again.
        m.typing();
        assert!(m.add.helper_page.as_ref().unwrap().error.is_none());
        m.leave_form();
        assert!(m.add.helper_page.is_none());
    }

    #[test]
    fn a_kind_of_your_own_is_typed_in_and_checked() {
        let (mut m, _) = connected(vec![]);
        fill(&mut m, "Flights", Err(""), "nearby-flights");
        assert!(error_after_submit(&mut m).contains("Type the kind"));
        for bad in [
            "Flights",
            "nearby-flights",
            "nearby-flights/0",
            "nearby flights/1",
            "a/12345",
        ] {
            fill(&mut m, "Flights", Err(bad), "nearby-flights");
            assert!(error_after_submit(&mut m).contains("name, a slash"), "{bad}");
        }
        fill(&mut m, "Flights", Err(" nearby-flights/1 "), "nearby-flights");
        let Some(Op::AddPages { pages }) = m.submit_helper_page() else {
            panic!("add")
        };
        assert_eq!(pages[0].kind, "nearby-flights/1");
        // Choosing a shape again uses it, whatever is still in the box.
        let mut f = HelperPageForm::default();
        f.choose_kind(None);
        f.custom_kind.set("nearby-flights/1");
        f.choose_kind(Some("meter"));
        assert_eq!(f.kind(), "meter");
        let steps = FeedSteps::new("nearby-flights", "nearby-flights/1");
        assert_eq!(steps.example, None, "only the four shapes have an example");
        assert_eq!(
            steps.send_command,
            "hub-send page nearby-flights --kind nearby-flights/1 --data -"
        );
    }

    #[test]
    fn a_full_hub_refuses_a_page_for_your_own_helper() {
        let (mut full, _) = connected(
            (0..HUB_PAGE_LIMIT)
                .map(|i| helper_page(&format!("p{i}"), "x", &format!("P{i}")))
                .collect(),
        );
        full.open_helper_page();
        assert!(full.add.helper_page.is_none());
        assert!(full.notice.as_ref().unwrap().title.contains("at most 50"));

        // Filled while the hub had room, then the hub filled up from another PC.
        let (mut m, now) = connected(vec![]);
        fill(&mut m, "Bus times", Ok("text"), "bus-times");
        let pages = (0..HUB_PAGE_LIMIT)
            .map(|i| helper_page(&format!("p{i}"), "x", &format!("P{i}")))
            .collect();
        m.finish(Ok(Outcome::Refreshed(Some(refresh(pages)))), now);
        assert!(m.submit_helper_page().is_none());
        assert!(m.notice.as_ref().unwrap().title.contains("at most 50"));
    }

    #[test]
    fn a_taken_title_gets_a_number_and_a_long_feed_gets_a_shorter_name_on_the_pi() {
        let (mut m, _) = connected(vec![helper_page("bus", "bus", "Bus times")]);
        fill(&mut m, "Bus times", Ok("text"), "the-number-eleven-bus-to-town");
        let Some(Op::AddPages { pages }) = m.submit_helper_page() else {
            panic!("add")
        };
        assert_eq!(pages[0].title, "Bus times 2");
        let steps = FeedSteps::new("the-number-eleven-bus-to-town", "text");
        assert!(steps.pi_name.starts_with("the-number-") && steps.pi_name.len() <= PI_HELPER_NAME_LEN);
        assert!(steps.pi_command.contains(" --feed the-number-eleven-bus-to-town"));
        assert_eq!(FeedSteps::new("short-feed", "text").pi_name, "short-feed");
    }

    #[test]
    fn long_feeds_sharing_a_start_get_different_valid_pi_names() {
        let a = FeedSteps::new("garden-weather-station-north", "text").pi_name;
        let b = FeedSteps::new("garden-weather-station-south", "text").pi_name;
        assert_ne!(a, b);
        assert_eq!(
            a,
            FeedSteps::new("garden-weather-station-north", "text").pi_name,
            "stable"
        );
        for name in [
            a,
            b,
            FeedSteps::new("abcdefghijklmnopqrs-tuv-wxyz", "text").pi_name,
        ] {
            assert!(name.len() <= PI_HELPER_NAME_LEN, "{name}");
            assert!(valid_feed(&name), "{name}");
        }
    }

    #[test]
    fn a_usage_feed_invite_sets_its_box_and_an_unlisted_tick_is_kept() {
        let (mut m, _) = connected(vec![helper_page("garden", "garden-weather", "Garden")]);
        m.invite_for_feed("claude-max");
        assert!(m.helper.claude && !m.helper.chatgpt && m.helper.feeds.is_empty());
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds, ["claude-max"]);
        m.busy = None;

        // The page isn't listed yet (the refresh failed), but its feed is still sent and shown.
        m.invite_for_feed("new-feed");
        let choices = m.helper_feed_choices();
        assert!(choices
            .iter()
            .any(|c| c.feed == "new-feed" && c.ticked && c.pages.is_empty()));
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds, ["new-feed"]);
        m.busy = None;
        m.toggle_helper_feed("new-feed");
        assert!(m.helper_feed_choices().iter().all(|c| c.feed != "new-feed"));
    }

    #[test]
    fn a_tick_for_a_page_that_has_gone_is_dropped_at_the_next_refresh() {
        let (mut m, _) = connected(vec![helper_page("garden", "garden-weather", "Garden")]);
        m.toggle_helper_feed("garden-weather");
        m.helper.feeds.push("gone".into());
        let r = m.last.clone().expect("a refresh");
        assert!(!m.take_refresh(r, Instant::now()), "the lists were readable");
        assert_eq!(m.helper.feeds, ["garden-weather"]);
    }

    #[test]
    fn a_new_string_clears_only_the_forms_own_problem() {
        let (mut m, _) = connected(vec![]);
        m.helper.claude = false;
        m.helper.chatgpt = false;
        m.helper.other.set("Bad_Name");
        assert!(m.create_helper().is_none());
        assert_eq!(
            m.helper.problem.as_deref(),
            Some("Bad_Name can't be a feed's name")
        );
        // Something else is showing by the time the string arrives: it stays.
        m.notice = Some(Guidance {
            tone: Tone::Problem,
            title: "Couldn't read the firmware lists".into(),
            detail: String::new(),
            next: Next::None,
        });
        m.clear_helper_problem();
        assert!(m.notice.is_some(), "another notice stays");
        assert!(m.helper.problem.is_none());

        assert!(m.create_helper().is_none());
        m.clear_helper_problem();
        assert!(m.notice.is_none(), "the form's own problem goes");
    }

    #[test]
    fn an_over_long_paste_in_other_feeds_is_refused_not_cut() {
        let (mut m, _) = connected(vec![]);
        m.helper.claude = false;
        m.helper.chatgpt = false;
        let legal: Vec<String> = (0..HUB_PAGE_LIMIT)
            .map(|i| format!("{:0>48}", format!("f{i}")))
            .collect();
        m.helper.other.insert(&legal.join(" "));
        assert_eq!(
            m.helper.other.text,
            legal.join(" "),
            "the largest legal list fits whole"
        );
        m.helper.other.set("");
        m.helper.other.insert(&"a".repeat(HELPER_OTHER_MAX + 50));
        assert!(m.create_helper().is_none());
        assert!(m.notice.as_ref().unwrap().title.contains("too long"));
    }

    #[test]
    fn escape_acts_only_on_what_the_section_shows() {
        let (mut m, _) = connected(vec![helper_page("bus", "bus", "Bus times")]);
        m.add.feed = Some(FeedSteps::new("bus", "text"));
        m.invite_for_feed("bus");
        m.notice = Some(Guidance::done("Hi", "there"));
        m.escape();
        assert!(
            m.notice.is_none() && m.add.feed.is_some(),
            "notice goes; the add flow stays"
        );
        m.section = Section::Pages;
        m.escape();
        assert!(m.add.feed.is_none(), "on Pages it closes the add flow");
        m.section = Section::Displays;
        m.enrol.open = true;
        m.escape();
        assert!(!m.enrol.open);
    }

    #[test]
    fn the_examples_pass_the_hubs_own_checks() {
        use hub_protocol::shapes::SHAPES;
        for kind in SHAPES {
            let data: serde_json::Value = serde_json::from_str(example_data(kind).unwrap()).unwrap();
            let page = hub_protocol::Page {
                id: "p".into(),
                title: "P".into(),
                kind: kind.into(),
                data_quality: DataQuality::ProviderReported,
                updated_at: Some(tests::ts("2026-11-10T20:00:00Z")),
                stale_after_seconds: 3600,
                note: None,
                data: Some(data),
            };
            assert!(page.check().is_ok(), "{kind}: {:?}", page.check());
        }
    }

    #[test]
    fn an_invite_can_name_the_hubs_helper_feeds_and_typed_ones() {
        let mut usage = helper_page("claude-max", "claude-max", "Claude Max");
        usage.kind = "meter".into();
        let mut typed_in = helper_page("note", "notes", "Note");
        typed_in.source = Source::Manual;
        let (mut m, _) = connected(vec![
            usage,
            helper_page("bus-times", "bus-times", "Bus times"),
            typed_in,
            helper_page("bus-times-2", "bus-times", "Bus times later"),
            helper_page("garden", "garden-weather", "Garden"),
        ]);
        let choices = m.helper_feed_choices();
        assert_eq!(
            choices.iter().map(|c| c.feed.as_str()).collect::<Vec<_>>(),
            ["bus-times", "garden-weather"],
            "helper pages' feeds, once each, without the usage feeds or a typed-in page's"
        );
        assert_eq!(choices[0].pages, ["Bus times", "Bus times later"]);
        assert!(choices.iter().all(|c| !c.ticked));

        // The usage boxes still work as before.
        m.helper.claude = false;
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds, ["chatgpt-plus"]);
        assert_eq!(m.invite_programs(), (true, false));
        m.busy = None;

        m.helper.chatgpt = false;
        m.toggle_helper_feed("garden-weather");
        m.toggle_helper_feed("bus-times");
        m.toggle_helper_feed("bus-times");
        assert!(m.helper_feed_choices()[1].ticked);
        m.helper.other.set(" sauna-temp, garden-weather  bins ");
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds, ["garden-weather", "sauna-temp", "bins"], "each once");
        assert_eq!(m.invite_programs(), (false, true));
        m.busy = None;

        m.helper.claude = true;
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds[0], "claude-max");
        assert_eq!(m.invite_programs(), (true, true));
        m.busy = None;
    }

    #[test]
    fn a_bad_or_missing_feed_is_refused_before_anything_is_sent() {
        let (mut m, _) = connected(vec![helper_page("garden", "garden-weather", "Garden")]);
        m.helper.claude = false;
        m.helper.chatgpt = false;
        assert!(m.create_helper().is_none());
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .title
            .contains("Choose what the helper sends to"));
        m.helper.other.set("Sauna_Temp");
        assert!(m.create_helper().is_none());
        let n = m.notice.clone().unwrap();
        assert!(
            n.title.contains("Sauna_Temp") && n.detail.contains("lower-case"),
            "{n:?}"
        );
        assert!(m.busy.is_none());
        let many: Vec<String> = (0..=HUB_PAGE_LIMIT).map(|i| format!("f{i}")).collect();
        m.helper.other.set(&many.join(" "));
        assert!(m.create_helper().is_none());
        assert!(m.notice.as_ref().unwrap().title.contains("at most 50 feeds"));

        // A tick for a page that has since gone is dropped at the next refresh, not sent.
        m.helper.other.clear();
        m.toggle_helper_feed("garden-weather");
        let now = Instant::now();
        m.finish(Ok(Outcome::Refreshed(Some(refresh(vec![])))), now);
        assert!(m.helper.feeds.is_empty());
        assert!(m.create_helper().is_none());
        assert!(m
            .notice
            .as_ref()
            .unwrap()
            .title
            .contains("Choose what the helper sends to"));
    }

    #[test]
    fn inviting_from_the_new_page_readies_the_form_for_just_its_feed() {
        let (mut m, _) = connected(vec![helper_page("bus-times", "bus-times", "Bus times")]);
        m.helper.other.set("old");
        m.invite_for_feed("bus-times");
        assert_eq!(m.section, Section::Settings);
        assert!(!m.helper.claude && !m.helper.chatgpt && m.helper.other.text.is_empty());
        assert!(m.helper_feed_choices()[0].ticked);
        let Some(Op::NewHelper(n)) = m.create_helper() else {
            panic!("helper")
        };
        assert_eq!(n.feeds, ["bus-times"]);
    }
}
