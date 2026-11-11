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
   or `WM_NAME` once it is mapped.
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
6. An X11 child window is always above what Slint draws. So while a
   sidebar, the fullscreen track bar, the plugin browser, a dialog or a toast
   covers the main area, the holder is unmapped and the main area shows the
   plugin's name (`editor-covered` in `ui/main.slint`). In fullscreen the
   area starts below the top edge strip so the swipe that brings in the
   track bar still works.
7. When the plugin is deselected, or the app closes, the editor is unmapped
   and reparented back to the root window before the holder (or the main
   window) is destroyed. A destroyed editor window would kill the plugin, or
   its bridge, with an X error.

## Limits

* **X11 only.** Under a pure Wayland session there is no main X11 window to
  embed into, and editors open in their own windows. XWayland works.
* **No scaling.** A small editor stays small; a larger one is cut off.
* **Nothing draws over an editor.** Overlays hide it instead.
* **Window not found** within 15 s: Carlita shows the generic parameter page
  instead.
* **Keyboard focus** is not handed to the editor.
