//! The display's end of USB provisioning (`docs/protocol-v1.md`, "Over USB"), for testing the
//! manager's sender without hardware. `display-simulator usb` serves it on a pseudo-terminal.

use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use hub_protocol::provisioning::Bundle;
use hub_protocol::usb::{encode, FromDisplay, Line, LineReader, ToDisplay, UsbErrorCode};

use crate::paired;

/// Where a received bundle waits until the display has enrolled with it.
pub fn bundle_file(dir: &Path) -> PathBuf {
    dir.join("received-bundle.json")
}

/// Whether the display holds a bundle or a device token. Either way it refuses another bundle until
/// it is factory-reset.
pub fn is_provisioned(dir: &Path) -> bool {
    paired::load(dir).is_ok() || bundle_file(dir).exists()
}

pub fn load_bundle(dir: &Path) -> Option<Bundle> {
    serde_json::from_slice(&std::fs::read(bundle_file(dir)).ok()?).ok()
}

fn store_bundle(dir: &Path, bundle: &Bundle) -> std::io::Result<()> {
    paired::write_private(
        &bundle_file(dir),
        &serde_json::to_vec(bundle).expect("serialises"),
    )
}

fn reply<P: Write>(port: &mut P, msg: &FromDisplay) -> std::io::Result<()> {
    port.write_all(&encode(msg))?;
    port.flush()
}

/// Answers the manager on `port` until it accepts a bundle, which it stores in `dir` and returns.
/// Returns `None` when the port closes first, or has nothing on its other end yet.
pub fn answer<P: Read + Write>(port: &mut P, dir: &Path) -> std::io::Result<Option<Bundle>> {
    let mut lines = LineReader::default();
    let mut buf = [0u8; 1024];
    loop {
        let n = match port.read(&mut buf) {
            Ok(0) => return Ok(None),
            // A pty whose other end nothing has open.
            Err(e) if e.kind() == ErrorKind::BrokenPipe || e.raw_os_error() == Some(5) => return Ok(None),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        for line in lines.push(&buf[..n]) {
            let Line::Json(text) = line else { continue };
            // Anything unreadable is ignored, as a boot log would be.
            let Ok(msg) = serde_json::from_str::<ToDisplay>(&text) else {
                continue;
            };
            match msg {
                ToDisplay::Hello { .. } => reply(
                    port,
                    &FromDisplay::Hello {
                        device: paired::simulator_info(),
                        provisioned: is_provisioned(dir),
                    },
                )?,
                ToDisplay::Provision { bundle } => {
                    let refuse = |code, message: &str| FromDisplay::Error {
                        code,
                        message: message.into(),
                    };
                    let problems = bundle.problems();
                    if is_provisioned(dir) {
                        reply(
                            port,
                            &refuse(
                                UsbErrorCode::AlreadyProvisioned,
                                "already set up; factory-reset this display first",
                            ),
                        )?;
                    } else if !problems.is_empty() {
                        reply(port, &refuse(UsbErrorCode::InvalidBundle, &problems.join("; ")))?;
                    } else if let Err(e) = store_bundle(dir, &bundle) {
                        reply(port, &refuse(UsbErrorCode::StorageFailed, &e.to_string()))?;
                    } else {
                        reply(port, &FromDisplay::Provisioned)?;
                        return Ok(Some(bundle));
                    }
                }
            }
        }
    }
}
