//! Raw HID byte decoder for Mirabox N4-family devices.
//!
//! Decoded mirajazz "key" indices (used internally; see `layout.rs` for the
//! mapping to physical positions):
//!
//! * 0..=4  -> row 1 (upper) keys 1.1..1.5
//! * 5..=9  -> row 2 (lower) keys 2.1..2.5
//! * 10..=13 -> sensor-strip zones 3.1..3.4 (no display of their own; LEDs
//!   share image indices 0..=3 on the N4).
//!
//! Encoders 0..=3 are the knobs (rotate + press).

use mirajazz::{error::MirajazzError, types::DeviceInput};

/// Total number of "buttons" mirajazz tracks internally. Must be large enough
/// to cover every input index we emit (row 1+2 + strip = 14).
pub const KEY_COUNT: usize = 14;
pub const ENCODER_COUNT: usize = 4;

pub const ROW_KEYS: u8 = 5;
pub const DISPLAYED_KEYS: u8 = 2 * ROW_KEYS;
pub const STRIP_KEYS: u8 = 4;

pub fn process_input(input: u8, state: u8) -> Result<DeviceInput, MirajazzError> {
    tracing::trace!("hid input=0x{:02x} state={}", input, state);

    match input {
        // HID byte 1..=5 -> row 1 (upper), 6..=10 -> row 2 (lower).
        1..=10 => Ok(button_event(input - 1, state != 0)),

        // Sensor strip (4 zones). The N4 only emits a single HID event per
        // touch (no release packet, and `state` byte is 0) so we always treat
        // a strip code as an instantaneous press. mirajazz will emit
        // ButtonUp(prev-zone) + ButtonDown(new-zone) when a different zone is
        // touched, which is what our on_press dispatcher wants.
        //
        // We *intentionally* do not include 0x38/0x39 here - on the N4 those
        // appear to be swipe/boundary events, not a 5th zone.
        0x40 => Ok(button_event(10, true)),
        0x41 => Ok(button_event(11, true)),
        0x42 => Ok(button_event(12, true)),
        0x43 => Ok(button_event(13, true)),

        0xa0 => Ok(encoder_twist(0, -1)),
        0xa1 => Ok(encoder_twist(0, 1)),
        0x50 => Ok(encoder_twist(1, -1)),
        0x51 => Ok(encoder_twist(1, 1)),
        0x90 => Ok(encoder_twist(2, -1)),
        0x91 => Ok(encoder_twist(2, 1)),
        0x70 => Ok(encoder_twist(3, -1)),
        0x71 => Ok(encoder_twist(3, 1)),

        0x37 | 0x00 => Ok(encoder_press(0)),
        0x35 => Ok(encoder_press(1)),
        0x33 => Ok(encoder_press(2)),
        0x36 => Ok(encoder_press(3)),

        // Unknown / swipe / boundary. Don't spam an error - just report NoData.
        _ => Ok(DeviceInput::NoData),
    }
}

fn button_event(idx: u8, pressed: bool) -> DeviceInput {
    let mut states = vec![false; KEY_COUNT];
    if (idx as usize) < states.len() && pressed {
        states[idx as usize] = true;
    }
    DeviceInput::ButtonStateChange(states)
}

fn encoder_twist(idx: u8, val: i8) -> DeviceInput {
    let mut vals = vec![0i8; ENCODER_COUNT];
    if (idx as usize) < vals.len() {
        vals[idx as usize] = val;
    }
    DeviceInput::EncoderTwist(vals)
}

fn encoder_press(idx: u8) -> DeviceInput {
    let mut states = vec![false; ENCODER_COUNT];
    if (idx as usize) < states.len() {
        states[idx as usize] = true;
    }
    DeviceInput::EncoderStateChange(states)
}
