# Patches to Carla's plugin bridge

`packaging/build-carla.sh` applies these to the Carla it builds. When
`build-appimage.sh` bundles a Carla from `CARLA_PREFIX` instead, it
rebuilds `carla-bridge-native` from that Carla version with these patches.

- `bridge-ui-scale.patch`: Carla does not hand its UI scale
  (`ENGINE_OPTION_FRONTEND_UI_SCALE`) to plugin bridges, so editors of
  plugins running in their own process were told a scale of 1.0 however
  the desktop is scaled. The patched bridge reads the scale from the
  `CARLA_BRIDGE_UI_SCALE` environment variable, which Carlita sets before
  starting the engine (bridges inherit its environment).
- `vst2-ui-scale.patch`: Carla tells every VST2 editor its UI scale
  (Studio One's `PreS`/`AeCs` call), even when the scale is 1.0. REAPER
  says nothing, and a plugin that is told nothing keeps its own scaling.
  Windows plugins under yabridge (which passes the call on) scale with
  Wine's DPI setting, and an explicit 1.0 overrides that, so they came out
  smaller than in REAPER (Superior Drummer, for one). The patched Carla
  only tells VST2 editors a scale other than 1.0. With `CARLA_PREFIX`, only
  the bridge is rebuilt, so this holds for plugins in their own process.
