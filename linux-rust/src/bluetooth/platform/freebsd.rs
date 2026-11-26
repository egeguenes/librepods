//! FreeBSD netgraph Bluetooth backend.
//!
//! FreeBSD has no BlueZ/D-Bus equivalent: pairing is handled by hcsecd(8)
//! (or a third-party daemon such as blued) and audio by virtual_oss(8). Nobody
//! broadcasts "device connected" events, so we poll each HCI node's ACL
//! connection list (an unprivileged ioctl) and diff it.
//!
//! SDP queries in libsdp only accept 16-bit UUIDs, so instead of looking up
//! the AirPods service UUID we classify a newly connected device by trying to
//! open the AACP L2CAP channel: only Apple accessories accept PSM 0x1001.

use super::{AACP_PSM, DeviceEvent, DeviceKind};
use crate::bluetooth::Address;
use crate::bluetooth::l2cap::L2capChannel;
use libc::{c_char, c_int, c_ulong, c_void};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_ATTEMPTS: u32 = 2;
/// How long to wait for a new ACL link to become encrypted before talking to
/// the device anyway. Unlike BlueZ, netgraph lists an ACL connection as soon as
/// it is opened, before authentication/encryption; AirPods ignore AACP setup
/// requests sent in that window.
const ENCRYPTION_GRACE: Duration = Duration::from_secs(10);

// ---- libbluetooth / netgraph ABI (FreeBSD 13+, checked against 15.x) -------

/// `HCI_DEVNAME_SIZE` from `<bluetooth.h>`.
const HCI_DEVNAME_SIZE: usize = 32;

/// `struct bt_devinfo` from `<bluetooth.h>` (128 bytes, bdaddr at offset 36).
/// Only the leading fields are used; the rest is kept opaque.
#[repr(C)]
struct BtDevinfo {
    devname: [c_char; HCI_DEVNAME_SIZE],
    state: u32,
    bdaddr: [u8; 6],
    _reserved0: u16,
    _rest: [u8; 84],
}
const _: () = assert!(std::mem::size_of::<BtDevinfo>() == 128);

/// `ng_hci_node_con_ep` from `<netgraph/bluetooth/include/ng_hci.h>`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct HciConEp {
    link_type: u8,
    encryption_mode: u8,
    mode: u8,
    role: u8,
    state: u16,
    reserved: u16,
    pending: u16,
    queue_len: u16,
    con_handle: u16,
    bdaddr: [u8; 6],
}
const _: () = assert!(std::mem::size_of::<HciConEp>() == 20);

/// `struct ng_btsocket_hci_raw_con_list`.
#[repr(C)]
struct HciConList {
    num_connections: u32,
    connections: *mut HciConEp,
}

/// `SIOC_HCI_RAW_NODE_GET_CON_LIST` =
/// `_IOWR('b', NGM_HCI_NODE_GET_CON_LIST, struct ng_btsocket_hci_raw_con_list)`.
const SIOC_HCI_RAW_NODE_GET_CON_LIST: c_ulong = 0xc010_626f;
const NG_HCI_LINK_ACL: u8 = 0x01;
const NG_HCI_CON_OPEN: u16 = 3;
const MAX_CONNECTIONS: usize = 64;

type BtDevenumCb = extern "C" fn(c_int, *const BtDevinfo, *mut c_void) -> c_int;

#[link(name = "bluetooth")]
unsafe extern "C" {
    fn bt_devenum(cb: BtDevenumCb, arg: *mut c_void) -> c_int;
}

struct HciNode {
    name: String,
    address: Address,
    /// Peers with an open ACL link on this node, and whether it is encrypted.
    peers: Vec<(Address, bool)>,
}

extern "C" fn devenum_cb(s: c_int, di: *const BtDevinfo, arg: *mut c_void) -> c_int {
    // SAFETY: `arg` is the `&mut Vec<HciNode>` passed to `bt_devenum` below and
    // `di` is valid for the duration of the callback.
    let nodes = unsafe { &mut *(arg as *mut Vec<HciNode>) };
    let di = unsafe { &*di };

    let name = unsafe { std::ffi::CStr::from_ptr(di.devname.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    let mut buf = [HciConEp::default(); MAX_CONNECTIONS];
    let mut list = HciConList {
        num_connections: MAX_CONNECTIONS as u32,
        connections: buf.as_mut_ptr(),
    };
    // `s` is a raw HCI socket already bound and connected to this node.
    let peers = if unsafe { libc::ioctl(s, SIOC_HCI_RAW_NODE_GET_CON_LIST, &mut list) } < 0 {
        debug!(
            "GET_CON_LIST on {} failed: {}",
            name,
            io::Error::last_os_error()
        );
        Vec::new()
    } else {
        let n = (list.num_connections as usize).min(MAX_CONNECTIONS);
        buf[..n]
            .iter()
            .filter(|c| c.link_type == NG_HCI_LINK_ACL && c.state == NG_HCI_CON_OPEN)
            .map(|c| (Address::from_le_bytes(c.bdaddr), c.encryption_mode != 0))
            .collect()
    };

    nodes.push(HciNode {
        name,
        address: Address::from_le_bytes(di.bdaddr),
        peers,
    });
    0 // keep enumerating
}

/// Enumerate HCI nodes and their open ACL connections. Blocking.
fn hci_nodes() -> io::Result<Vec<HciNode>> {
    let mut nodes: Vec<HciNode> = Vec::new();
    let ret = unsafe { bt_devenum(devenum_cb, &mut nodes as *mut _ as *mut c_void) };
    if ret < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EAFNOSUPPORT) | Some(libc::EPROTONOSUPPORT) => io::Error::new(
                e.kind(),
                "FreeBSD Bluetooth stack not available (kldload ng_ubt ng_btsocket \
                 and start the bluetooth service)",
            ),
            _ => e,
        });
    }
    Ok(nodes)
}

