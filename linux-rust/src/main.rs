mod bluetooth;
mod devices;
#[cfg(target_os = "freebsd")]
mod freebsd_audio;
mod media_controller;
mod ui;
mod utils;

#[cfg(target_os = "linux")]
use crate::bluetooth::le::start_le_monitor;
use crate::bluetooth::managers::DeviceManagers;
use crate::bluetooth::platform::{self, DeviceEvent, DeviceKind};
use crate::devices::enums::DeviceData;
use crate::ui::messages::BluetoothUIMessage;
use crate::ui::tray::MyTray;
use crate::utils::{get_app_settings_path, get_devices_path};
use clap::Parser;
use devices::airpods::AirPodsDevice;
use ksni::TrayMethods;
use log::{info, warn};
use std::collections::HashMap;
use std::env;
use std::sync::atomic::{AtomicBool};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::mpsc::unbounded_channel;

#[derive(Parser)]
struct Args {
    #[arg(long, short = 'd', help = "Enable debug logging")]
    debug: bool,
    #[arg(
        long,
        help = "Disable system tray, useful if your environment doesn't support AppIndicator or StatusNotifier"
    )]
    no_tray: bool,
    #[arg(long, help = "Start the application minimized to tray")]
    start_minimized: bool,
    #[arg(
        long,
        help = "Enable Bluetooth LE debug logging. Only use when absolutely necessary; this produces a lot of logs."
    )]
    le_debug: bool,
    #[arg(long, short = 'v', help = "Show application version and exit")]
    version: bool
}

fn main() -> iced::Result {
    let args = Args::parse();

    if args.version {
        println!(
            "You are running LibrePods version {}",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }

    let log_level = if args.debug { "debug" } else { "info" };
    // let wayland_display = env::var("WAYLAND_DISPLAY").is_ok();
    // if wayland_display && env::var("WGPU_BACKEND").is_err() {
    //     unsafe { env::set_var("WGPU_BACKEND", "gl") };
    // }
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var(
                "RUST_LOG",
                log_level.to_owned()
                    + &format!(
                        ",zbus=warn,winit=warn,tracing=warn,iced_wgpu=warn,wgpu_hal=warn,wgpu_core=warn,cosmic_text=warn,naga=warn,iced_winit=warn,librepods::bluetooth::le={}",
                        if args.le_debug { "debug" } else { "info" }
                    ),
            )
        };
    }
    env_logger::init();

    let (ui_tx, ui_rx) = unbounded_channel::<BluetoothUIMessage>();

    let device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Load stem_control initial value from settings JSON, then apply CLI override.
    if args.no_tray {
        // Run headless without UI
        info!("Running in headless mode (no GUI)");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async_main(ui_tx, device_managers)).unwrap();
        Ok(())
    } else {
        // Run with UI
        let device_managers_clone = device_managers.clone();
        std::thread::spawn(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async_main(ui_tx, device_managers_clone))
                .unwrap();
        });

        ui::window::start_ui(ui_rx, args.start_minimized, device_managers)
    }
}

