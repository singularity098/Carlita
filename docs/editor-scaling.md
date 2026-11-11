# Plugin editor hosting and scaling

The requirements flag this as the riskiest part. The approach is implemented
in `crates/carlita-x11` and covered by an automated test on Xvfb.

## How it works

1. Carla shows the selected plugin's editor (`carla_show_custom_ui`) with a
   unique window title, `carlita-editor-<slot>`
   (`carla_set_custom_ui_title`). Bridged plugins (separate processes, used
   for crash isolation) and yabridge editors are ordinary X11 windows too.
2. The capture thread finds the window by its `_NET_WM_NAME` or `WM_NAME`.
   It then moves the window off-screen (x = −16000) and redirects it with
   **XComposite**, so the window keeps rendering into an off-screen pixmap.
3. **XDamage** reports changes. A frame is copied (BGRX → RGBA) only when
   something changed, at most every 16 ms, plus a refresh every 500 ms for GL
   editors that report no damage. The frame becomes a Slint `Image` and is
   drawn in the main area. Because Slint draws it, the sidebars, toasts and
   on-screen keyboards can slide over the editor.
4. `ScaleTransform` (in `carlita-core`) places the editor in one of these modes:
   * **Off**: native size, shrunk only if it does not fit.
   * **Fit**: as large as possible, aspect ratio kept.
   * **Fixed**: 100 %, 150 %, 200 % or 300 %.

   The mode can be set per plugin (Options → Display) and is saved with the
   preset. The global default lives in settings.
5. Touches are mapped back with `ScaleTransform::to_editor`. They are
   delivered to the deepest child window under that point as synthetic
   `ButtonPress`, `MotionNotify` and `ButtonRelease` events (`XSendEvent`).
   Scrolling becomes buttons 4 and 5.
6. If an editor's `WM_NORMAL_HINTS` say it can be resized (min size ≠ max
   size) and the mode is *Fit*, the window is resized to the area instead of
   being pixel-scaled, so it renders sharply at full size.

## Known risks and fallbacks

* **Synthetic events.** Some toolkits ignore events with `send_event` set,
  and Wine may be one of them. Options → Display → *Touch forwarding* →
  **XTest** switches to real input. While a finger is down, the editor is
  raised at the screen origin, unscaled, and real pointer events are
  injected. This works everywhere but shows the unscaled editor during the
  gesture. **Next step:** test both modes against a real yabridged plugin
  (for example a Wine VST3) on the target hardware.
* **Window not found** within 15 s: Carlita shows the generic parameter page
  instead.
* **No X server or no Composite** (pure Wayland): capture is disabled and
  editors open in their own windows. Running under XWayland works.
* **Throughput.** Frames are copied with `GetImage`, which is fine for typical
  editors (≤ 1000×700 at 60 fps on a mini PC). Switching to MIT-SHM or a
  GL texture-from-pixmap path is the obvious optimisation if needed.
* **Multi-touch** is not forwarded; editors get a single pointer.
