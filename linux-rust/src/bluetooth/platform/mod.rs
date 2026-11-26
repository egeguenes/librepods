//! OS-specific Bluetooth glue: adapter information and device connection
//! events. Everything above this layer (AACP/ATT managers, UI, tray) is
//! platform-neutral.
//!
//! * `linux`: BlueZ over D-Bus (via BlueR and the `dbus` crate).
//! * `freebsd`: the netgraph Bluetooth stack (`ng_hci`/`ng_l2cap`) via
//!   `libbluetooth` and raw HCI socket ioctls. There is no daemon emitting
//!   connection events, so the ACL connection list is polled.

use crate::bluetooth::Address;

#[cfg(target_os = "freebsd")]
mod freebsd;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "freebsd")]
pub use freebsd::{local_address, watch_devices};
#[cfg(target_os = "linux")]
pub use linux::{local_address, watch_devices};

/// AACP service UUID advertised by AirPods over SDP.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const AIRPODS_SERVICE_UUID: &str = "74ec2172-0bad-4d01-8f77-997b2be0722a";

/// PSM of Apple's Accessory Control Protocol channel.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
pub const AACP_PSM: u16 = 0x1001;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Speaks AACP (AirPods / Beats).
    AirPods,
    /// Anything else; may still be a device listed in `devices.json`.
    Other,
}

#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A device is (or, at start-up, already was) connected.
    Connected {
        address: Address,
        name: Option<String>,
        kind: DeviceKind,
    },
    Disconnected { address: Address },
}
