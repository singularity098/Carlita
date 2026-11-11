<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/branding/carlita-logo.svg">
    <img src="assets/branding/carlita-logo-light.svg" alt="Carlita" width="420">
  </picture>
</p>

# Carlita

A touch-first plugin chain host for a pedalboard: a Rust app built on the
[Carla](https://kx.studio/Applications:Carla) engine with a
[Slint](https://slint.dev) UI, made for a small Linux PC with a ~12"
touchscreen.

* One live chain of plugins (LV2, VST2, VST3, CLAP, LADSPA, Carla internals,
  and Windows plugins bridged with yabridge). Audio and MIDI flow from each
  plugin into the next.
* Add, remove, reorder and bypass plugins while audio keeps running.
* Swipe in from the **left** edge for the chain and from the **right** edge
  for options (audio, presets, MIDI learn, WiFi, Bluetooth, display, system).
* Plugin editors fill the screen, and small fixed-size editors are scaled up.
  Touches are mapped back to the editor's own coordinates.
* Each plugin can run in its own process, so a crashing plugin is swapped for
  a pass-through and the rest of the chain keeps playing.

The full requirements are in [docs/requirements.md](docs/requirements.md).
[docs/architecture.md](docs/architecture.md) describes the design and
the current state of each requirement.

## Building

Ubuntu 24.04 / Debian:

```bash
sudo apt install carla carla-dev clang libclang-dev libasound2-dev libxkbcommon-dev libfontconfig-dev
```

```bash
cargo run --release -- --windowed
```

Carla's headers and library are found with `pkg-config` (`carla-standalone`).
To use a Carla you built yourself (≥ 2.6 is needed for CLAP), set
`CARLA_INCLUDE_DIR=/opt/carla/include/carla` and
`CARLA_LIB_DIR=/opt/carla/lib/carla`.

### AppImage

```bash
packaging/appimage/build-appimage.sh
```

This bundles Carla (engine, plugin bridges and the scanner) next to
Carlita. When PipeWire is running, the AppImage uses PipeWire's JACK
library, so Carlita talks to PipeWire directly and no `jackd` is needed.

### Tests

```bash
jackd --no-realtime -d dummy -r 48000 -p 256 &
```

```bash
xvfb-run -a -s "-screen 0 1280x800x24 +extension Composite" cargo test --workspace
```

The engine test runs the real Carla engine against JACK. The editor-capture
test creates an X11 window, captures it off-screen and forwards a click to it.
Both tests skip themselves when JACK or X is not available, unless
`CARLITA_REQUIRE_ENGINE=1` is set (CI sets it).

## Running

| Option | Effect |
| --- | --- |
| `--windowed` / `--fullscreen` | Override the saved window mode (F11 toggles it) |
| `--kiosk` | Fullscreen, for boot-to-app setups (see `packaging/kiosk/`) |
| `--no-preset` | Do not load the default preset at startup |
| `CARLITA_HOME=dir` | Keep settings, presets and the plugin cache below `dir` |
| `SLINT_BACKEND=winit-software` | Software rendering on machines without OpenGL |

Files are stored in `~/.config/carlita/settings.json`,
`~/.local/share/carlita/presets/*.json` and `~/.cache/carlita/plugins.json`.

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/carla-sys` | bindgen bindings to `libcarla_standalone2` |
| `crates/carlita-core` | Chain model, presets, settings, plugin catalog and scanner, MIDI learn, editor scaling maths. Contains no FFI and has full unit tests. |
| `crates/carlita-engine` | Engine thread that owns the Carla host: commands in, events and meters out |
| `crates/carlita-x11` | Finds plugin editor windows, captures them with XComposite/XDamage and forwards input |
| `crates/carlita-system` | NetworkManager, BlueZ and logind over D-Bus |
| `crates/carlita` | The Slint app (`ui/*.slint`) and the glue between all of the above |

## License

GPL-3.0-or-later (see [LICENSE](LICENSE)). Carla is GPL-2.0-or-later and
Slint is used under its GPL-3.0 option. GPL-3.0-or-later is the license the
combined binary ends up under anyway, and it remains compatible with Carla.
The `carla-sys` crate on its own is GPL-2.0-or-later.
