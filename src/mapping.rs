//! Translation between the mirajazz DeviceStateUpdate and logical slot ids.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use mirajazz::state::DeviceStateUpdate;
use serde::{Deserialize, Serialize};

use crate::inputs::{DISPLAYED_KEYS, ROW_KEYS, STRIP_KEYS, SWIPE_ENCODER};

/// Logical slot referring to a physical control on the N4.
///
/// * `Key(0..=4)`  -> row 1 (upper), keys 1.1..1.5.
/// * `Key(5..=9)`  -> row 2 (lower), keys 2.1..2.5.
/// * `Strip(0..=3)` -> sensor-strip zones 3.1..3.4.
/// * `Knob(0..=3)` -> rotary encoders 4.1..4.4.
/// * `Swipe`        -> whole-strip swipe gesture (emits rotate events only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "index")]
pub enum SlotId {
    Key(u8),
    Strip(u8),
    Knob(u8),
    Swipe,
}

impl SlotId {
    pub fn parse(s: &str) -> Option<Self> {
        if s == "swipe" {
            return Some(SlotId::Swipe);
        }
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
            SlotId::Knob(_) | SlotId::Swipe | SlotId::Key(_) | SlotId::Strip(_) => None,
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
        DeviceStateUpdate::EncoderDown(n) | DeviceStateUpdate::EncoderUp(n) => {
            if n == SWIPE_ENCODER {
                // Swipes never produce press/release - suppress noise.
                None
            } else {
                Some(SlotId::Knob(n))
            }
        }
        DeviceStateUpdate::EncoderTwist(n, _) => {
            if n == SWIPE_ENCODER {
                Some(SlotId::Swipe)
            } else {
                Some(SlotId::Knob(n))
            }
        }
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

#[derive(Default)]
struct RotateAcc {
    /// Slots in first-seen order (one entry per slot when it first appears).
    order: Vec<SlotId>,
    sums: HashMap<SlotId, i32>,
}

impl RotateAcc {
    fn add(&mut self, slot: SlotId, d: i8) {
        match self.sums.entry(slot) {
            Entry::Occupied(mut e) => {
                *e.get_mut() += d as i32;
            }
            Entry::Vacant(e) => {
                self.order.push(slot);
                e.insert(d as i32);
            }
        }
    }

    fn flush(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        let order = std::mem::take(&mut self.order);
        for slot in order {
            let Some(mut sum) = self.sums.remove(&slot) else {
                continue;
            };
            if sum == 0 {
                continue;
            }
            // N4 quirk: the first physical detent often arrives as six ±1 HID steps
            // (±6 total). Treat that as a single click so relative gain/volume steps
            // match the hardware.
            if sum.abs() == 6 {
                sum = sum.signum();
            }
            let v = sum.clamp(i8::MIN as i32, i8::MAX as i32) as i8;
            out.push(InputEvent::Rotate(slot, v));
        }
        self.sums.clear();
        out
    }
}

/// Merge `Rotate` deltas for the same slot between non-rotate events, and collapse
/// spurious ±6 totals (see [`RotateAcc::flush`]).
pub fn coalesce_rotate_batch(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out = Vec::with_capacity(events.len());
    let mut acc = RotateAcc::default();
    for ev in events {
        match ev {
            InputEvent::Rotate(s, d) => acc.add(s, d),
            other => {
                out.extend(acc.flush());
                out.push(other);
            }
        }
    }
    out.extend(acc.flush());
    out
}
