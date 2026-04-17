//! Send keyboard combos via `enigo`.

use anyhow::{Context, Result, anyhow};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use crate::{actions::ActionContext, config::ActionSpec};

pub struct HotkeyBackend;

impl HotkeyBackend {
    pub fn new() -> Self {
        Self
    }
}

pub async fn send_hotkey(ctx: &ActionContext, spec: &ActionSpec) -> Result<()> {
    let keys = spec
        .get("keys")
        .and_then(|v| v.as_array())
        .context("hotkey.keys must be a non-empty array")?;
    if keys.is_empty() {
        return Err(anyhow!("hotkey.keys must be a non-empty array"));
    }

    let names: Vec<String> = keys
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    let _guard = ctx.hotkey.lock().await;

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut enigo = Enigo::new(&Settings::default()).context("init enigo")?;
        let parsed: Vec<Key> = names
            .iter()
            .map(|n| parse_key(n))
            .collect::<Result<Vec<_>>>()?;
        for k in &parsed {
            enigo.key(*k, Direction::Press).context("key press")?;
        }
        for k in parsed.iter().rev() {
            enigo.key(*k, Direction::Release).context("key release")?;
        }
        Ok(())
    })
    .await??;
    Ok(())
}

fn parse_key(name: &str) -> Result<Key> {
    let n = name.trim().to_ascii_lowercase();
    Ok(match n.as_str() {
        "ctrl" | "control" | "lctrl" | "leftctrl" => Key::Control,
        "rctrl" | "rightctrl" => Key::RControl,
        "alt" | "lalt" | "leftalt" => Key::Alt,
        "shift" | "lshift" | "leftshift" => Key::Shift,
        "rshift" | "rightshift" => Key::RShift,
        "win" | "meta" | "super" | "lwin" | "leftwin" => Key::Meta,
        "esc" | "escape" => Key::Escape,
        "enter" | "return" => Key::Return,
        "space" => Key::Space,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "insert" => Key::Insert,
        s if s.starts_with('f') && s[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u32 = s[1..].parse().context("parse fN")?;
            match n {
                1 => Key::F1, 2 => Key::F2, 3 => Key::F3, 4 => Key::F4,
                5 => Key::F5, 6 => Key::F6, 7 => Key::F7, 8 => Key::F8,
                9 => Key::F9, 10 => Key::F10, 11 => Key::F11, 12 => Key::F12,
                13 => Key::F13, 14 => Key::F14, 15 => Key::F15, 16 => Key::F16,
                17 => Key::F17, 18 => Key::F18, 19 => Key::F19, 20 => Key::F20,
                21 => Key::F21, 22 => Key::F22, 23 => Key::F23, 24 => Key::F24,
                other => return Err(anyhow!("unknown F-key: F{other}")),
            }
        }
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            Key::Unicode(c)
        }
        other => return Err(anyhow!("unknown key name: {other}")),
    })
}
