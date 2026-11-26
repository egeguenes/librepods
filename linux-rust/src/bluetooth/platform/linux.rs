//! BlueZ backend.

use super::{AIRPODS_SERVICE_UUID, DeviceEvent, DeviceKind};
use crate::bluetooth::Address;
use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::message::MatchRule;
use log::{debug, info};
use std::collections::HashMap;
use std::io;
use tokio::sync::mpsc::UnboundedSender;

fn to_io(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(e)
}

pub async fn local_address() -> io::Result<Address> {
    let session = bluer::Session::new().await.map_err(to_io)?;
    let adapter = session.default_adapter().await.map_err(to_io)?;
    Ok(adapter.address().await.map_err(to_io)?.into())
}

/// Report already-connected devices, then forward BlueZ `Device1.Connected`
/// changes as `DeviceEvent`s until the D-Bus connection fails.
pub async fn watch_devices(tx: UnboundedSender<DeviceEvent>) -> io::Result<()> {
    let session = bluer::Session::new().await.map_err(to_io)?;
    let adapter = session.default_adapter().await.map_err(to_io)?;
    adapter.set_powered(true).await.map_err(to_io)?;

    info!("Checking for connected devices...");
    let target_uuid = uuid::Uuid::parse_str(AIRPODS_SERVICE_UUID).unwrap();
    for addr in adapter.device_addresses().await.map_err(to_io)? {
        let device = adapter.device(addr).map_err(to_io)?;
        let connected = device.is_connected().await.unwrap_or(false);
        debug!("Checking device: {}, connected: {}", addr, connected);
        if !connected {
            continue;
        }
        let is_airpods = device
            .uuids()
            .await
            .ok()
            .flatten()
            .is_some_and(|uuids| uuids.contains(&target_uuid));
        let name = device.name().await.ok().flatten();
        let _ = tx.send(DeviceEvent::Connected {
            address: addr.into(),
            name,
            kind: if is_airpods { DeviceKind::AirPods } else { DeviceKind::Other },
        });
    }

    // The dbus crate's blocking API needs its own thread.
    tokio::task::spawn_blocking(move || dbus_loop(tx))
        .await
        .map_err(to_io)?
}

fn dbus_loop(tx: UnboundedSender<DeviceEvent>) -> io::Result<()> {
    let conn = Connection::new_system().map_err(to_io)?;
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
    conn.add_match(rule, move |_: (), conn, msg| {
        let Some(path) = msg.path() else {
            return true;
        };
        if !path.contains("/org/bluez/hci") || !path.contains("/dev_") {
            return true;
        }
        let Ok((iface, changed, _)) =
            msg.read3::<String, HashMap<String, Variant<Box<dyn RefArg>>>, Vec<String>>()
        else {
            return true;
        };
        if iface != "org.bluez.Device1" {
            return true;
        }
        let Some(connected_var) = changed.get("Connected") else {
            return true;
        };
        let Some(is_connected) = connected_var.0.as_ref().as_u64() else {
            return true;
        };
        let proxy = conn.with_proxy("org.bluez", path, std::time::Duration::from_millis(5000));
        let Ok(addr_str) = proxy.get::<String>("org.bluez.Device1", "Address") else {
            return true;
        };
        let Ok(address) = addr_str.parse::<Address>() else {
            return true;
        };
        if is_connected == 0 {
            let _ = tx.send(DeviceEvent::Disconnected { address });
            return true;
        }
        let uuids = proxy
            .get::<Vec<String>>("org.bluez.Device1", "UUIDs")
            .unwrap_or_default();
        let is_airpods = uuids
            .iter()
            .any(|u| u.eq_ignore_ascii_case(AIRPODS_SERVICE_UUID));
        let name = proxy.get::<String>("org.bluez.Device1", "Name").ok();
        let _ = tx.send(DeviceEvent::Connected {
            address,
            name,
            kind: if is_airpods { DeviceKind::AirPods } else { DeviceKind::Other },
        });
        true
    })
    .map_err(to_io)?;

    info!("Listening for Bluetooth connections via D-Bus...");
    loop {
        conn.process(std::time::Duration::from_millis(1000))
            .map_err(to_io)?;
    }
}
