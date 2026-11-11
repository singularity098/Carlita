//! Raw, unsafe bindings to Carla's standalone host API (`CarlaHost.h`).
//!
//! Generated at build time by bindgen. Use the safe wrapper in
//! `carlita-engine` instead of calling these directly.
#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// `PLUGIN_CLAP` was added in Carla 2.6. It is defined here so the host can
/// request CLAP plugins when running against a newer Carla; older engines
/// simply report an error for it.
pub const PLUGIN_CLAP_COMPAT: PluginType = 14;
