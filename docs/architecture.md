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
  input ─┬─► comp ─► ┌ A: amp ─► cab ┐ ─► reverb ─► [Guitar bus] ─┬─► output
         │           └ B: (dry) ─────┘                           │
         └─► (input 2 only) eq ──────────────────► [Vocals bus] ─┘
  ```
* **Patchbay wiring.** Carla runs in *patchbay* mode. The UI sends the rig's
  layout (`Command::SetLayout`) and `rig::route` turns it into links between
  nodes. The engine resolves each node to a group in Carla's internal graph
  and changes only the connections that differ, making new ones before
  removing old ones, so adding, moving and bypassing plugins happen while
  audio keeps running. A plugin without audio outputs (a tuner) passes audio
  on, and a plugin without MIDI outputs passes MIDI on, as in a rack.
* **Buses.** The engine adds Carla's internal `audiogain_s` as an input bus,
  an output bus and one bus per track. The buses carry the meters, the track
  volume (the gain parameter) and mute (the bus is disconnected from the
  output). Hardware → input bus and output bus → hardware are fixed links.
* **Two graphs.** Carla reports either its internal graph or the JACK graph,
  whichever was refreshed last, through the same callbacks. The engine keeps
  the internal graph reported and switches to the JACK graph only briefly
  (auto-connect, device lists), putting a marker into the callback queue
  before each refresh so every callback is filed under the right graph.
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
  and MIDI-learn mappings. File writes are atomic. Version 1 presets
  (a single chain) load as a one-track rig.
* **Scanning** runs `carla-discovery-native` once per binary, in a separate
  process with a timeout. The results are cached in `plugins.json`.
  yabridge's `~/.vst/yabridge` and `~/.vst3/yabridge` folders are part of the
  standard search paths, and plugins found there are marked "yabridge".
* **Audio I/O.** Under JACK or PipeWire, the "input device" and "output
  device" are the JACK clients the chain is auto-connected to. The default
  is the physical `system` ports. Hardware MIDI (`system`, PipeWire
  `Midi-Bridge`, `a2j`) is connected to the chain's MIDI input. Sample rate
  and buffer size are requested with `PIPEWIRE_QUANTUM` under PipeWire, or
  passed to the driver under ALSA. A plain JACK server sets its own values.

## Status against the requirements

| Requirement | Status |
| --- | --- |
| Serial chain, audio + MIDI passed down the chain | Done (Carla patchbay mode, wired by the engine) |
| Parallel branches | Done: parallel blocks with any number of lanes, empty lanes are dry. Lanes are summed; Carla does not compensate latency between lanes, so lanes with different PDC drift apart by that delay. |
| Several chains at once (tracks) | Done: each track takes input 1+2, 1, 2 or nothing, and has volume, mute and a meter. All tracks receive hardware MIDI. |
| JACK / PipeWire, device, sample rate, buffer size | Done. PipeWire goes through pipewire-jack. |
| One-tap bypass | Done |
| Add / remove / reorder while running | Done, covered by the engine test |
| Plugin scan, remembered list | Done |
| yabridge plugins | Done (they appear as native plugins) |
| Named presets, default preset at startup | Done |
| Crash/hang isolation | Done with bridges. The crash is detected from Carla's error callbacks. |
| Level meters, DSP load, per-plugin PDC | Done. The meters read the input and output buses, and each track's bus. Total latency uses the longest path. |
| MIDI learn for bypass | Done: toggle or momentary, CC, note or program change, saved per preset |
| Top bar: track tabs with levels, app-drawn window controls (no WM decorations), docked when windowed, swipe-down from the top edge in fullscreen | Done. Moving and resizing go through winit's `drag_window` / `drag_resize_window`, so the window manager still snaps the window. |
| Left sidebar: edge swipe, hover tab, list, drag reorder (into and out of lanes), swipe/long-press remove with undo, browser with search/format/category/favorites, bypass, close gestures, 48 px+ targets | Done |
| Right sidebar: audio, presets, WiFi (NetworkManager), Bluetooth (BlueZ), display, system, on-screen keyboard toggle | Done. WiFi and Bluetooth are untested on real hardware. |
| On-screen text keyboard (WiFi passwords, names) | Done |
| Virtual piano → plugins | Done. Notes go to the selected plugin if it takes MIDI, otherwise to the first plugin that does. Single touch only. |
| Fullscreen default, windowed, F11, size remembered | Done. Real fullscreen needs a window manager (kiosk setup uses matchbox). |
| Editor shown in the main area, native resize | Done: editors are reparented into the main window, at their own size; resizable editors fill the area. Scaling was dropped. Sidebars and the track bar cut the editor down instead of hiding it, with a snapshot under them. See [editor-embedding.md](editor-embedding.md). |
| Generic parameter page for plugins without editors | Done |
| Dark theme, 60 fps sidebar slides that follow the finger | Done. The look and themes come from Xythos (`ui/theme.slint`, flat dark background; Jost in `assets/fonts/`). The theme gradients need the GPU renderer (femtovg); the software renderer draws gradient fills without rounded corners. |
| Kiosk mode | Example systemd and xinit files in `packaging/kiosk/` |
| AppImage | `packaging/appimage/build-appimage.sh` and a CI job on tags. The full AppImage build has not been tried yet. |
| CLAP | Needs Carla ≥ 2.6. Ubuntu ships 2.5.8, so bundle a newer Carla in the AppImage. |
