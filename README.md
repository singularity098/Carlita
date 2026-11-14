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

* Several tracks of plugins playing at once (LV2, VST2, VST3, CLAP, LADSPA,
  Carla internals, and Windows plugins bridged with yabridge). Audio and MIDI
  flow from each plugin into the next, and parallel blocks split the signal
  into lanes (one can stay dry) and mix them back together.
* Each track picks its own hardware inputs (any of an interface's
  channels: one for mono, two for a stereo pair) and its own MIDI devices.
  While picking, each input lights up when it hears something, and the
  options set how many inputs the interface has.
* Add, remove, reorder, split and bypass plugins while audio keeps running.
* Tracks are tabs in the top bar, which Carlita draws itself in place of the
  window manager's title bar. In fullscreen, swipe down from the **top** edge
  to bring it in.
* Swipe in from the **left** edge for the chain and from the **right** edge
  for options (audio, presets, MIDI learn, WiFi, Bluetooth, display, system),
  and up from the **bottom** edge for the on-screen piano. With a mouse, a
  tab shows when the pointer is at an edge. Each panel has a button that
  folds it away again.
* The selected plugin's editor is shown inside Carlita's window, in the main
  area (editors that can resize fill it).
* A flat dark look and a choice of themes (Display options).
* Each plugin can run in its own process, so a crashing plugin is swapped for
  a pass-through and the rest of the chain keeps playing.
* Stem recording (under JACK/PipeWire): open or name a project in options →
  Record (projects go in `~/Music/Carlita` unless you pick another folder),
  then record, pause, resume and stop there or with the record button in the
  top bar. Each take is a `Take NN` folder holding one file per track (what
  the track plays, after its plugins), all starting at the same sample, and
  optionally each track's dry inputs for re-amping. Files are 32-bit float
  WAV (RF64 past 4 GB, so takes can run as long as the disk allows) or
  24-bit FLAC, about half the size.

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

CI builds the AppImage on every push and pull request (download it from the
run's *Carlita-AppImage* artifact) and attaches it to a release for `v*` tags.

This bundles Carla (engine, plugin bridges and the scanner) next to
Carlita. Carla's plugin bridge is rebuilt from the same Carla release with
the patches in `packaging/appimage/carla-patches` (it needs Carla's build
dependencies, listed in the CI workflow). When PipeWire is running, the AppImage uses PipeWire's JACK
library, so Carlita talks to PipeWire directly and no `jackd` is needed.

### Tests

```bash
jackd --no-realtime -d dummy -r 48000 -p 256 &
```

```bash
xvfb-run -a -s "-screen 0 1280x800x24" cargo test --workspace
```

The engine test runs the real Carla engine against JACK. The editor-embedding
test creates X11 windows and checks that an editor is moved into the main
window, cut down to the part an overlay leaves uncovered (with a snapshot),
hidden, shown and handed back, and that clicks on the edge strips, F11 and the
keyboard focus reach the main window while an editor is shown.
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
| `SLINT_BACKEND=winit-femtovg` | The lighter femtovg GPU renderer instead of the default Skia over OpenGL (rougher edges on curves) |

Files are stored in `~/.config/carlita/settings.json`,
`~/.local/share/carlita/presets/*.json` and `~/.cache/carlita/plugins.json`.

## Layout

| Crate | Purpose |
| --- | --- |
| `crates/carla-sys` | bindgen bindings to `libcarla_standalone2` |
| `crates/carlita-core` | Rig model (tracks, chains, parallel blocks, routing), presets, settings, plugin catalog and scanner, MIDI learn. Contains no FFI and has full unit tests. |
| `crates/carlita-engine` | Engine thread that owns the Carla host: commands in, events and meters out |
| `crates/carlita-x11` | Finds plugin editor windows and shows them inside the main window (reparenting) |
| `crates/carlita-system` | NetworkManager, BlueZ and logind over D-Bus |
| `crates/carlita` | The Slint app (`ui/*.slint`) and the glue between all of the above |

## License

GPL-3.0-or-later (see [LICENSE](LICENSE)). Carla is GPL-2.0-or-later and
Slint is used under its GPL-3.0 option. GPL-3.0-or-later is the license the
combined binary ends up under anyway, and it remains compatible with Carla.
The `carla-sys` crate on its own is GPL-2.0-or-later.

The Jost font in `assets/fonts/` is under the SIL Open Font License
(`assets/fonts/OFL.txt`). The look and themes come from
[Xythos](https://github.com/singularity098/xythos).
