# Architecture

```
            ┌──────────── UI thread (Slint event loop) ────────────┐
 touch ───► │ ui/*.slint ◄──► app.rs (App state, 16 ms poll timer) │
            └───▲───────────────▲──────────────▲────────────▲──────┘
      commands/ │ events  frames│/input   D-Bus│      MIDI  │ scan results
    ┌───────────┴──┐  ┌─────────┴───┐  ┌───────┴──────┐ ┌───┴────┐ ┌──────────┐
    │ engine thread│  │ x11 capture │  │ system worker│ │ midir  │ │ scanner  │
    │ Carla host   │  │ XComposite  │  │ NM/BlueZ/    │ │ (learn)│ │ carla-   │
    │ (rack mode)  │  │ XDamage     │  │ logind       │ │        │ │ discovery│
    └──────┬───────┘  └─────────────┘  └──────────────┘ └────────┘ └──────────┘
           │ Carla owns the realtime audio thread and plugin bridge processes
```

* **The UI never blocks on audio work.** Every Carla call happens on the engine
  thread (`carlita-engine`). The UI sends `Command`s and polls `Event`s.
  Loading a preset adds one plugin per engine tick, so the UI keeps
  updating while it loads.
* **Live chain.** Carla runs in *continuous rack* mode. Plugin *i* in the
  chain is Carla plugin id *i*, and plugins are processed in series with
  audio and MIDI passed along. Adding, removing and moving plugins
  (`carla_switch_plugins`) happen while audio keeps running.
* **Bypass.** Plugins that can mix dry and wet are set fully dry, so audio
  passes straight through. Plugins that cannot (synths, mono→stereo) are
  deactivated.
* **Crash isolation.** With *Isolate plugins* on (the default), every plugin
  runs in a Carla bridge process. When a bridge crashes or hangs, the slot is
  replaced in place by a unity-gain pass-through (`audiogain_s`), so Carla's
  ids stay aligned and the rest of the chain keeps sounding. The slot shows
  "Crashed – tap to reload", and reloading restores its last known state.
  Plugins that fail to load get the same placeholder.
* **Presets** are JSON files that contain the whole chain, each plugin's
  Carla state (`.carxs` XML from `carla_save_plugin_state`), per-plugin
  scaling and MIDI-learn mappings. File writes are atomic.
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
| Serial chain, audio + MIDI passed down the chain | Done (Carla rack mode) |
| Parallel branches | **Not done.** Rack mode is serial only. Parallel paths need Carla's patchbay mode, and the requirements also list "splits or a patchbay" as out of scope, so this needs a decision. |
| JACK / PipeWire, device, sample rate, buffer size | Done. PipeWire goes through pipewire-jack. |
| One-tap bypass | Done |
| Add / remove / reorder while running | Done, covered by the engine test |
| Plugin scan, remembered list | Done |
| yabridge plugins | Done (they appear as native plugins) |
| Named presets, default preset at startup | Done |
| Crash/hang isolation | Done with bridges. The crash is detected from Carla's error callbacks. |
| Level meters, DSP load, per-plugin PDC | Done. Meters read the first plugin's input and the last plugin's output, so they show nothing while the chain is empty. |
| MIDI learn for bypass | Done: toggle or momentary, CC, note or program change, saved per preset |
| Left sidebar: edge swipe, hover tab, list, drag reorder, swipe/long-press remove with undo, browser with search/format/category/favorites, bypass, close gestures, 48 px+ targets | Done |
| Right sidebar: audio, presets, WiFi (NetworkManager), Bluetooth (BlueZ), display, system, on-screen keyboard toggle | Done. WiFi and Bluetooth are untested on real hardware. |
| On-screen text keyboard (WiFi passwords, names) | Done |
| Virtual piano → plugins | Done. Notes go to the selected plugin if it takes MIDI, otherwise to the first plugin that does. Single touch only. |
| Fullscreen default, windowed, F11, size remembered | Done. Real fullscreen needs a window manager (kiosk setup uses matchbox). |
| Editor fills the main area, scaling, input mapping, per-plugin setting, native resize | Prototype done. See [editor-scaling.md](editor-scaling.md). |
| Generic parameter page for plugins without editors | Done |
| Dark theme, 60 fps sidebar slides that follow the finger | Done |
| Kiosk mode | Example systemd and xinit files in `packaging/kiosk/` |
| AppImage | `packaging/appimage/build-appimage.sh` and a CI job on tags. The full AppImage build has not been tried yet. |
| CLAP | Needs Carla ≥ 2.6. Ubuntu ships 2.5.8, so bundle a newer Carla in the AppImage. |
