//! Minimal AVRCP controller (FreeBSD only; BlueZ does this on Linux).
//!
//! Apple headphones use AVRCP "absolute volume": the audio source owns the
//! volume and tells the headphones what it is. Without it AirPods play A2DP
//! audio at an inaudible level and stem volume swipes do nothing. This opens
//! the AVCTP channel (PSM 0x17), sets the absolute volume, tracks volume
//! changes made on the headphones, and answers their commands.

use crate::bluetooth::Address;
use crate::bluetooth::l2cap::L2capChannel;
use log::{debug, info, warn};
use std::io;
use std::time::Duration;

const PSM_AVCTP: u16 = 0x0017;
const AVRCP_PID: [u8; 2] = [0x11, 0x0e];
const BT_SIG_COMPANY: [u8; 3] = [0x00, 0x19, 0x58];

const CT_CONTROL: u8 = 0x00;
const CT_STATUS: u8 = 0x01;
const CT_NOTIFY: u8 = 0x03;
const RSP_NOT_IMPLEMENTED: u8 = 0x08;
const RSP_ACCEPTED: u8 = 0x09;
const RSP_STABLE: u8 = 0x0c;
const RSP_CHANGED: u8 = 0x0d;
const RSP_INTERIM: u8 = 0x0f;
const SUBUNIT_PANEL: u8 = 0x48;
const OP_VENDOR_DEPENDENT: u8 = 0x00;
const OP_PASS_THROUGH: u8 = 0x7c;

const PDU_GET_CAPABILITIES: u8 = 0x10;
const PDU_REGISTER_NOTIFICATION: u8 = 0x31;
const PDU_SET_ABSOLUTE_VOLUME: u8 = 0x50;
const EVENT_VOLUME_CHANGED: u8 = 0x0d;

fn vendor(ctype: u8, pdu: u8, params: &[u8]) -> Vec<u8> {
    let mut v = vec![ctype, SUBUNIT_PANEL, OP_VENDOR_DEPENDENT];
    v.extend_from_slice(&BT_SIG_COMPANY);
    v.push(pdu);
    v.push(0x00); // single packet
    v.extend_from_slice(&(params.len() as u16).to_be_bytes());
    v.extend_from_slice(params);
    v
}

struct Avctp {
    ch: L2capChannel,
    label: u8,
}

impl Avctp {
    async fn command(&mut self, avc: &[u8]) -> io::Result<()> {
        let mut pkt = vec![self.label << 4, AVRCP_PID[0], AVRCP_PID[1]];
        self.label = (self.label + 1) & 0x0f;
        pkt.extend_from_slice(avc);
        self.ch.send(&pkt).await.map(|_| ())
    }

    async fn respond(&self, label: u8, avc: &[u8]) -> io::Result<()> {
        let mut pkt = vec![(label << 4) | 0x02, AVRCP_PID[0], AVRCP_PID[1]];
        pkt.extend_from_slice(avc);
        self.ch.send(&pkt).await.map(|_| ())
    }

    async fn register_volume_notification(&mut self) -> io::Result<()> {
        self.command(&vendor(CT_NOTIFY, PDU_REGISTER_NOTIFICATION, &[EVENT_VOLUME_CHANGED, 0, 0, 0, 0]))
            .await
    }
}

/// Connect AVRCP, set the absolute volume (0..=127) and service the channel
/// until it closes.
pub async fn run(address: Address, volume: u8) -> io::Result<()> {
    let ch = L2capChannel::connect(address, PSM_AVCTP, Duration::from_secs(10)).await?;
    let mut avctp = Avctp { ch, label: 0 };
    info!("AVRCP connected to {}, setting volume {}/127", address, volume.min(127));
    avctp
        .command(&vendor(CT_CONTROL, PDU_SET_ABSOLUTE_VOLUME, &[volume.min(127)]))
        .await?;
    avctp.register_volume_notification().await?;

    let mut buf = [0u8; 1024];
    loop {
        let n = avctp.ch.recv(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let p = &buf[..n];
        if n < 6 {
            continue;
        }
        let (label, is_rsp) = (p[0] >> 4, p[0] & 0x02 != 0);
        let avc = &p[3..];
        let (ctype, opcode) = (avc[0], avc[2]);

        if is_rsp {
            if opcode == OP_VENDOR_DEPENDENT && avc.len() >= 11 {
                let (pdu, params) = (avc[6], &avc[10..]);
                match (pdu, ctype) {
                    (PDU_SET_ABSOLUTE_VOLUME, RSP_ACCEPTED) => {
                        debug!("AVRCP volume set to {}/127", params.first().copied().unwrap_or(0))
                    }
                    (PDU_REGISTER_NOTIFICATION, RSP_INTERIM | RSP_CHANGED) if params.len() >= 2 => {
                        debug!("AirPods volume is {}/127", params[1] & 0x7f);
                        if ctype == RSP_CHANGED {
                            avctp.register_volume_notification().await?;
                        }
                    }
                    _ => debug!("AVRCP response ctype=0x{ctype:02x} pdu=0x{pdu:02x}"),
                }
            }
            continue;
        }

        // Commands from the headphones (they also act as an AVRCP controller).
        let reply = match opcode {
            OP_PASS_THROUGH if avc.len() >= 4 => {
                debug!(
                    "AVRCP pass-through op=0x{:02x} {}",
                    avc[3] & 0x7f,
                    if avc[3] & 0x80 != 0 { "released" } else { "pressed" }
                );
                let mut r = avc.to_vec();
                r[0] = RSP_ACCEPTED;
                r
            }
            OP_VENDOR_DEPENDENT if avc.len() >= 10 => match (avc[6], ctype) {
                (PDU_GET_CAPABILITIES, CT_STATUS) => {
                    let cap_id = avc.get(10).copied().unwrap_or(3);
                    let params: Vec<u8> = if cap_id == 2 {
                        vec![2, 1, 0x00, 0x19, 0x58] // company IDs: BT SIG
                    } else {
                        vec![cap_id, 0] // no events supported as target
                    };
                    vendor(RSP_STABLE, PDU_GET_CAPABILITIES, &params)
                }
                (PDU_SET_ABSOLUTE_VOLUME, CT_CONTROL) => {
                    let v = avc.get(10).copied().unwrap_or(0) & 0x7f;
                    vendor(RSP_ACCEPTED, PDU_SET_ABSOLUTE_VOLUME, &[v])
                }
                _ => {
                    let mut r = avc.to_vec();
                    r[0] = RSP_NOT_IMPLEMENTED;
                    r
                }
            },
            _ => {
                let mut r = avc.to_vec();
                r[0] = RSP_NOT_IMPLEMENTED;
                r
            }
        };
        if let Err(e) = avctp.respond(label, &reply).await {
            warn!("AVRCP reply failed: {}", e);
        }
    }
}