async fn hci_nodes_async() -> io::Result<Vec<HciNode>> {
    tokio::task::spawn_blocking(hci_nodes)
        .await
        .map_err(io::Error::other)?
}

pub async fn local_address() -> io::Result<Address> {
    hci_nodes_async()
        .await?
        .into_iter()
        .next()
        .map(|n| n.address)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Bluetooth adapter found"))
}

async fn probe_kind(address: Address) -> DeviceKind {
    for attempt in 1..=PROBE_ATTEMPTS {
        match L2capChannel::connect(address, AACP_PSM, PROBE_TIMEOUT).await {
            Ok(_channel) => return DeviceKind::AirPods, // dropped => closed
            // netgraph maps "PSM not supported" through its HCI error table
            // (to ENOTCONN), so errno can't tell a refusal from a transient
            // failure; just retry a bounded number of times.
            Err(e) => debug!("AACP probe of {} (attempt {}) failed: {}", address, attempt, e),
        }
        if attempt < PROBE_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    DeviceKind::Other
}

/// How often to try reaching known AirPods that are not connected.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(15);

/// AirPods addresses LibrePods has seen before (from devices.json).
fn known_airpods() -> Vec<Address> {
    let Ok(text) = std::fs::read_to_string(crate::utils::get_devices_path()) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, v)| v.get("type_").and_then(|t| t.as_str()) == Some("AirPods"))
        .filter_map(|(k, _)| k.parse().ok())
        .collect()
}

/// PSM of SDP, which needs no security.
const SDP_PSM: u16 = 0x0001;

/// BlueZ reconnects bonded devices by itself; netgraph doesn't, and AirPods
/// don't connect to a computer on their own. Periodically page known AirPods
/// by opening an SDP channel and holding it while librepods-btagent
/// authenticates and encrypts the link; then `watch_devices` sees the
/// encrypted link and opens AACP. Opening AACP directly does not work: AirPods
/// answer "pending, security required" and then drop the link (reason 0x05)
/// when the agent enables encryption in that state.
async fn auto_connect() {
    // Give the watcher a moment to report links that are already up.
    tokio::time::sleep(POLL_INTERVAL * 2).await;
    loop {
        try_known_airpods().await;
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

async fn try_known_airpods() {
    {
        let known = known_airpods();
        if known.is_empty() {
            return;
        }
        let connected: HashSet<Address> = match hci_nodes_async().await {
            Ok(nodes) => nodes.into_iter().flat_map(|n| n.peers).map(|(a, _)| a).collect(),
            Err(_) => return,
        };
        for address in known.into_iter().filter(|a| !connected.contains(a)) {
            debug!("Trying to connect to known AirPods {}", address);
            match L2capChannel::connect(address, SDP_PSM, PROBE_TIMEOUT).await {
                Ok(channel) => {
                    info!("Reconnected to {}", address);
                    // Keep the link up until it is encrypted and the watcher
                    // has opened AACP (which then keeps it up).
                    tokio::time::sleep(POLL_INTERVAL * 3).await;
                    drop(channel);
                }
                Err(e) => debug!("{} not reachable: {}", address, e),
            }
        }
    }
}

pub async fn watch_devices(tx: UnboundedSender<DeviceEvent>) -> io::Result<()> {
    info!("Watching netgraph HCI nodes for ACL connections...");
    tokio::spawn(auto_connect());
    let mut known: HashMap<Address, DeviceKind> = HashMap::new();
    // New links still waiting to become encrypted, with when we first saw them.
    let mut pending: HashMap<Address, Instant> = HashMap::new();
    let mut warned = false;
    let mut announced_nodes: HashSet<String> = HashSet::new();

    loop {
        let current: HashMap<Address, bool> = match hci_nodes_async().await {
            Ok(nodes) => {
                warned = false;
                for n in &nodes {
                    if announced_nodes.insert(n.name.clone()) {
                        info!("Using Bluetooth adapter {} ({})", n.name, n.address);
                    }
                }
                nodes.into_iter().flat_map(|n| n.peers).collect()
            }
            Err(e) => {
                if !warned {
                    warn!("Cannot query Bluetooth adapters: {}", e);
                    warned = true;
                }
                HashMap::new()
            }
        };
        pending.retain(|a, _| current.contains_key(a));

        let gone: Vec<Address> = known
            .keys()
            .filter(|a| !current.contains_key(a))
            .copied()
            .collect();
        for address in gone {
            known.remove(&address);
            debug!("ACL link to {} closed", address);
            let _ = tx.send(DeviceEvent::Disconnected { address });
        }

        for (address, encrypted) in current {
            if known.contains_key(&address) {
                continue;
            }
            let first_seen = *pending.entry(address).or_insert_with(Instant::now);
            if !encrypted {
                if first_seen.elapsed() < ENCRYPTION_GRACE {
                    debug!("ACL link to {} not encrypted yet, waiting", address);
                    continue;
                }
                warn!(
                    "ACL link to {} still unencrypted after {:?}; connecting anyway",
                    address, ENCRYPTION_GRACE
                );
            }
            pending.remove(&address);
            debug!("New ACL link to {}, probing for AACP", address);
            let kind = probe_kind(address).await;
            known.insert(address, kind);
            let _ = tx.send(DeviceEvent::Connected {
                address,
                name: None,
                kind,
            });
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}
