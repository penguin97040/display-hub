//! A paired display: hub details and a device token, kept like a display keeps them in encrypted
//! flash. Here it is a JSON file readable only by its owner, which is fine for a simulator.

use std::io::Write;
use std::path::{Path, PathBuf};

use hub_client::{ClientError, FirmwareStream, HubClient};
use hub_protocol::device::DeviceInfo;
use hub_protocol::firmware::FirmwareReport;
use hub_protocol::provisioning::{Bundle, HubLocation};
use hub_protocol::Snapshot;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct State {
    pub hub: HubLocation,
    pub device_id: String,
    pub device_token: String,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("hub", &self.hub.hosts)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// What the display in `dir` says about itself: its running firmware's version, and its trusted
/// keys, floor and spare slot (`firmware::store::device_info`). A state folder that can't be read
/// reports a factory board.
pub fn device_info(dir: &Path) -> DeviceInfo {
    crate::firmware::store::device_info(dir).unwrap_or_else(|e| {
        eprintln!("display-simulator: couldn't read the firmware state: {e}");
        let board = crate::firmware::store::Board::default();
        DeviceInfo {
            device_type: crate::firmware::store::DEVICE_TYPE.into(),
            firmware_version: board.factory_version,
            hardware_revision: crate::firmware::store::HARDWARE_REVISION.into(),
            protocol_version: hub_protocol::PROTOCOL_VERSION,
            firmware: None,
        }
    })
}

fn state_file(dir: &Path) -> PathBuf {
    dir.join("device.json")
}

pub fn cache_file(dir: &Path) -> PathBuf {
    dir.join("last-snapshot.json")
}

pub fn load(dir: &Path) -> Result<State, String> {
    let bytes = std::fs::read(state_file(dir))
        .map_err(|e| format!("this simulated display isn't set up ({e}); run `enrol` first"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("the saved display state is damaged: {e}"))
}

fn save(dir: &Path, state: &State) -> std::io::Result<()> {
    write_private(
        &state_file(dir),
        &serde_json::to_vec_pretty(state).expect("serialises"),
    )
}

/// Writes a file only its owner can read, atomically, creating its folder if needed.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

/// A factory reset: erases the hub details, token, saved pages and any received bundle. The firmware
/// state beside them (trusted keys, floor, app slots and update record) is kept: those belong to the
/// firmware's maker, not to the hub the display was paired with.
pub fn reset(dir: &Path) -> std::io::Result<()> {
    for f in [state_file(dir), cache_file(dir), crate::usb::bundle_file(dir)] {
        match std::fs::remove_file(f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Tries each host in order. Moves on after a connection failure, or a key mismatch at one address
/// (a spoofed name), but stops at the first answer from the real hub.
async fn first_host<T, F, Fut>(hub: &HubLocation, mut call: F) -> Result<T, ClientError>
where
    F: FnMut(HubClient) -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    let pins = hub_client::provisioning::pins(hub);
    let mut last = ClientError::Unreachable("no hub addresses".into());
    for host in &hub.hosts {
        let client = HubClient::pinned(host, hub.port, pins.clone())?;
        match call(client).await {
            Err(e) if e.is_connection() => last = e,
            other => return other,
        }
    }
    Err(last)
}

/// Enrols with the hub named in `bundle` and saves the result in `dir`.
pub async fn enrol(bundle: &Bundle, dir: &Path) -> Result<State, String> {
    let problems = bundle.problems();
    if !problems.is_empty() {
        return Err(format!(
            "the provisioning bundle isn't valid: {}",
            problems.join("; ")
        ));
    }
    match crate::firmware::store::accept_bundle(dir, bundle.firmware.as_ref()) {
        Ok(Ok(())) => {}
        Ok(Err(locked)) => return Err(locked.to_string()),
        Err(e) => return Err(format!("couldn't save the firmware keys: {e}")),
    }
    let secret = bundle.enrolment.secret.clone();
    let info = device_info(dir);
    let resp = first_host(&bundle.hub, |c| {
        let (secret, info) = (secret.clone(), info.clone());
        async move { c.enrol(&secret, info).await }
    })
    .await
    .map_err(|e| e.to_string())?;
    let state = State {
        hub: bundle.hub.clone(),
        device_id: resp.device_id,
        device_token: resp.device_token,
    };
    save(dir, &state).map_err(|e| format!("couldn't save the display's state: {e}"))?;
    // The secret is used up. A display keeps the hub details and its token, never the bundle.
    let _ = std::fs::remove_file(crate::usb::bundle_file(dir));
    Ok(state)
}

pub async fn snapshot(state: &State) -> Result<Snapshot, ClientError> {
    let token = state.device_token.clone();
    first_host(&state.hub, |c| {
        let token = token.clone();
        async move { c.device_snapshot(Some(&token)).await }
    })
    .await
}

/// A heartbeat saying `info`, from [`device_info`].
pub async fn heartbeat(state: &State, info: &DeviceInfo) -> Result<(), ClientError> {
    let token = state.device_token.clone();
    first_host(&state.hub, |c| {
        let token = token.clone();
        async move { c.heartbeat(&token, info).await }
    })
    .await
}

/// Starts reading a firmware download.
pub async fn firmware_stream(state: &State, package_id: &str) -> Result<FirmwareStream, ClientError> {
    let token = state.device_token.clone();
    first_host(&state.hub, |c| {
        let token = token.clone();
        async move { c.firmware_stream(&token, package_id).await }
    })
    .await
}

pub async fn firmware_report(state: &State, report: &FirmwareReport) -> Result<(), ClientError> {
    let token = state.device_token.clone();
    first_host(&state.hub, |c| {
        let token = token.clone();
        async move { c.firmware_report(&token, report).await }
    })
    .await
}
