//! Device discovery and connection for supported Mirabox/Ajazz N4-family devices.

use anyhow::{Context, Result, anyhow};
use mirajazz::{
    device::{Device, DeviceQuery, list_devices},
    types::HidDevice,
};
use tracing::info;

use crate::inputs::{ENCODER_COUNT, KEY_COUNT};

// Known devices (VID, PID, human name)
const KNOWN: &[(u16, u16, &str)] = &[
    (0x6602, 0x1001, "Mirabox N4"),
    (0x6603, 0x1007, "Mirabox N4E"),
    (0x5548, 0x1008, "Mirabox N4 Pro"),
    (0x5548, 0x1021, "Mirabox N4 Pro E"),
    (0x5548, 0x1023, "VSDInside N4 Pro"),
    (0x0300, 0x3004, "Ajazz AKP05E"),
    (0x0300, 0x3013, "Ajazz AKP05E Pro"),
    (0x0300, 0x3006, "Ajazz AKP05"),
    (0x0b00, 0x1003, "Mars Gaming MSD-Pro"),
    (0x1500, 0x3002, "Soomfon CN003"),
    (0x0200, 0x3001, "Redragon SS552"),
];

fn queries() -> Vec<DeviceQuery> {
    KNOWN
        .iter()
        .map(|(vid, pid, _)| DeviceQuery::new(65440, 1, *vid, *pid))
        .collect()
}

fn name_for(vid: u16, pid: u16) -> Option<&'static str> {
    KNOWN.iter().find(|(v, p, _)| *v == vid && *p == pid).map(|(_, _, n)| *n)
}

pub struct Found {
    pub hid: HidDevice,
    pub vendor_id: u16,
    pub product_id: u16,
    pub name: Option<String>,
    pub serial_number: Option<String>,
}

pub async fn find_connected() -> Result<Vec<Found>> {
    let qs = queries();
    let set = list_devices(&qs).await.context("list_devices")?;
    let mut out = Vec::new();
    for hid in set.into_iter() {
        // HidDevice derefs to DeviceInfo, so we can read these fields directly.
        let vid = hid.vendor_id;
        let pid = hid.product_id;
        let serial = hid.serial_number.clone();
        out.push(Found {
            name: name_for(vid, pid).map(|s| s.to_string()),
            vendor_id: vid,
            product_id: pid,
            serial_number: serial,
            hid,
        });
    }
    Ok(out)
}

/// Opens the first matching device. Protocol v3 is assumed for N4-family.
pub async fn open_first() -> Result<Device> {
    let mut found = find_connected().await?;
    if found.is_empty() {
        return Err(anyhow!(
            "No supported device found (checked {} VID/PID pairs). Is the N4 plugged in?",
            KNOWN.len()
        ));
    }
    let target = found.remove(0);
    info!(
        "Opening {} (vid=0x{:04x} pid=0x{:04x} serial={:?})",
        target.name.as_deref().unwrap_or("<unknown>"),
        target.vendor_id,
        target.product_id,
        target.serial_number
    );
    // `HidDevice` derefs to `DeviceInfo`, which is what `Device::connect` expects.
    let device = Device::connect(&target.hid, 3, KEY_COUNT, ENCODER_COUNT)
        .await
        .context("Device::connect")?;
    // N4 / AKP05 firmware only emits one HID event per encoder click (no
    // separate release). mirajazz would otherwise wait for a state=false
    // transition before firing EncoderUp, which never comes, so disable the
    // "both states" mode and let mirajazz synthesize Down+Up per click.
    let device = device.with_supports_both_encoder_states(false);
    Ok(device)
}
