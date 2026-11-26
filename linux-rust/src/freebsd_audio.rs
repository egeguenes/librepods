//! FreeBSD audio routing for AirPods.
//!
//! `librepods-btagent` (root) runs `virtual_oss(8)` while the AirPods' encrypted
//! link is up, which creates `/dev/dsp_<alias>` (A2DP/SBC). When AirPods
//! connect, LibrePods:
//! 1. opens AVRCP and sets the absolute volume (AirPods are inaudible without it),
//! 2. waits for that OSS device and adds it to PulseAudio as an "AirPods" sink,
//!    makes it the default and moves playing streams to it.
//!
//! When they disconnect, the sink is removed (the agent stops virtual_oss).
//! The AirPods need a short alias in `/etc/bluetooth/hosts`; see FREEBSD.md.

use crate::bluetooth::Address;
use crate::bluetooth::avrcp;
use log::{debug, error, info, warn};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;

/// How long to wait for the agent's virtual_oss to create the OSS device.
const DEVICE_WAIT: Duration = Duration::from_secs(20);
const SINK_NAME: &str = "librepods_airpods";
/// The volume slider in the desktop controls PulseAudio's (digital) volume;
/// the headphones themselves are set to full scale so that 100% in the panel
/// is the AirPods' maximum, like on an iPhone. Stem swipes still work.
const AVRCP_VOLUME: u8 = 127;
/// PulseAudio volume for a freshly created AirPods sink, so it doesn't start
/// at 100% (= AirPods maximum).
const INITIAL_SINK_VOLUME: &str = "35%";

pub struct AudioSession {
    address: Address,
    pulse_module: Option<u32>,
    avrcp: Option<JoinHandle<()>>,
}

/// First name for `address` in /etc/bluetooth/hosts, if it fits virtual_oss.
fn hosts_alias(address: Address) -> Option<String> {
    let text = std::fs::read_to_string("/etc/bluetooth/hosts").ok()?;
    let want = address.to_string().to_lowercase();
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let a = it.next()?;
            let name = it.next()?;
            (a.to_lowercase() == want).then(|| name.to_string())
        })
        .find(|name| "/dev/bluetooth/".len() + name.len() <= 26)
}

async fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().await.ok()?;
    if !out.status.success() {
        warn!(
            "pactl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Remove sinks left behind by a previous run (e.g. after a crash).
async fn unload_stale_modules() {
    if let Some(list) = pactl(&["list", "short", "modules"]).await {
        for line in list.lines().filter(|l| l.contains(SINK_NAME)) {
            if let Some(id) = line.split_whitespace().next() {
                let _ = pactl(&["unload-module", id]).await;
            }
        }
    }
}

fn first_time_marker() -> std::path::PathBuf {
    crate::utils::get_devices_path().with_file_name("freebsd_sink_initialized")
}

impl AudioSession {
    pub async fn start(address: Address) -> Option<Self> {
        let Some(alias) = hosts_alias(address) else {
            error!(
                "No short alias for {} in /etc/bluetooth/hosts; audio disabled. \
                 Add a line like '{} airpods' (see FREEBSD.md).",
                address,
                address.to_string().to_lowercase()
            );
            return None;
        };
        let mut session = AudioSession { address, pulse_module: None, avrcp: None };

        // AVRCP first: virtual_oss connects A2DP on its own and audio should be
        // audible as soon as it does.
        session.avrcp = Some(tokio::spawn(async move {
            for attempt in 1..=3 {
                match avrcp::run(address, AVRCP_VOLUME).await {
                    Ok(()) => {
                        info!("AVRCP channel to {} closed", address);
                        return;
                    }
                    Err(e) => warn!("AVRCP to {} failed (attempt {}): {}", address, attempt, e),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }));

        let dev_path = format!("/dev/dsp_{}", alias);
        let deadline = tokio::time::Instant::now() + DEVICE_WAIT;
        while !Path::new(&dev_path).exists() {
            if tokio::time::Instant::now() >= deadline {
                error!(
                    "{} did not appear; is librepods-btagent running? \
                     (check /var/log/librepods-btagent.log)",
                    dev_path
                );
                // Keep AVRCP running; the sink just won't be added.
                return Some(session);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        info!("Audio device {} is up", dev_path);

        // PulseAudio sink.
        unload_stale_modules().await;
        let device_arg = format!("device={}", dev_path);
        let sink_arg = format!("sink_name={}", SINK_NAME);
        let module = pactl(&[
            "load-module",
            "module-oss",
            &device_arg,
            &sink_arg,
            "record=0",
            // Quoted: PulseAudio splits module arguments on spaces.
            "sink_properties='device.description=AirPods device.icon_name=audio-headphones-bluetooth device.form_factor=headphones'",
        ])
        .await
        .and_then(|s| s.trim().parse::<u32>().ok());
        match module {
            Some(id) => {
                session.pulse_module = Some(id);
                let marker = first_time_marker();
                if !marker.exists() {
                    let _ = pactl(&["set-sink-volume", SINK_NAME, INITIAL_SINK_VOLUME]).await;
                    let _ = std::fs::write(&marker, b"");
                }
                let _ = pactl(&["set-default-sink", SINK_NAME]).await;
                if let Some(inputs) = pactl(&["list", "short", "sink-inputs"]).await {
                    for id in inputs.lines().filter_map(|l| l.split_whitespace().next()) {
                        let _ = pactl(&["move-sink-input", id, SINK_NAME]).await;
                    }
                }
                info!("PulseAudio sink '{}' ready and set as default", SINK_NAME);
            }
            None => warn!(
                "Could not add the AirPods to PulseAudio; play to {} directly instead",
                dev_path
            ),
        }
        Some(session)
    }

    /// Undo everything. PulseAudio moves streams back to another sink by itself.
    pub async fn stop(mut self) {
        debug!("Stopping audio session for {}", self.address);
        if let Some(id) = self.pulse_module.take() {
            let _ = pactl(&["unload-module", &id.to_string()]).await;
        }
        if let Some(task) = self.avrcp.take() {
            task.abort();
        }
        info!("Audio for {} stopped", self.address);
    }
}
