# Plugin editors in the main window

Plugin editors are separate X11 windows: Carla shows them as top-level
windows, and with plugin bridges they live in another process. Carlita
shows them inside its own window by **reparenting** them. The code is in
`crates/carlita-x11` and is covered by a test on Xvfb.

Pixel-scaling small editors (an earlier prototype that captured the editor
off-screen with XComposite and forwarded synthetic input) was dropped
(decided 2026-11-11): editors are shown at their own size.

## How it works

1. Carla shows the selected plugin's editor (`carla_show_custom_ui`) with a
   unique window title, `carlita-editor-<slot>`
   (`carla_set_custom_ui_title`). Bridged plugins and yabridge editors are
   ordinary X11 windows too.
2. The host thread watches for new windows (SubstructureNotify on the root,
   plus a search every 100 ms) and finds the editor by its `_NET_WM_NAME`
   or `WM_NAME` once it is mapped. Some editors ignore the title Carla
   gives them, so the host notes which windows are already shown when the
   editor is asked for (the request goes to the host before Carla is told
   to show the editor). If no window has the title after 1.5 s, a window
   that appeared since then is taken instead, as long as it is the only
   one, is at least 32 × 32, and its `_NET_WM_PID` (when it has one) is
   Carlita's or one of its child processes' (plugin bridges, Wine).
3. If a window manager manages it, Carlita withdraws it first, as a program
   withdraws its own window (ICCCM 4.1.4): it unmaps the window and sends
   the window manager a synthetic `UnmapNotify`, then waits (up to 1 s) for
   the window manager to let go. Otherwise the window manager keeps treating
   the window as its own and may unmap or move it later.
4. Carlita creates a *holder* window, a child of the main window, and
   reparents the editor into it. The holder sits in the middle of the main
   area, as large as the editor but no larger than the area, so a larger
   editor is cut off at the right and bottom. Editors whose
   `WM_NORMAL_HINTS` allow resizing are resized to fill the area.
5. The editor draws itself and gets real pointer and touch input from the X
   server, so it works with every toolkit (Wine and yabridge included).
   Many toolkits move their window when they resize it (a plugin's own
   small/default/large menu, or after Carlita resized it), to where they
   were as a window of their own. Inside the holder that put the editor
   off-centre, so the host moves it back whenever it moves itself, and
   centres it again whenever it changes its own size. The holder's
   background is opaque Carlita grey: pixel 0, which it was, is see-through
   in a 32-bit (ARGB) main window, and the desktop showed through Carlita
   wherever the editor did not cover the holder.
6. An X11 child window is always above what Slint draws, so nothing can be
   drawn over an editor. The main area reaches the window's edges (below
   the docked track bar, above the keyboard), and the editor may lie under
   the edge strips. So the host keeps an invisible *input-only* window over
   each strip (`HostCommand::SetEdges`), stacked above every holder. X
   passes input on such a window to its parent, the main window, so
   swipes and taps on a strip reach Carlita even over an editor, while the
   editor still shows through. While the mouse hovers a strip, the holder is
   cut back by the width of the strip's tab (`uncovered-x` and friends in
   `ui/main.slint`), with the editor moved inside the holder so it stays
   where it is, and the see-through tab is drawn over the editor's
   snapshot. The holder also stops above the toast. While a sidebar, the
   fullscreen track bar, the plugin browser or a dialog is open, the holder
   is unmapped and the snapshot shows under them, so a tap outside a
   sidebar closes it rather than reaching the plugin.
7. Just before something starts to cover a fully shown editor, the host
   reads its pixels (`GetImage` on the editor window) and sends them as a
   `Snapshot`. The main area draws that picture where the editor is, so the
   sidebars, the track bar and dialogs slide over a picture of the plugin
   rather than an empty area. The picture does not update while covered
   (meters stop moving under it). If it cannot be read (an editor of an
   unusual pixel format, or partly off screen), the plugin's name shows
   instead.
8. When the plugin is deselected, or the app closes, the editor is unmapped
   and reparented back to the root window before the holder (or the main
   window) is destroyed. A destroyed editor window would kill the plugin, or
   its bridge, with an X error.

## Limits

* **X11 only.** Under a pure Wayland session there is no main X11 window to
  embed into, and editors open in their own windows. XWayland works.
* **No scaling by Carlita.** A small editor stays small; a larger one is
  cut off. Plugins are told the desktop's scale times the *plugin editor
  size* picked in Display options (`ENGINE_OPTION_FRONTEND_UI_SCALE`, and
  `CARLA_BRIDGE_UI_SCALE` for the patched bridge); changing it restarts the
  engine, since plugins read it when they start. Editors that size their
  controls to the window (Xythos, for one, shrinks below 900 × 560 logical
  pixels) are still limited by the area they are given.
* **Nothing draws over an editor.** Overlays cut it down or hide it, with
  a still snapshot under them. The plugin is not usable while a sidebar is
  open.
* **Window not found** within 15 s: Carlita closes the editor (so it is not
  left behind as a window of its own) and shows the generic parameter page
  instead.
* **Keyboard focus** is not handed to the editor; an editor takes it when
  clicked (yabridge's when the mouse enters them). The host gives it back
  to the main window when the pointer leaves the editor, when the editor is
  hidden or closed, and whenever the focus would otherwise be lost, so
  typing reaches Carlita's text fields again. winit drops every key while
  the main window does not have the focus.
* **F11** is taken from the main window with a passive key grab, so it
  toggles fullscreen even while an editor has the keyboard focus (which
  would otherwise keep it).
