//! The Firmware screen and the firmware part of each display card: wording and layout only. What
//! they show comes from the model (`model/updates.rs`, `firmware.rs`).

use std::path::PathBuf;
use std::time::Instant;

use gpui::accesskit::Role;
use gpui::{div, prelude::*, px, ClipboardItem, Context, Div, ElementId, SharedString, Window};

use super::{nz_date, waiting, FieldId, Kind, Manager};
use crate::firmware::{Signer, SignerKind, Trust};
use crate::guidance::{self, size_text, Guidance};
use crate::model::{InstallStage, Model, Op, PackageSays, PackageSource};
use crate::theme::*;
use crate::ui::{self, c, keep_case, Metrics};

/// "the key the hub calls "Kea Displays" (3f9a-01c2-…)".
fn signer_text(s: &Signer) -> String {
    guidance::signer_phrase(s)
}

/// "rev-a, rev-b", each in its own case in an upper-case label.
fn hardware_text(revisions: &[String]) -> String {
    revisions
        .iter()
        .map(|r| keep_case(r))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The maker's own words about a release, marked as theirs and kept apart from the app's.
fn maker_note(note: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(ui::label("The maker's note"))
        .child(ui::p_muted(note))
}

/// What a package says about itself, in a confirmation: each value as the maker wrote it, then
/// the maker's note.
pub(super) fn package_says(says: &PackageSays) -> Div {
    let mut out = div()
        .id("package-says")
        .role(Role::Group)
        .aria_label("What the package says about itself")
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .border_1()
        .border_color(c(LINE))
        .child(ui::label("What the package says about itself"));
    for (what, value) in &says.facts {
        out = out.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .child(ui::label(what))
                .child(ui::data(format!("\"{value}\""))),
        );
    }
    if let Some(n) = &says.note {
        out = out.child(maker_note(n.clone()));
    }
    div().child(out)
}

