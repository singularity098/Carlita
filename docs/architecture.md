# Architecture

```
            ┌──────────── UI thread (Slint event loop) ────────────┐
 touch ───► │ ui/*.slint ◄──► app.rs (App state, 16 ms poll timer) │
            └───▲───────────────▲──────────────▲────────────▲──────┘
      commands/ │ events   area/│events   D-Bus│      MIDI  │ scan results
    ┌───────────┴──┐  ┌─────────┴───┐  ┌───────┴──────┐ ┌───┴────┐ ┌──────────┐
    │ engine thread│  │ x11 editor  │  │ system worker│ │ midir  │ │ scanner  │
    │ Carla host   │  │ embedding   │  │ NM/BlueZ/    │ │ (learn)│ │ carla-   │
    │ (patchbay)   │  │ (reparent)  │  │ logind       │ │        │ │ discovery│
    └──────┬───────┘  └─────────────┘  └──────────────┘ └────────┘ └──────────┘
           │ Carla owns the realtime audio thread and plugin bridge processes
```

* **The UI never blocks on audio work.** Every Carla call happens on the engine
  thread (`carlita-engine`). The UI sends `Command`s and polls `Event`s.
  Loading a preset adds one plugin per engine tick, so the UI keeps
  updating while it loads.
* **Tracks and parallel blocks.** The rig (`carlita-core::rig`) holds
  several tracks that play at once. Each track is a chain of blocks; a block
  is a plugin or a *parallel* block whose lanes each hold a serial chain of
  plugins (an empty lane is a dry path). Parallel blocks hold plain plugins
  only, which keeps the list readable on a touchscreen.

  ```
  in 1+2 ──► comp ─► ┌ A: amp ─► cab ┐ ─► reverb ─► [Guitar bus] ─┬─► output
                     └ B: (dry) ─────┘                           │
  in 5 ────► eq ─────────────────────────────────► [Vocals bus] ─┘
  KeyStep ─► synth ──────────────────────────────► [Keys bus] ───┘
  ```
* **Track inputs.** Each track picks any of the hardware input channels
  (one plays on both sides of a stereo plugin, two make a stereo pair) and
  any MIDI devices, or all of them (the default, which also takes devices
  plugged in later). Devices are stored by their port name ("client:port").
  Old presets' "stereo", "left", "right" and "none" read as inputs 1+2, 1,
  2 and none.
  The options set how many inputs the interface really has (*Input
  channels*, "All" by default), so tracks are not offered every port the
  audio system reports. While a track's input page is open, the engine
  meters each offered input on its own (`Command::ProbeInputs`, probe buses
  `Carlita.probe-<n>` with two inputs each) and the page lights the inputs
  that hear something, so playing shows where an instrument comes in.
* **Wiring.** Under JACK (and PipeWire), Carla runs in *multiple clients*
  mode: every plugin and bus is a JACK client of its own, named
  `Carlita.<slot> <plugin>` (`Carlita.track-<id>`, `Carlita.in-<id>`,
  `Carlita.out` for buses), and the rig is wired in the JACK graph, from the
  hardware ports straight to each track's first plugins. Carla's single
  client has only two audio inputs and one MIDI input that merges every
  device, so it cannot give each track its own. With other drivers (ALSA),
  Carla runs in *patchbay* mode and the rig is wired in its internal graph,
  whose inputs are the device's channels; MIDI devices are merged there.
  Either way, the UI sends the rig's layout (`Command::SetLayout`) and
  `rig::route` turns it into links between nodes. The engine resolves each
  node to ports and changes only the connections that differ, making new
  ones before removing old ones, so adding, moving and bypassing plugins
  happen while audio keeps running. In the JACK graph it only touches
  connections into its own clients and from its output bus to the output
  device; connections other programs make (say, recording Carlita's
  output) are left alone. A plugin without audio outputs (a tuner) passes audio
  on, and a plugin without MIDI outputs passes MIDI on, as in a rack.
* **Buses.** The engine adds Carla's internal `audiogain_s` as an output bus
  and two buses per track: one at its end, which carries the track volume
  (the gain parameter), mute (the bus is disconnected from the output) and
  the track's level, and one that only meters the inputs the track takes.
  The output bus feeds the first two hardware outputs.
