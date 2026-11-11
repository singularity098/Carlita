# Carlita - Requirements

## Overview

A touch-first plugin chain host for a pedalboard: Rust app, Carla engine, Slint UI, on a Linux mini PC with a ~12" touchscreen.

- **Purpose:** load and run a serial or parallel chain of audio plugins live; no recording, no timeline
- **Hardware:** small x86 Linux mini PC, ~12" touchscreen (as low as 1280×800), audio interface
- **Engine:** Carla backend (`libcarla_standalone2`) via its C API, with Rust bindings from `bindgen`
- **UI:** Slint, dark theme, GPU renderer
- **Plugins:** native Linux VST2, VST3, CLAP, LV2, and Windows plugins through yabridge (yabridge managed by the user separately)
- **Display server:** X11 (or XWayland), because plugin editors are X11 windows
- **License:** GPL-2.0-or-later compatible, to match Carla
- **Packaging:** Available as an appimage

## Audio and plugin hosting

The engine runs several tracks at once, each a chain that may contain parallel blocks, glitch-free, with plugin changes made live.

- Run plugins in a serial or parallel chain: input → plugin 1 → plugin 2 → … → output
- Parallel blocks: split the signal into lanes (each a serial chain, an empty lane is the dry signal) and mix them back together
- Several tracks (chains) play at the same time, each fed by input 1+2, input 1, input 2 or no input, with its own volume and mute
- Supports both audio and MIDI input and output.  One plugin may send MIDI or audio down the chain to the next plugin which receives it
- Audio through JACK / PipeWire (native pipewire support); choose input and output device, sample rate and buffer size
- Bypass any plugin with one tap without removing it
- Add, remove and reorder plugins while audio keeps running, without dropouts
- Scan for plugins and remember the plugin list between launches
- Load yabridge-bridged Windows plugins like any native plugin
- Save and load the whole chain, including every plugin's state, as named presets, and set a default preset when we load the application
- A plugin that crashes or hangs must not take down the app or the audio for the rest of the chain
- Show input and output level meters and a CPU / DSP load readout, as well as PDC per plugin
- Allow the user to MIDI learn to enable or bypass certain plugins

## Top bar: tracks and window controls

The tracks are picked from a bar along the top of the screen, which also holds the window controls Carlita draws itself.

- Browser-style tabs, one per track, with a level bar and a mute badge, and a "+" tab to add a track
- Tapping a tab shows that track's chain and the plugin last shown for it; tapping the selected tab opens the chain sidebar
- Windowed: the bar is docked at the top and replaces the window manager's title bar (drag to move, double-tap to maximize, fullscreen / minimize / maximize / close buttons, resize from the window border)
- Fullscreen: the bar is hidden and slides down with a swipe from the top edge (or the tab that shows while the mouse hovers there); it closes with a swipe up, a tap outside, or after picking a track

## Left sidebar: the chain

Swiping in from the left edge opens the chain, where plugins are added, removed, reordered and selected.

- Opens with a swipe from a host-owned strip on the left screen edge (works even over a plugin editor)
- Also opens from a visible tab or button, for windowed mode and mouse use, but these are only visible when the mouse is hovering to the side so that VST can be displayed in 100% fullscreen normally
- The selected track's name (tap to rename), input, mute, volume and delete
- Lists the chain in order, parallel lanes indented; tap a plugin to show its editor in the main area
- Drag a plugin to reorder it, including into and out of parallel lanes; split a plugin into a parallel block and add or remove lanes
- Swipe a plugin sideways or long-press it to remove it, with a short undo
- An "Add plugin" button opens a searchable list, filterable by format and category, with favorites
- Per-plugin bypass toggle visible in the list
- Closes with a swipe back, a tap outside, or after selecting a plugin
- Large touch targets (at least ~10 mm / about 48 px)

## Right sidebar: options

Swiping in from the right edge opens settings, including WiFi and Bluetooth.

- Opens with a swipe from a host-owned strip on the right screen edge, or a visible button
- **Audio:** device, sample rate, buffer size, current latency
- **Presets:** save, save as, load, rename, delete
- **WiFi:** on/off toggle, list of networks, connect with an on-screen keyboard (via NetworkManager over D-Bus)
- **Bluetooth:** on/off toggle, list of paired and nearby devices, pair and connect (via BlueZ over D-Bus)
- **Display:** fullscreen / windowed, brightness if the hardware supports it, default plugin editor scaling
- **System:** app version, rescan plugins, restart app, shut down / reboot the PC
- Toggles apply immediately; no separate Save step
- Also has a button which can toggle on/off a virtual musical keyboard on the bottom of the screen, which may be used to send midi notes to the plugins

## Display and plugin editor scaling

The app runs fullscreen by default, and small legacy plugin editors can be scaled up to fill the screen.

- Starts fullscreen by default; can also run in a resizable window
- Toggle between fullscreen and windowed from the options sidebar or a keyboard shortcut
- Remember window size and mode between launches
- The selected plugin's editor fills the main area between the two sidebar strips
- **Scale to fit:** plugin editors smaller than the screen can be scaled up, keeping their aspect ratio
- Touches on a scaled editor are mapped back to the plugin's real coordinates, so knobs and buttons work
- Scaling can be set per plugin (off, fit to screen, or a fixed factor) and is remembered
- Plugins that support resizing or HiDPI natively use that instead of pixel scaling
- Plugins without an editor get a generic parameter page with large sliders

**Open question:** scaling a fixed-size X11 editor means redirecting its window off-screen (XComposite), drawing it scaled, and forwarding input (XTest). This is the riskiest requirement and should be prototyped early with a yabridged plugin.

## Look, feel and performance

The UI is a polished dark theme that responds instantly to touch and never disturbs the audio.

- Dark theme throughout, high contrast and readable on a stage
- Smooth sidebar slide animations at the display's full refresh rate (60 fps or better)
- Sidebars follow the finger while dragging, then snap open or closed
- Visible response to a touch within one frame
- UI work never blocks or glitches the audio thread
- App launches to a playable chain in a few seconds
- Light enough on CPU and memory to leave headroom for plugins on a mini PC
- Optional boot-straight-into-the-app kiosk mode

## Out of scope

- Multitrack recording, timelines or arrangement
- A free-form patchbay (the rig's structure is tracks, chains and parallel blocks)
- macOS and Windows builds
