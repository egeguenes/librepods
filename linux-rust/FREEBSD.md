# LibrePods on FreeBSD

The Rust app (`linux-rust/`) builds and runs natively on FreeBSD (tested on
FreeBSD 15). It talks to the kernel's netgraph Bluetooth stack directly, so
BlueZ is not needed. The GUI, tray icon and MPRIS media control are the same as
on Linux.

## How it works

| Concern                 | Linux                          | FreeBSD                                                     |
|-------------------------|--------------------------------|-------------------------------------------------------------|
| AACP / ATT channels     | BlueZ L2CAP sockets (BlueR)    | `PF_BLUETOOTH` / `BLUETOOTH_PROTO_L2CAP` seqpacket sockets  |
| Connection events       | BlueZ `Device1` D-Bus signals  | Polling each HCI node's ACL connection list (every 2 s)     |
| Detecting AirPods       | SDP UUID `74ec2172-…`          | Probing the AACP channel (PSM `0x1001`) on new ACL links    |
| Adapter address         | BlueZ adapter                  | `bt_devenum(3)` from `libbluetooth`                         |
| Pairing                 | BlueZ agent                    | External, see below                                         |
| Audio (A2DP)            | PipeWire / PulseAudio          | `virtual_oss(8)`                                            |

The platform-specific code is in `src/bluetooth/l2cap.rs` and
`src/bluetooth/platform/`. Nothing else in the app depends on the operating
system.

## Requirements

```sh
pkg install rust pkgconf dbus pulseaudio
```

You need `dbus` for the tray icon (StatusNotifierItem) and MPRIS. `libpulse`
is still linked, but PulseAudio doesn't have to be running.

Load the Bluetooth stack. To make this persistent, add `ng_ubt_load="YES"` and
`ng_btsocket_load="YES"` to `/boot/loader.conf`.

```sh
kldload ng_ubt ng_btsocket
service bluetooth start ubt0
```

LibrePods doesn't need root. Reading the HCI connection list and opening L2CAP
channels both work as a normal user.

## Pairing and connecting

LibrePods doesn't pair. It attaches to AirPods once they have an ACL link to
your adapter.

* **Pairing:** AirPods use Secure Simple Pairing. Use a pairing daemon that
  supports it, such as [blued](https://github.com/niccokunzmann/blued), which
  was used in
  [#528](https://github.com/librepods-org/librepods/issues/528). Legacy
  `hcsecd(8)` link keys work too if you already have them.
* **Audio:** route audio with `virtual_oss(8)`'s Bluetooth backend, which
  is in the `virtual_oss_bluetooth` package. `virtual_oss` only accepts
  device names up to 26 characters, and `/dev/bluetooth/XX:XX:XX:XX:XX:XX`
  is longer than that. Add a short alias to `/etc/bluetooth/hosts` first
  (for example `XX:XX:XX:XX:XX:XX airpods`), then run:

  ```sh
  pkg install virtual_oss_bluetooth
  virtual_oss -C 2 -c 2 -r 48000 -b 16 -s 4ms \
      -R /dev/null -P /dev/bluetooth/airpods -d dsp9
  ```

  This brings up the ACL link. LibrePods notices it within about 2 seconds,
  opens the AACP channel and takes over from there.

* **PulseAudio:** add the device as an output. PulseAudio's OSS module
  thinks it has hardware volume control, but `virtual_oss` ignores those
  volume calls, so the AirPods play at full volume no matter where the
  volume slider is. Put a remap sink on top so PulseAudio does the volume
  in software:

  ```sh
  # Remove any earlier setup first, so this is safe to run more than once.
  pactl list short modules | awk '/remap-sink|dsp9/ {print $1}' | xargs -n1 pactl unload-module 2>/dev/null
  pactl load-module module-oss device=/dev/dsp9 sink_name=airpods record=0 \
      'sink_properties=device.description="AirPods-raw-DO-NOT-USE"'
  pactl load-module module-remap-sink sink_name=airpods_vol master=airpods \
      'sink_properties="device.description=AirPods device.class=sound"'
  pactl set-default-sink airpods_vol
  pactl set-sink-volume airpods_vol 15%
  ```

  `device.class=sound` matters on KDE Plasma: by default its volume applet
  hides "filter" sinks such as remap sinks. Choose **AirPods** in the applet,
  not the raw output.

  The name `dsp9` keeps `virtual_oss` from taking over `/dev/dsp`, so the
  built-in sound card keeps working.

## Build and run

```sh
cd linux-rust
cargo build --release
./target/release/librepods            # GUI + tray
./target/release/librepods --no-tray  # headless
```

Use `-d` for debug logs. The `just` AppImage targets are Linux-only. Use plain
`cargo build` on FreeBSD.

## What works and what doesn't yet

Works, since it's all AACP:

* Noise control: off / ANC / transparency / adaptive
* Conversational awareness toggle
* Battery
* Ear detection, with play/pause via MPRIS
* Stem press controls
* Renaming
* All other control commands

Not yet supported on FreeBSD:

* **BLE monitoring.** The BLE advertisement monitor (battery while
  disconnected, proximity-based auto-connect) uses BlueZ's
  `AdvertisementMonitor` API.
* **A2DP profile switching.** On ear removal/insertion, Linux switches the
  PipeWire/PulseAudio card profile. With `virtual_oss` there's no card profile,
  so this step is skipped. Pausing and resuming media still works.
* **Volume ducking.** Conversational awareness lowers volume on Linux by
  finding the BlueZ PulseAudio sink for the device. There is no such sink on
  FreeBSD, so the volume stays as it is.
