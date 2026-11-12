# Patches to Carla's plugin bridge

`build-appimage.sh` rebuilds `carla-bridge-native` from the Carla release
it bundles, with these patches, and puts it in the AppImage in place of
Carla's own.

- `bridge-ui-scale.patch`: Carla does not hand its UI scale
  (`ENGINE_OPTION_FRONTEND_UI_SCALE`) to plugin bridges, so editors of
  plugins running in their own process were told a scale of 1.0 however
  the desktop is scaled. The patched bridge reads the scale from the
  `CARLA_BRIDGE_UI_SCALE` environment variable, which Carlita sets before
  starting the engine (bridges inherit its environment).
