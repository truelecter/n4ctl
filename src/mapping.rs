//! Translation between the mirajazz DeviceStateUpdate and logical slot ids.

use mirajazz::state::DeviceStateUpdate;
use serde::{Deserialize, Serialize};

use crate::inputs::{DISPLAYED_KEYS, ROW_KEYS, STRIP_KEYS};

/// Logical slot referring to a physical control on the N4.
///
/// * `Key(0..=4)`  -> row 1 (upper), keys 1.1..1.5.
/// * `Key(5..=9)`  -> row 2 (lower), keys 2.1..2.5.
/// * `Strip(0..=3)` -> sensor-strip zones 3.1..3.4.
/// * `Knob(0..=3)` -> rotary encoders 4.1..4.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "index")]
pub enum SlotId {
    Key(u8),
    Strip(u8),
    Knob(u8),
}

impl SlotId {
    pub fn parse(s: &str) -> Option<Self> {
        let (prefix, num) = s.split_once('_')?;
        let n: u8 = num.parse().ok()?;
        match prefix {
            "key" if n < DISPLAYED_KEYS => Some(SlotId::Key(n)),
            "strip" if n < STRIP_KEYS => Some(SlotId::Strip(n)),
            "knob" if n < 4 => Some(SlotId::Knob(n)),
            _ => None,
        }
    }

    /// Mirajazz `set_button_image` index for this slot, if it has a display.
    ///
    /// Empirically verified on the Mirabox N4:
    /// * `Strip(0..=3)` -> image idx `0..=3` (strip LEDs).
    /// * image idx 4 is an unused gap.
    /// * `Key(5..=9)` (row 2) -> image idx `5..=9`.
    /// * `Key(0..=4)` (row 1) -> image idx `10..=14`.
    pub fn image_index(self) -> Option<u8> {
        match self {
            SlotId::Key(n) if n < ROW_KEYS => Some(10 + n),
            SlotId::Key(n) if n < DISPLAYED_KEYS => Some(n),
            SlotId::Strip(n) if n < STRIP_KEYS => Some(n),
            _ => None,
        }
    }

    /// Iterate every slot on the device that has a physical display, in a
    /// stable order suitable for clearing / rendering.
    pub fn all_displayed() -> impl Iterator<Item = SlotId> {
        (0..DISPLAYED_KEYS)
            .map(SlotId::Key)
            .chain((0..STRIP_KEYS).map(SlotId::Strip))
    }
}

/// Logical input event produced by the device.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Press(SlotId),
    Release(SlotId),
    Rotate(SlotId, i8),
}

/// Try to resolve a DeviceStateUpdate to a SlotId (without direction).
pub fn update_to_slot(u: &DeviceStateUpdate) -> Option<SlotId> {
    match *u {
        DeviceStateUpdate::ButtonDown(n) | DeviceStateUpdate::ButtonUp(n) => raw_button_slot(n),
        DeviceStateUpdate::EncoderDown(n) | DeviceStateUpdate::EncoderUp(n) => Some(SlotId::Knob(n)),
        DeviceStateUpdate::EncoderTwist(n, _) => Some(SlotId::Knob(n)),
    }
}

fn raw_button_slot(raw: u8) -> Option<SlotId> {
    match raw {
        0..=9 => Some(SlotId::Key(raw)),
        10..=13 => Some(SlotId::Strip(raw - DISPLAYED_KEYS)),
        _ => None,
    }
}

pub fn update_to_event(u: &DeviceStateUpdate, slot: SlotId) -> InputEvent {
    match *u {
        DeviceStateUpdate::ButtonDown(_) | DeviceStateUpdate::EncoderDown(_) => InputEvent::Press(slot),
        DeviceStateUpdate::ButtonUp(_) | DeviceStateUpdate::EncoderUp(_) => InputEvent::Release(slot),
        DeviceStateUpdate::EncoderTwist(_, v) => InputEvent::Rotate(slot, v),
    }
}

/// Expand a single `DeviceStateUpdate` into zero or more logical
/// `InputEvent`s, with two N4-specific quirks handled:
///
/// 1. Strip zones only fire a single "press" HID event, so we synthesise a
///    matching `Release` immediately after each `Press`.
/// 2. mirajazz will emit a spurious `ButtonUp(strip)` next time a different
///    button is pressed (because our strip state vector flips from `true` to
///    `false`). We drop those so unrelated presses don't retrigger
///    `on_release` on strip slots.
pub fn expand_update(u: &DeviceStateUpdate) -> Vec<InputEvent> {
    let Some(slot) = update_to_slot(u) else { return Vec::new() };
    match (slot, u) {
        (SlotId::Strip(_), DeviceStateUpdate::ButtonDown(_)) => {
            vec![InputEvent::Press(slot), InputEvent::Release(slot)]
        }
        (SlotId::Strip(_), DeviceStateUpdate::ButtonUp(_)) => Vec::new(),
        _ => vec![update_to_event(u, slot)],
    }
}
