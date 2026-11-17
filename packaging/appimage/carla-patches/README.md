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
