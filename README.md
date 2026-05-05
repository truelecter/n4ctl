# n4ctl

**Disclaimer**: this code is mainly AI written, but gets work done.

Standalone Rust controller for the **Mirabox N4** and related N4-family devices (Ajazz AKP05, Mirabox N4E / N4 Pro, etc.). No StreamDock, no CEF UI, no Node/C# plugins. One main binary plus a small Windows launcher, one TOML config.

## Integrations

- **OBS**: WebSocket v5 (`obws` 0.14). Put the password in `OBS_PASSWORD` (`password_env = "OBS_PASSWORD"`) or inline with `password = "..."`.
- **Voicemeeter** (Windows): loads `VoicemeeterRemote64.dll` via `libloading`; path is auto-detected but can be overridden with `dll_path`.
- **System volume** (Windows): default render endpoint via CoreAudio.
- **Discord mute**: driven by Discord's own "push-to-mute" keybind — configure the same combo in both the Discord client and `n4ctl`'s `hotkey` action.

## Device layout

Device surface for the base N4 (top to bottom):

1. Row 1 (upper) — 5 keys with 112x112 displays (`key_0 .. key_4`)
2. Row 2 (lower) — 5 keys with 112x112 displays (`key_5 .. key_9`)
3. Sensor strip — 4 zones with backlit LED tiles (`strip_0 .. strip_3`)
4. 4 rotary encoders with press and rotate (`knob_0 .. knob_3`)

The N4's firmware uses a non-linear image-index layout (strip at 0..3, a gap at 4, row 2 at 5..9, row 1 at 10..14); `n4ctl` handles this translation for you.

## Quick start

```powershell
# 1. Plug in your N4 and confirm it's detected.
cargo run --release -- list

# 2. (optional) Verify the physical layout of each control. Pressed keys flash
#    green, released keys flash blue. Each event is also logged as key_N /
#    strip_N / knob_N so you know what to write in the TOML.
cargo run --release -- map

# 3. Copy the example config and run.
Copy-Item .\config.example.toml .\config.toml
cargo run --release -- --config .\config.toml
# (equivalent: `... -- run` — `run` is the default when omitted)
```

A `release` build is a single `n4ctl.exe` (a few MB). On Windows you also get `n4ctl-launcher.exe`, a console-free stub used by scheduled-task autostart so the task does not flash a window.

## CLI

| command | purpose |
| --- | --- |
| `n4ctl` / `n4ctl run` | run the controller (default subcommand); `--config <path>` selects the TOML |
| `n4ctl list` | enumerate matching HID devices |
| `n4ctl raw` | dump raw HID input reports (diagnostic) |
| `n4ctl map` | log decoded logical events and highlight pressed keys |
| `n4ctl probe` | light each firmware image index in turn (map physical positions to indices); optional `--max`, `--dwell-ms` |
| `n4ctl install` / `n4ctl uninstall` | Windows autostart via scheduled task (default) or Windows service; see `--help` |

## Config

See [`config.example.toml`](config.example.toml). If you omit `--config`, the first existing file among `./config.toml` and `~/n4ctl.toml` is used (otherwise `./config.toml` is attempted and you get a clear missing-file error).

Top-level sections:

- `[device]` — brightness (0..=100), optional asset root dir
- `[obs]` — `url`, `password` / `password_env`
- `[voicemeeter]` — `flavor = "banana" | "potato" | "standard"` + optional DLL path

Then one or more `[[pages]]` tables. The first page (or the one with `default = true`) is shown on startup. Each page has a `[pages.slots]` sub-table keyed by **slot id**:

- `key_0 .. key_4` — row 1 (upper, displayed)
- `key_5 .. key_9` — row 2 (lower, displayed)
- `strip_0 .. strip_3` — sensor strip (4 zones, backlit tiles)
- `knob_0 .. knob_3` — rotary encoders (no image)
- `swipe` — whole-strip swipe gesture (only fires `on_rotate`; the rotation value is `-1` for a left swipe and `+1` for a right swipe)

Each slot can define:

- `image` — PNG, JPEG, BMP, **GIF** (animated GIFs cycle on the key), or SVG path (resolved relative to the config file, or `[device].assets_root`). SVGs are rasterised to 112×112 on load and are the recommended format — they stay sharp and are trivial to recolour in a text editor.
- `image_on` — alternate image for the "active" state (2-state icons)
- `on_press`, `on_release`, `on_rotate` — action tables (see below)

## Runtime behaviour

- **Hot reload.** Saving `config.toml` re-parses, validates and re-renders without restart.
- **OBS sync.** On connect `n4ctl` subscribes to OBS events and updates scene / virtual-cam icons as you change things in OBS itself.
- **Voicemeeter sync.** A background poller (~400 ms) mirrors each `voicemeeter.mute` slot's state from the live Voicemeeter session.
- **Auto reconnect.** If the device disappears or a read stream errors out repeatedly, the session tears down and is re-established with a 2 s backoff — no process restart needed.

Swap colours by opening the SVG in any editor — e.g. `fill="#22c55e"` on `mic_on.svg`. Changes are picked up on the next hot reload.

### Built-in actions

| `action =` | params | notes |
| --- | --- | --- |
| `obs.scene` | `scene` (name), optional `collection` | 2-state: only the active-scene slot lights up. |
| `obs.virtual_cam` | — | Toggles OBS virtual camera; icon follows live state. |
| `system.volume` | `step` (percent per tick, signed) | Usually on `on_rotate`. |
| `hotkey` | `keys = ["Ctrl", "Shift", "M"]` | Simulates a global key combo. Use for Discord mute etc. |
| `voicemeeter.gain` | `target = "Strip" \| "Bus"`, `index`, `step` (dB/tick), optional `min`/`max` | |
| `voicemeeter.mute` | `target`, `index` | Polled every ~400ms so the icon reflects Voicemeeter's real state. |
| `page.next` / `page.prev` | — | Cycle through `[[pages]]`. |
| `page.cycle` | — | Cycle pages; on `on_rotate` bindings (knobs or `swipe`) it honors the rotation sign. |
| `page.goto` | `page` (name) | Jump to a named page. |

Any slot key can be omitted; keys without an `image` just stay blank.

## Build

```powershell
cargo build --release
.\target\release\n4ctl.exe --help
```

Release is tested with `mirajazz = "0.13"`, `obws = "0.14"`, `enigo = "0.3"`, stable Rust 1.80+.

Licensed under the [Mozilla Public License 2.0](LICENSE). Device I/O uses [`mirajazz`](https://github.com/4ndv/mirajazz) (MPL-2.0); input decoding takes behavioral cues from [`opendeck-akp05`](https://github.com/ambiso/opendeck-akp05).