impl Manager {
    /// Opens the system's file picker, then hands the chosen file to the model.
    fn prompt_open(&mut self, cx: &mut Context<Self>, then: fn(&mut Model, PathBuf) -> Option<Op>) {
        let answer = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let op = then(&mut this.model, path);
                        this.run(op, cx);
                    });
                }
            }
        })
        .detach();
    }

    /// Redraws while an install runs, so its progress line moves.
    pub(super) fn watch_install(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let going = this.update(cx, |this, cx| {
                cx.notify();
                matches!(this.model.busy, Some(crate::model::Busy::Installing { .. }))
            });
            if !matches!(going, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    pub(super) fn firmware(
        &mut self,
        m: Metrics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let head = self
            .head(
                "Firmware",
                "Signed firmware for your displays: the packages on the hub, installing over USB, and who may add \
                 packages.",
                m,
                cx,
            )
            .into_any_element();
        if self.model.last.is_none() {
            return vec![head, waiting(m).into_any_element()];
        }
        if self.model.shelf().is_none() {
            // Installing from a file over USB needs nothing new from the hub, so it stays available.
            let note = ui::with_ticks(ui::panel(Some(("Firmware on the hub", "")), m).child(ui::body(m).child(
                ui::p_muted(
                    "This hub doesn't handle firmware updates yet. Update the hub to the same release as this app \
                     to keep firmware on it and update displays over Wi-Fi. Installing from a file over USB works \
                     already.",
                ),
            )))
            .into_any_element();
            let install = self.install_panel(m, window, cx);
            let row = if m.wide {
                div()
                    .flex()
                    .items_start()
                    .gap(px(m.gap))
                    .child(div().flex_1().min_w_0().child(note))
                    .child(div().w(px(500.0)).flex_none().child(install))
            } else {
                div().flex().flex_col().gap(px(m.gap)).child(install).child(note)
            };
            return vec![head, row.into_any_element()];
        }
        let packages = self.packages_panel(m, cx);
        let keys = self.keys_panel(m, window, cx);
        let publishers = self.publishers_panel(m, window, cx);
        let install = self.install_panel(m, window, cx);
        let row = if m.wide {
            div()
                .flex()
                .items_start()
                .gap(px(m.gap))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(m.gap))
                        .child(packages)
                        .child(keys)
                        .child(publishers),
                )
                .child(div().w(px(500.0)).flex_none().child(install))
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(m.gap))
                .child(install)
                .child(packages)
                .child(keys)
                .child(publishers)
        };
        vec![head, row.into_any_element()]
    }

    fn packages_panel(&mut self, m: Metrics, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let add = self.button(
            "fw-add",
            "Add a firmware file",
            Kind::Primary,
            !busy,
            cx,
            |this, _, cx| this.prompt_open(cx, Model::upload_firmware),
        );
        let rows = self.model.package_rows();
        let mut list = div().flex().flex_col();
        if rows.is_empty() {
            list = list.child(ui::p_muted(
                "No firmware on the hub yet. Add a .dhfw file from your display's maker, or they can add it \
                 themselves as a publisher.",
            ));
        }
        for p in rows {
            let (a, b) = (p.id.clone(), p.id.clone());
            let remove = self.button(
                format!("fw-remove-{}", p.id),
                "Remove",
                Kind::Quiet,
                !busy,
                cx,
                move |this, window, cx| {
                    this.model.ask_remove_package(&a);
                    let fh = this.fh("confirm-no", cx);
                    window.focus(&fh, cx);
                },
            );
            let usb = p.usb_install.then(|| {
                self.button(
                    format!("fw-usb-{}", p.id),
                    "Install over USB",
                    Kind::Secondary,
                    !busy,
                    cx,
                    move |this, _, cx| {
                        let op = this.model.load_package(PackageSource::Hub(b.clone()));
                        this.run(op, cx);
                    },
                )
            });
            let mut facts = vec![
                format!("For {}", hardware_text(&p.hardware_revisions)),
                size_text(p.size),
                if p.usb_install {
                    "Can do a first install over USB".into()
                } else {
                    "Updates only".into()
                },
                format!("Added {}", nz_date(p.uploaded_at)),
            ];
            if !p.offered_to.is_empty() {
                facts.push(format!("On offer to {}", p.offered_to.join(", ")));
            }
            list = list.child(
                div()
                    .id(ElementId::Name(format!("fw-{}", p.id).into()))
                    .role(Role::Group)
                    .aria_label(SharedString::from(format!("{} {}", p.device_type, p.version)))
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(12.0))
                    .border_b_1()
                    .border_color(c(LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(240.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(ui::h3(format!("{} {}", p.device_type, p.version)))
                            .child(ui::label(&format!("Signed by {}", signer_text(&p.signer))))
                            .when_some(p.summary.clone(), |d, s| d.child(maker_note(s)))
                            .child(ui::label(&facts.join(" · "))),
                    )
                    .child(div().flex().gap(px(8.0)).children(usb).child(remove)),
            );
        }
        let count = self.model.firmware_packages().len().to_string();
        ui::with_ticks(
            ui::panel(Some(("Firmware on the hub", &count)), m).child(
                ui::body(m)
                    .child(ui::p_muted(
                        "The app checks a file before it goes to the hub, and each display checks it again with its \
                         own keys before installing. Nothing installs until you press Update on a display.",
                    ))
                    .child(div().flex().child(add))
                    .child(list),
            ),
        )
    }

    fn keys_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let editing = self.model.fw.label_edit.as_ref().map(|e| e.key_id.clone());
        let mut list = div().flex().flex_col();
        let rows = self.model.key_rows();
        if rows.is_empty() {
            list = list.child(ui::p_muted(
                "No keys yet. They appear with packages and displays.",
            ));
        }
        for k in rows {
            let mut uses = Vec::new();
            if k.packages > 0 {
                uses.push(format!(
                    "Signs {} package{} on the hub",
                    k.packages,
                    if k.packages == 1 { "" } else { "s" }
                ));
            }
            if !k.trusted_by.is_empty() {
                uses.push(format!("Trusted by {}", k.trusted_by.join(", ")));
            }
            let mut row = div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .py(px(12.0))
                .border_b_1()
                .border_color(c(LINE))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.0))
                        .child(ui::h3(k.signer.name()))
                        .when(k.signer.kind == SignerKind::Official, |d| {
                            d.child(ui::tag("Built into the app", MUTED))
                        })
                        .when(matches!(k.signer.kind, SignerKind::Labelled(_)), |d| {
                            d.child(ui::tag("The hub's name for it", MUTED))
                        }),
                )
                .child(ui::data(k.signer.fingerprint.clone()))
                .when(!uses.is_empty(), |d| d.child(ui::label(&uses.join(" · "))));
            if k.signer.kind != SignerKind::Official {
                if editing.as_deref() == Some(k.signer.key_id.as_str()) {
                    let field = self.field(
                        FieldId::SignerLabel,
                        "Your name for this key",
                        "Such as the maker's name. Leave it empty to remove the name. Enter saves.",
                        window,
                        cx,
                    );
                    let save = self.button("label-save", "Save", Kind::Primary, !busy, cx, |this, _, cx| {
                        let op = this.model.save_label();
                        this.run(op, cx);
                    });
                    let cancel =
                        self.button("label-cancel", "Cancel", Kind::Quiet, true, cx, |this, _, _| {
                            this.model.cancel_label()
                        });
                    row = row
                        .child(field)
                        .child(div().flex().gap(px(8.0)).child(save).child(cancel));
                } else {
                    let id = k.signer.key_id.clone();
                    let name = self.button(
                        format!("label-{}", k.signer.key_id),
                        if k.labelled { "Rename" } else { "Name it" },
                        Kind::Quiet,
                        !busy,
                        cx,
                        move |this, window, cx| {
                            this.model.begin_label(&id);
                            let fh = this.fh(format!("field-{:?}", FieldId::SignerLabel), cx);
                            window.focus(&fh, cx);
                        },
                    );
                    row = row.child(div().flex().child(name));
                }
            }
            list = list.child(row);
        }
        ui::with_ticks(
            ui::panel(Some(("Makers' keys", "Names")), m).child(
                ui::body(m)
                    .child(ui::p_muted(
                        "Each package is signed with its maker's key. The app names its own official keys; you can \
                         name the others. Names are kept on the hub, so they are only as trustworthy as the hub: \
                         the fingerprint is what counts. Compare it with the one the maker publishes.",
                    ))
                    .child(list),
            ),
        )
    }

    fn publishers_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let label = self.field(
            FieldId::PublisherLabel,
            "Publisher's name",
            "Such as the maker, or the computer they build firmware on.",
            window,
            cx,
        );
        let create = self.button(
            "publisher-create",
            "Create publisher string",
            Kind::Secondary,
            !busy,
            cx,
            |this, _, cx| {
                let op = this.model.create_publisher();
                this.run(op, cx);
            },
        );
        let mut body = ui::body(m)
            .child(ui::p_muted(
                "A publisher is a firmware maker's hub-firmware tool. It can add packages to this hub and list \
                 them, and nothing else: installing still takes your Update, and each display's own checks.",
            ))
            .child(label)
            .child(div().flex().child(create));
        if let Some(s) = self.model.fw.publisher_string.clone() {
            let shown: String = s
                .as_bytes()
                .chunks(24)
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            let full = s.to_string();
            let copy = self.button(
                "publisher-copy",
                "Copy publisher string",
                Kind::Primary,
                true,
                cx,
                move |this, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(full.clone()));
                    this.model.notice = Some(Guidance::done(
                        "Copied the publisher string",
                        "Paste it where hub-firmware login asks. It works once, within 10 minutes.",
                    ));
                },
            );
            body = body.child(
                div()
                    .id("publisher-string")
                    .role(Role::Group)
                    .aria_label("Publisher string")
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .p(px(14.0))
                    .border_1()
                    .border_color(c(LINE))
                    .child(ui::label("Publisher string: works once, within 10 minutes"))
                    .child(ui::data(shown))
                    .child(div().flex().child(copy))
                    .child(ui::p_muted(
                        "On the maker's computer, run hub-firmware login and paste it. Then hub-firmware add sends \
                         a package here.",
                    )),
            );
        }
        let publishers = self.model.publishers().to_vec();
        if !publishers.is_empty() {
            let mut list = div().flex().flex_col().gap(px(8.0)).child(ui::label("Set up"));
            for p in publishers {
                let id = p.id.clone();
                let row = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.0))
                    .py(px(6.0))
                    .border_t_1()
                    .border_color(c(LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(200.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(ui::h3(p.label.clone()))
                            .child(ui::label(&format!("Paired {}", nz_date(p.created_at)))),
                    );
                list = list.child(if p.revoked {
                    row.child(ui::tag("Removed", MUTED))
                } else {
                    row.child(self.button(
                        format!("publisher-revoke-{id}"),
                        "Remove",
                        Kind::Quiet,
                        !busy,
                        cx,
                        move |this, window, cx| {
                            this.model.ask_revoke_publisher(&id);
                            let fh = this.fh("confirm-no", cx);
                            window.focus(&fh, cx);
                        },
                    ))
                });
            }
            body = body.child(list);
        }
        ui::with_ticks(ui::panel(Some(("Publishers", "Makers")), m).child(body))
    }

    fn install_panel(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let stage = self.model.fw.install.stage.clone();
        let cancel = self.button(
            "install-cancel",
            "Cancel",
            Kind::Quiet,
            !busy,
            cx,
            |this, _, _| this.model.close_install(),
        );
        let body = match stage {
            InstallStage::Choose => {
                let file = self.button(
                    "install-file",
                    "Choose a file",
                    Kind::Primary,
                    !busy,
                    cx,
                    |this, _, cx| {
                        this.model.open_install();
                        this.prompt_open(cx, |m, path| m.load_package(PackageSource::File(path)));
                    },
                );
                let mut from_hub = div().flex().flex_col().gap(px(8.0));
                let usable: Vec<_> = self
                    .model
                    .package_rows()
                    .into_iter()
                    .filter(|p| p.usb_install)
                    .collect();
                if !usable.is_empty() {
                    from_hub = from_hub.child(ui::label("Or one on the hub"));
                }
                for p in usable {
                    let id = p.id.clone();
                    from_hub = from_hub.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(12.0))
                            .child(self.button(
                                format!("install-hub-{}", p.id),
                                &format!("Use {} {}", keep_case(&p.device_type), keep_case(&p.version)),
                                Kind::Secondary,
                                !busy,
                                cx,
                                move |this, _, cx| {
                                    let op = this.model.load_package(PackageSource::Hub(id.clone()));
                                    this.run(op, cx);
                                },
                            ))
                            .child(ui::label(&format!(
                                "For {} · Signed by {}",
                                hardware_text(&p.hardware_revisions),
                                signer_text(&p.signer)
                            ))),
                    );
                }
                ui::body(m)
                    .child(ui::p_muted(
                        "Install firmware erases a display and puts its maker's firmware on it, over the USB cable. \
                         Use it for a new display, or for one that trusts no keys. Afterwards the display trusts that \
                         maker's keys, so it can take updates through the hub.",
                    ))
                    .child(div().flex().flex_wrap().gap(px(8.0)).child(file).when(
                        self.model.fw.install.open,
                        |d| d.child(cancel),
                    ))
                    .child(from_hub)
            }
            InstallStage::Ready => self
                .install_ready(m, window, cx)
                .child(div().flex().child(cancel)),
            InstallStage::Running { port } => {
                let p = self.model.install_progress();
                ui::body(m)
                    .child(ui::h3(format!("Installing on {port}")))
                    .child(
                        div()
                            .id("install-progress")
                            .role(Role::Status)
                            .aria_label(SharedString::from(guidance::install_step(&p)))
                            .child(ui::p(guidance::install_step(&p))),
                    )
                    .child(ui::p_muted(
                        "Keep the cable plugged in, and leave the app open until it finishes.",
                    ))
            }
            InstallStage::Done { name, port } => {
                let done = self.button("install-done", "Done", Kind::Secondary, true, cx, |this, _, _| {
                    this.model.close_install()
                });
                ui::body(m)
                    .child(ui::h3(format!("{name} is installed")))
                    .child(ui::p_muted(format!(
                        "The display on {port} has its new firmware, its setup details and its maker's keys. It \
                         joins Wi-Fi and checks in with the hub; the Displays screen shows it when it does."
                    )))
                    .child(div().flex().child(done))
            }
        };
        ui::with_ticks(ui::panel(Some(("Install firmware", "USB")), m).child(body))
    }

    /// A package is chosen: who signed it, the display's details, and the port.
    fn install_ready(&mut self, m: Metrics, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let busy = self.model.busy.is_some();
        let Some(loaded) = self.model.fw.install.loaded.clone() else {
            return ui::body(m);
        };
        let signer = self
            .model
            .install_signer()
            .expect("a loaded package has a signer");
        let man = &loaded.checked.manifest;
        let usb = man.usb_install.as_ref();
        let needs = usb.map_or(String::new(), |u| {
            format!(
                "For an {} board with at least {} MB of flash.",
                u.chip,
                u.min_flash_bytes / (1024 * 1024)
            )
        });
        let who = match &signer.kind {
            SignerKind::Official => div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(ui::h3(signer.name()))
                .child(ui::data(signer.fingerprint.clone())),
            kind => {
                let heading = match kind {
                    SignerKind::Labelled(l) => format!("Signed by the key the hub calls \"{l}\""),
                    _ => "Signed by a maker this app doesn't know".into(),
                };
                // An older hub can't keep names for keys.
                let maker = (*kind == SignerKind::Unknown && self.model.hub_has_firmware()).then(|| {
                    self.field(
                        FieldId::InstallMaker,
                        "Your name for this maker (optional)",
                        "Saved on the hub, so displays and packages signed with this key show it.",
                        window,
                        cx,
                    )
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(ui::h3(heading))
                    .child(ui::label("Key fingerprint"))
                    .child(ui::data(signer.fingerprint.clone()).text_size(px(20.0)))
                    .child(ui::p(
                        "Compare this fingerprint with the one the maker publishes, such as on their website or \
                         release page. Go on only if they match.",
                    ))
                    .children(maker)
            }
        };
        let signer_box = div()
            .id("install-signer")
            .role(Role::Group)
            .aria_label(SharedString::from(format!(
                "Who signed it: {}, fingerprint {}",
                signer.name(),
                signer.fingerprint
            )))
            .p(px(14.0))
            .border_1()
            .border_color(c(BEAK))
            .child(who);
        let name = self.field(FieldId::InstallName, "Display name", "", window, cx);
        let ssid = self.field(
            FieldId::InstallSsid,
            "Wi-Fi network (2.4 GHz)",
            "Installing erases the display's Wi-Fi settings.",
            window,
            cx,
        );
        let pw = self.field(
            FieldId::InstallPassword,
            "Wi-Fi password",
            "Empty for an open network.",
            window,
            cx,
        );
        let ports = self.model.enrol.ports.clone();
        let mut list = div().flex().flex_col().gap(px(8.0));
        if ports.is_empty() {
            list = list.child(ui::p_muted(
                "No serial ports found. Plug the display in with a data cable, then look again.",
            ));
        }
        for (i, p) in ports.iter().enumerate() {
            let port = p.name.clone();
            let go = self.button(
                format!("install-port-{i}"),
                &format!("Erase and install on {}", p.name),
                Kind::Secondary,
                !busy,
                cx,
                move |this, window, cx| {
                    let op = this.model.ask_install(&port);
                    if this.model.confirm.is_some() {
                        let fh = this.fh("confirm-no", cx);
                        window.focus(&fh, cx);
                    }
                    this.run(op, cx);
                },
            );
            list = list.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(12.0))
                    .child(go)
                    .child(ui::label(&p.description)),
            );
        }
        let rescan = self.button(
            "install-rescan",
            "Look again",
            Kind::Quiet,
            !busy,
            cx,
            |this, _, cx| {
                let op = this.model.list_ports();
                this.run(op, cx);
            },
        );
        let other = self.field(
            FieldId::InstallPort,
            "Or type a port",
            "Such as COM5, or /dev/ttyACM0. Enter goes on.",
            window,
            cx,
        );
        let again = self.button(
            "install-again",
            "Choose another package",
            Kind::Quiet,
            !busy,
            cx,
            |this, _, _| this.model.choose_again(),
        );
        let hardware = format!("For hardware {}", hardware_text(&man.hardware_revisions));
        ui::body(m)
            .child(ui::h3(loaded.title()))
            .when_some(man.summary.clone(), |d, s| d.child(maker_note(s)))
            .child(ui::label(&hardware))
            .child(ui::label(&needs))
            .child(signer_box)
            .child(name)
            .child(ssid)
            .child(pw)
            .child(ui::p_muted(
                "If the board isn't found in download mode: unplug the cable, hold the BOOT button while you plug it \
                 back in, then let go.",
            ))
            .child(list)
            .child(other)
            .child(div().flex().flex_wrap().gap(px(8.0)).child(rescan).child(again))
    }

    /// The firmware part of a display card. `None` for a hub without signed updates.
    pub(super) fn display_firmware_block(&mut self, display: &str, cx: &mut Context<Self>) -> Option<Div> {
        let now = Instant::now();
        let fw = self.model.display_firmware(display, now)?;
        if fw.hub_unaware {
            return None;
        }
        let busy = self.model.busy.is_some();
        let mut out = div().flex().flex_col().gap(px(8.0)).child(ui::label("Firmware"));
        if let Some(v) = &fw.version {
            let line = match &fw.signed_by {
                Some(s) => format!("{v}, signed by {}", signer_text(s)),
                None => v.clone(),
            };
            out = out.child(ui::p(line));
        }
        if fw.paused {
            out = out.child(ui::p_muted(guidance::paused_words(
                &self.model.display_name(display),
            )));
        }
        let install_button = |this: &mut Self, cx: &mut Context<Self>| {
            this.button(
                format!("display-install-{display}"),
                "Install firmware",
                Kind::Secondary,
                !busy,
                cx,
                |this, _, _| this.model.open_install(),
            )
        };
        match &fw.trust {
            Trust::NotReported if fw.version.is_some() => {
                out = out
                    .child(ui::p_muted(
                        "This display hasn't said which firmware keys it trusts, so it can't take updates over \
                         Wi-Fi. Its firmware may be older than signed updates. Install firmware over USB gives it \
                         new firmware and its maker's keys.",
                    ))
                    .child(div().flex().child(install_button(self, cx)));
            }
            Trust::NotReported => {}
            Trust::NoKeys => {
                out = out
                    .child(ui::p_muted(
                        "This display trusts no firmware keys, so it refuses every update over Wi-Fi. Install \
                         firmware over USB once to give it its maker's keys.",
                    ))
                    .child(div().flex().child(install_button(self, cx)));
            }
            Trust::Keys(keys) => {
                let names: Vec<String> = keys.iter().map(signer_text).collect();
                out = out.child(ui::label(&format!(
                    "Accepts updates signed by {}",
                    names.join(", ")
                )));
            }
        }
        let open = fw.offer.as_ref().is_some_and(|(_, p)| p.is_open());
        if let Some((offer, phase)) = &fw.offer {
            let retry = fw.retry(&offer.package_id);
            let (title, detail) = guidance::offer_words(
                phase,
                offer,
                fw.version.as_deref(),
                &self.model.poll_text(),
                retry.as_ref(),
            );
            let d = display.to_string();
            let cancel = open.then(|| {
                self.button(
                    format!("offer-cancel-{display}"),
                    "Cancel update",
                    Kind::Quiet,
                    !busy,
                    cx,
                    move |this, _, cx| {
                        let op = this.model.cancel_offer(&d);
                        this.run(op, cx);
                    },
                )
            });
            let when = if open {
                String::new()
            } else {
                format!(" · {}", nz_date(offer.ended_at.unwrap_or(offer.updated_at)))
            };
            out = out.child(
                div()
                    .id(ElementId::Name(format!("offer-{display}").into()))
                    .role(Role::Status)
                    .aria_label(SharedString::from(format!("{title}. {detail}")))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .p(px(12.0))
                    .border_1()
                    .border_color(c(if phase.is_problem() { BEAK } else { LINE }))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .child(ui::tag(if open { "Updating" } else { "Last update" }, MUTED))
                            .child(div().flex_1().min_w_0().child(ui::h3(format!("{title}{when}")))),
                    )
                    .when(!detail.is_empty(), |d| d.child(ui::p_muted(detail)))
                    .children(cancel.map(|c| div().flex().child(c))),
            );
        }
        if !open {
            if let Some(a) = &fw.available {
                let (d, p) = (display.to_string(), a.package_id.clone());
                let update = self.button(
                    format!("update-{display}"),
                    &format!("Update to {}", keep_case(&a.version)),
                    Kind::Primary,
                    !busy,
                    cx,
                    move |this, window, cx| {
                        this.model.ask_update(&d, &p, Instant::now());
                        let fh = this.fh("confirm-no", cx);
                        window.focus(&fh, cx);
                    },
                );
                out = out.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(8.0))
                                .child(ui::tag("Update available", BEAK))
                                .child(ui::label(&format!(
                                    "{}, signed by {}",
                                    keep_case(&a.version),
                                    signer_text(&a.signer)
                                ))),
                        )
                        .when_some(a.summary.clone(), |d, s| d.child(maker_note(s)))
                        .child(div().flex().child(update)),
                );
            }
            if !fw.others.is_empty() {
                let mut row = div().flex().flex_wrap().gap(px(8.0));
                for o in fw.others.clone() {
                    let (d, p) = (display.to_string(), o.package_id.clone());
                    row = row.child(self.button(
                        format!("other-{display}-{}", o.package_id),
                        &format!("Install {}", keep_case(&o.version)),
                        Kind::Quiet,
                        !busy,
                        cx,
                        move |this, window, cx| {
                            this.model.ask_update(&d, &p, Instant::now());
                            let fh = this.fh("confirm-no", cx);
                            window.focus(&fh, cx);
                        },
                    ));
                }
                out = out.child(ui::label("Other versions on the hub")).child(row);
            }
        }
        Some(out)
    }

    /// In Add a display: the keys waiting from an install, and a way to leave them out.
    pub(super) fn pending_keys_block(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let p = self.model.pending_keys()?.clone();
        let busy = self.model.busy.is_some();
        let keys: Vec<String> = p.keys.iter().map(signer_text).collect();
        let (text, button, include) = if p.include {
            (
                format!(
                    "This display will trust keys from {} from the firmware just installed on {}. They are sent \
                     only to {}.",
                    keys.join("; "),
                    p.port,
                    p.port
                ),
                "Leave them out",
                false,
            )
        } else {
            (
                format!(
                    "The keys from the firmware just installed on {} are left out, so the display keeps whatever \
                     keys it already has.",
                    p.port
                ),
                "Include them",
                true,
            )
        };
        let toggle = self.button(
            "pending-keys",
            button,
            Kind::Quiet,
            !busy,
            cx,
            move |this, _, _| this.model.include_pending_keys(include),
        );
        Some(
            div().child(
                div()
                    .id("pending-keys-box")
                    .role(Role::Group)
                    .aria_label(SharedString::from(text.clone()))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .p(px(12.0))
                    .border_1()
                    .border_color(c(BEAK))
                    .child(ui::p(text))
                    .child(div().flex().child(toggle)),
            ),
        )
    }
}
