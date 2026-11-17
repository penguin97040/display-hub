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
    DisplayView, EnrolmentGrant, HelperInvite, HelperView, HubStatus, NewHelper, NewPage, PageChange,
    PageContentInput, PageView,
};
use hub_core::freshness::{short_duration, Freshness};
use hub_core::model::clean_title;
use hub_core::Source;
use hub_protocol::limits::MAX_TITLE_LEN;
use hub_protocol::pairing::PairingString;
use hub_protocol::provisioning::{Bundle, Wifi, MAX_HOSTS};
use hub_protocol::{DataQuality, Snapshot, Timestamp};
use zeroize::Zeroizing;

use hub_templates::Template;
use usage::{clean_percent, Service};

use crate::field::Field;
use crate::forms::{ContentEdit, TemplateForm};
use crate::guidance::{self, Doing, Guidance, Next, Tone};
use crate::settings::SavedHub;
use crate::shown::{kind_label, origin_text, Shown};

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
    pub pages: Vec<PageView>,
    pub displays: Vec<DisplayView>,
    pub preview: Snapshot,
    pub helpers: Vec<HelperView>,
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
    NewEnrolment {
        name: String,
    },
    NewHelper(NewHelper),
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
    /// Pages were added. `failed` is set when one of them wasn't, and the ones after it weren't tried.
    Added {
        refresh: Refresh,
        failed: Option<AddFailure>,
    },
    Enrolment {
        grant: EnrolmentGrant,
        refresh: Refresh,
    },
    HelperInvite {
        invite: HelperInvite,
        refresh: Refresh,
    },
    Ports(Vec<PortInfo>),
    Sent(UsbDisplay),
    Saved(PathBuf),
    Forgotten,
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
}

/// What is running, for the busy label and to know what a failure meant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Busy {
    Pairing,
    Refreshing,
    Saving {
        done: String,
    },
    /// Adding pages from a template.
    Adding {
        done: String,
        setup: Option<SetupNote>,
    },
    Enrolling,
    Ports,
    Sending {
        port: String,
    },
    SavingFile,
    Forgetting,
}

impl Busy {
    pub fn label(&self) -> &'static str {
        match self {
            Busy::Pairing => "Pairing",
            Busy::Refreshing => "Checking the hub",
            Busy::Saving { .. } => "Saving",
            Busy::Adding { .. } => "Adding",
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
                 that PC, usage-helper logout tidies up."
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

/// Adding pages from a template: pick one, fill in its form, then perhaps a setup step.
pub struct AddFlow {
    /// The templates that come with the app.
    pub templates: Vec<Template>,
    /// The chosen template's form. Without one, the picker shows.
    pub form: Option<TemplateForm>,
    /// What to do next, after pages were added.
    pub setup: Option<SetupNote>,
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
    match same.iter().position(|p| p.title == title) {
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
            // A page with nothing typed in yet and a blank editor stays as it is.
            if stored || !editor.is_blank() {
                let built = editor.build(hub_now).map_err(|p| {
                    EditError::Notice(Guidance {
                        tone: Tone::Problem,
                        title: p.title,
                        detail: p.detail,
                        next: Next::None,
                    })
                })?;
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
}

/// Setting up a PC helper (`usage-helper`).
#[derive(Default)]
pub struct HelperForm {
    pub claude: bool,
    pub chatgpt: bool,
    /// The `dh1.` string for `usage-helper login`, once made. A secret until used or expired.
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
    pub add: AddFlow,
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
            add: AddFlow {
                templates: hub_templates::built_in(),
                form: None,
                setup: None,
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
            self.notice = None;
        }
    }

    /// Back to the list of templates.
    pub fn leave_form(&mut self) {
        self.add.form = None;
    }

    /// Checks the form and adds the pages it makes.
    pub fn submit_add(&mut self) -> Option<Op> {
        let form = self.add.form.as_mut()?;
        let pages = match form.check() {
            Ok(p) => p,
            Err(e) => {
                form.error = Some(e);
                return None;
            }
        };
        form.error = None;
        let setup = form.template.setup.clone();
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
        self.start(Busy::Adding { done, setup }, Op::AddPages { pages })
    }

    /// The command to put on the clipboard, and a note that it was copied.
    pub fn copy_setup(&mut self) -> Option<String> {
        let s = self.add.setup.as_mut()?;
        s.copied = true;
        Some(s.command.clone())
    }

    /// Done with the setup step: back to the list of templates.
    pub fn finish_setup(&mut self) {
        self.add.setup = None;
        self.add.form = None;
    }

    /// Clears messages about what was typed once someone types again.
    pub fn typing(&mut self) {
        if let Some(f) = self.add.form.as_mut() {
            f.error = None;
        }
        if let Some(f) = self.edit.as_mut().and_then(|e| e.form.as_mut()) {
            f.error = None;
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

    pub fn helpers(&self) -> &[HelperView] {
        self.last
            .as_ref()
            .map(|r| r.helpers.as_slice())
            .unwrap_or_default()
    }

    pub fn create_helper(&mut self) -> Option<Op> {
        // Each service the helper reports is the feed of the same name.
        let feeds: Vec<String> = [
            (self.helper.claude, Service::ClaudeMax),
            (self.helper.chatgpt, Service::ChatgptPlus),
        ]
        .into_iter()
        .filter(|&(on, _)| on)
        .map(|(_, s)| s.slug().to_string())
        .collect();
        if feeds.is_empty() {
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
            Op::NewHelper(NewHelper { label, feeds }),
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
            Ok(Outcome::Added { refresh, failed }) => {
                self.take_refresh(refresh, now);
                self.add.form = None;
                match (failed, busy) {
                    (None, Some(Busy::Adding { done, setup })) => {
                        self.add.setup = setup;
                        self.notice = Some(Guidance::done(done, ""));
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
                                "{} wasn't added: {}. {} The pages added before it are on the list.",
                                f.title,
                                why.title.to_lowercase(),
                                why.detail
                            ),
                            next: why.next,
                        });
                    }
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
        if matches!(busy, Some(Busy::Saving { .. } | Busy::Adding { .. })) {
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

    fn ts(s: &str) -> Timestamp {
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

    fn refresh(pages: Vec<PageView>) -> Refresh {
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
            },
            helpers: vec![],
        }
    }

    fn connected(pages: Vec<PageView>) -> (Model, Instant) {
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
                refresh: refresh(vec![page("x", Source::Manual)]),
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
                refresh: refresh(vec![page("pi-status", Source::Helper)]),
                failed: None,
            }),
            now,
        );
        let setup = m.add.setup.clone().expect("a setup step");
        assert!(setup.command.starts_with("sudo display-hub-helper add pi-status"));
        assert!(setup.help.contains("Raspberry Pi") && !setup.copied);
        assert_eq!(m.copy_setup().as_deref(), Some(setup.command.as_str()));
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
                refresh: refresh(vec![page("a", Source::Manual)]),
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
        assert!(m.notice.as_ref().unwrap().title.contains("0 to 100"));
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
        m.finish(Ok(Outcome::Refreshed(again)), now);
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