* **Two graphs.** Carla reports either its internal graph or the JACK graph
  through the same callbacks. The engine puts a marker into the callback
  queue before a refresh, so every callback is filed under the right graph;
  under JACK it switches to the JACK graph once, at start. Plugin clients
  are matched to plugins by name: the plugin ids in JACK's metadata go stale
  when a plugin before them is removed.
* **Bypass.** Plugins that can mix dry and wet are set fully dry, so audio
  passes straight through. Plugins that cannot (synths, mono→stereo) are
  deactivated and routed around.
* **Crash isolation.** With *Isolate plugins* on (the default), every plugin
  runs in a Carla bridge process. When a bridge crashes or hangs, the slot is
  replaced in place by a unity-gain pass-through (`audiogain_s`), which is
  wired where the plugin was, so the rest of the rig keeps sounding. The slot shows
  "Crashed – tap to reload", and reloading restores its last known state.
  Plugins that fail to load get the same placeholder.
* **Presets** are JSON files that contain the whole rig, each plugin's
  Carla state (`.carxs` XML from `carla_save_plugin_state`), per-plugin
  and MIDI-learn mappings, and which plugin was shown (`selected`, plus
  `track_selections` for the other tracks), which is shown again on load.
  File writes are atomic. Version 1 presets (a single chain) load as a
  one-track rig.
* **Scanning** runs `carla-discovery-native` once per binary, in a separate
  process with a timeout. The results are cached in `plugins.json`.
  yabridge's `~/.vst/yabridge` and `~/.vst3/yabridge` folders are part of the
  standard search paths, and plugins found there are marked "yabridge".
  As in Carla's own scanner, the probe only reads each plugin's description
  (`CARLA_DISCOVERY_NO_PROCESSING_CHECKS`) rather than running it. yabridge
  plugins get six times the timeout, since each one starts Wine, and a probe
  that times out is killed with everything it started. Each scan writes
  `scan.log` next to `plugins.json`: the folders searched, how many plugins
  each held, and every probe's result with the error output of failures
  (where yabridge and Wine report). In the AppImage, Carlita gives the
  programs it starts the desktop's own `LD_LIBRARY_PATH` back, so Wine does
  not load the AppImage's bundled libraries.
* **Audio I/O.** Under JACK or PipeWire, the "input device" is the JACK
  client whose outputs are the input channels tracks pick from, and the
  "output device" the client the output bus feeds. The default is the
  physical `system` ports. The MIDI devices tracks pick from are the ports
  of `system`, PipeWire's `Midi-Bridge` and a2jmidid's `a2j`. Sample rate
  and buffer size are requested with `PIPEWIRE_QUANTUM` under PipeWire, or
  passed to the driver under ALSA. A plain JACK server sets its own values.

## Status against the requirements