async fn async_main(
    ui_tx: tokio::sync::mpsc::UnboundedSender<BluetoothUIMessage>,
    device_managers: Arc<RwLock<HashMap<String, DeviceManagers>>>,
) -> std::io::Result<()> {
    let args = Args::parse();

    let mut managed_devices_mac: Vec<String> = Vec::new(); // includes ony non-AirPods. AirPods handled separately.

    let devices_path = get_devices_path();
    let devices_json = std::fs::read_to_string(&devices_path).unwrap_or_else(|e| {
        log::error!("Failed to read devices file: {}", e);
        "{}".to_string()
    });
    let devices_list: HashMap<String, DeviceData> = serde_json::from_str(&devices_json)
        .unwrap_or_else(|e| {
            log::error!("Deserialization failed: {}", e);
            HashMap::new()
        });
    for (mac, device_data) in devices_list.iter() {
        if device_data.type_ == devices::enums::DeviceType::Nothing {
            managed_devices_mac.push(mac.clone());
        }
    }

    let tray_handle = if args.no_tray {
        None
    } else {
        let tray = MyTray {
            conversation_detect_enabled: None,
            battery_headphone: None,
            battery_headphone_status: None,
            battery_l: None,
            battery_l_status: None,
            battery_r: None,
            battery_r_status: None,
            battery_c: None,
            battery_c_status: None,
            connected: false,
            listening_mode: None,
            allow_off_option: None,
            command_tx: None,
            ui_tx: Some(ui_tx.clone()),
        };
        let handle = tray.spawn().await.unwrap();
        Some(handle)
    };

    #[cfg(target_os = "linux")]
    {
        let le_tray_clone = tray_handle.clone();
        tokio::spawn(async move {
            info!("Starting LE monitor...");
            if let Err(e) = start_le_monitor(le_tray_clone).await {
                log::error!("LE monitor error: {}", e);
            }
        });
    }

    // FreeBSD: per-device audio routing (virtual_oss + PulseAudio + AVRCP).
    #[cfg(target_os = "freebsd")]
    let audio_sessions: Arc<tokio::sync::Mutex<HashMap<String, freebsd_audio::AudioSession>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let (event_tx, mut event_rx) = unbounded_channel::<DeviceEvent>();
    let watcher = tokio::spawn(platform::watch_devices(event_tx));
    info!("Listening for new connections.");

    while let Some(event) = event_rx.recv().await {
        match event {
            DeviceEvent::Disconnected { address } => {
                #[cfg(target_os = "freebsd")]
                {
                    let sessions = audio_sessions.clone();
                    let key = address.to_string();
                    tokio::spawn(async move {
                        if let Some(s) = sessions.lock().await.remove(&key) {
                            s.stop().await;
                        }
                    });
                }
                if let Err(e) = ui_tx.send(BluetoothUIMessage::DeviceDisconnected(address.to_string())) {
                    warn!("Failed to send DeviceDisconnected UI message: {:?}", e);
                }
            }
            DeviceEvent::Connected { address, name, kind } => {
                let addr_str = address.to_string();
                let ui_tx = ui_tx.clone();
                let device_managers = device_managers.clone();

                if managed_devices_mac.contains(&addr_str) {
                    info!("Managed device connected: {}, initializing", addr_str);
                    let type_ = devices_list.get(&addr_str).unwrap().type_.clone();
                    if type_ == devices::enums::DeviceType::Nothing {
                        tokio::spawn(async move {
                            let dev = devices::nothing::NothingDevice::new(address, ui_tx.clone()).await;
                            let mut managers = device_managers.write().await;
                            let dev_managers = DeviceManagers::with_att(dev.att_manager.clone());
                            managers
                                .entry(addr_str.clone())
                                .or_insert(dev_managers)
                                .set_att(dev.att_manager);
                            drop(managers);
                            if let Err(e) = ui_tx.send(BluetoothUIMessage::DeviceConnected(addr_str)) {
                                warn!("Failed to send DeviceConnected UI message: {:?}", e);
                            }
                        });
                    }
                    continue;
                }

                if kind != DeviceKind::AirPods {
                    continue;
                }
                info!(
                    "AirPods connected: {} ({}), initializing",
                    name.as_deref().unwrap_or("Unknown"),
                    addr_str
                );
                #[cfg(target_os = "freebsd")]
                {
                    let sessions = audio_sessions.clone();
                    let key = addr_str.clone();
                    tokio::spawn(async move {
                        let mut sessions = sessions.lock().await;
                        if let Some(old) = sessions.remove(&key) {
                            old.stop().await;
                        }
                        if let Some(s) = freebsd_audio::AudioSession::start(address).await {
                            sessions.insert(key, s);
                        }
                    });
                }
                let tray_handle = tray_handle.clone();
                tokio::spawn(async move {
                    let airpods_device = AirPodsDevice::new(address, tray_handle, ui_tx.clone()).await;
                    let mut managers = device_managers.write().await;
                    // let dev_managers = DeviceManagers::with_both(airpods_device.aacp_manager.clone(), airpods_device.att_manager.clone());
                    let dev_managers = DeviceManagers::with_aacp(airpods_device.aacp_manager.clone());
                    managers
                        .entry(addr_str.clone())
                        .or_insert(dev_managers)
                        .set_aacp(airpods_device.aacp_manager);
                    drop(managers);
                    if let Err(e) = ui_tx.send(BluetoothUIMessage::DeviceConnected(addr_str)) {
                        warn!("Failed to send DeviceConnected UI message: {:?}", e);
                    }
                });
            }
        }
    }

    // The watcher only drops its sender when it stops; surface why.
    watcher.await.map_err(std::io::Error::other)?
}
