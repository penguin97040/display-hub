//! A paired display: hub details and a device token, kept like a display keeps them in encrypted
//! flash. Here it is a JSON file readable only by its owner, which is fine for a simulator.

use std::io::Write;
use std::path::{Path, PathBuf};

use hub_client::{ClientError, HubClient};
use hub_protocol::device::DeviceInfo;
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

pub fn simulator_info() -> DeviceInfo {
    DeviceInfo {
        device_type: "llm-usage-display".into(),
        firmware_version: "0.0.0-simulator".into(),
        hardware_revision: "simulated".into(),
        protocol_version: hub_protocol::PROTOCOL_VERSION,
    }
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
    std::fs::create_dir_all(dir)?;
    let path = state_file(dir);
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(state).expect("serialises"))?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Erases everything the display holds, as a factory reset must.
pub fn reset(dir: &Path) -> std::io::Result<()> {
    for f in [state_file(dir), cache_file(dir)] {
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
    let secret = bundle.enrolment.secret.clone();
    let resp = first_host(&bundle.hub, |c| {
        let secret = secret.clone();
        async move { c.enrol(&secret, simulator_info()).await }
    })
    .await
    .map_err(|e| e.to_string())?;
    let state = State {
        hub: bundle.hub.clone(),
        device_id: resp.device_id,
        device_token: resp.device_token,
    };
    save(dir, &state).map_err(|e| format!("couldn't save the display's state: {e}"))?;
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

pub async fn heartbeat(state: &State) -> Result<(), ClientError> {
    let token = state.device_token.clone();
    first_host(&state.hub, |c| {
        let token = token.clone();
        async move { c.heartbeat(&token, &simulator_info()).await }
    })
    .await
}