| Requirement | Status |
| --- | --- |
| Serial chain, audio + MIDI passed down the chain | Done (Carla patchbay mode, wired by the engine) |
| Parallel branches | Done: parallel blocks with any number of lanes, empty lanes are dry. Lanes are summed; Carla does not compensate latency between lanes, so lanes with different PDC drift apart by that delay. |
| Several chains at once (tracks) | Done: each track takes any hardware inputs (up to 64) and any MIDI devices (under JACK/PipeWire; with ALSA every track gets all MIDI), and has volume, mute and meters. |
| JACK / PipeWire, device, sample rate, buffer size | Done. PipeWire goes through pipewire-jack. |
| One-tap bypass | Done |
| Add / remove / reorder while running | Done, covered by the engine test |
| Plugin scan, remembered list | Done |
| yabridge plugins | Done (they appear as native plugins) |
| Named presets, default preset at startup | Done |
| Crash/hang isolation | Done with bridges. The crash is detected from Carla's error callbacks. |
| Level meters, DSP load, per-plugin PDC | Done. The meters read the input and output buses, and each track's bus, once per display frame (`Command::SetMeterInterval`); the bars jump up and fall back smoothly between reports (`src/meters.rs`). The DSP load and latency texts change at most four times a second, since any property change redraws the whole window on the GPU renderers. Total latency uses the longest path. |
| MIDI learn for bypass | Done: toggle or momentary, CC, note or program change, saved per preset |
| Top bar: track tabs with levels, app-drawn window controls (no WM decorations), docked when windowed, swipe-down from the top edge in fullscreen | Done. Moving and resizing go through winit's `drag_window` / `drag_resize_window`, so the window manager still snaps the window. |
| Left sidebar: edge swipe, hover tab, list, drag reorder (into and out of lanes), swipe/long-press remove with undo, browser with search/format/category/favorites, bypass, close gestures, 48 px+ targets | Done |
| Right sidebar: audio, presets, WiFi (NetworkManager), Bluetooth (BlueZ), display, on-screen keyboard toggle; rescan, restart, reboot and shut down as icon buttons in its fixed header | Done. WiFi and Bluetooth are untested on real hardware. |
| Hover tips | Done (`ui/tooltip.slint`): a control holds a `Tip` bound to its TouchArea; the window draws the one tip (`TooltipLayer`) after 600 ms. A press hides it, so touch never shows one. Tips under a plugin editor are hidden by it (nothing draws over an editor). |
| On-screen text keyboard (WiFi passwords, names) | Done. A hardware keyboard types into the same prompt (Return accepts, Escape cancels). |
| Virtual piano → plugins | Done. Notes go to the selected plugin if it takes MIDI, otherwise to the first plugin that does. Sliding across the keys plays each key passed over. Opens from a strip on the bottom edge (hover tab), folds away with its own button. Single touch only. |
| Fullscreen default, windowed, F11, size remembered | Done. Real fullscreen needs a window manager (kiosk setup uses matchbox). |
| Editor shown in the main area, native resize | Done: editors are reparented into the main window, at their own size; resizable editors fill the area. Scaling was dropped. Sidebars and the track bar cut the editor down instead of hiding it, with a snapshot under them. See [editor-embedding.md](editor-embedding.md). |
| Generic parameter page for plugins without editors | Done |
| Stem recording | Done (JACK/PipeWire only): `src/recorder.rs` is Carlita's own JACK client, `Carlita.recorder`, with an input per channel of each track's bus output (and of the inputs the track takes, for dry stems); the engine wires it like its own clients (`Command::SetRecorder`, ports named by `recorder_port`). All its ports are in one client, so every stem gets the same cycles and they line up to the sample; when the writer falls behind, a whole cycle is left out of every stem alike. The audio thread only copies into `rtrb` ring buffers; a writer thread per file (`src/stem_file.rs`) writes a 32-bit float WAV or a 24-bit FLAC (`flacenc`; peaks over 0 dBFS are clipped), as picked on the Record page, and updates its header every 2 s, so a crash leaves files that play. A take is a `Take NN` folder in the project with a `take.txt`. Pause stops writing and resume carries on in the same files. Tracks added during a take are recorded from the next one. A WAV file turns into RF64 (EBU Tech 3306: its JUNK chunk becomes ds64) when it passes 4 GB, about 1.5 hours of a stereo stem at 96 kHz, so takes have no length limit; FLAC has none either (2^36 frames). |
| Dark theme, 60 fps sidebar slides that follow the finger | Done. The look and themes come from Xythos (`ui/theme.slint`, flat dark background; Jost in `assets/fonts/`). Drawn with Slint's Skia renderer over OpenGL (only the `renderer-skia-opengl` feature: plain `renderer-skia` also builds Skia over wgpu and prefers it, which dropped frames on a discrete GPU; femtovg left round buttons visibly jagged on some GPUs; it stays selectable with `SLINT_BACKEND=winit-femtovg`). Skia's GPU backends still draw `Path` elements with hard edges, so round buttons are rounded rectangles and the line icons are SVG images rasterised on the CPU at their drawn size and tinted (`ui/icons.slint`, `src/icons.rs`); avoid `Path` for anything visible. The theme gradients need a GPU renderer; the software renderer draws gradient fills without rounded corners. |
| Kiosk mode | Example systemd and xinit files in `packaging/kiosk/` |
| AppImage | `packaging/appimage/build-appimage.sh` and a CI job on tags. The full AppImage build has not been tried yet. |
| CLAP | Needs Carla ≥ 2.6. Ubuntu ships 2.5.8, so bundle a newer Carla in the AppImage. |
