//! Minimal safe wrappers over `carla-sys`. Everything here must only be used
//! from the engine thread.

use carla_sys as sys;
use carlita_core::{BinaryArch, PluginFormat};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;

/// Raw engine callback, copied out of Carla's callback context.
#[derive(Debug, Clone)]
pub struct RawCallback {
    pub action: u32,
    pub plugin_id: u32,
    pub value1: i32,
    pub value2: i32,
    pub valuef: f32,
    pub text: String,
}

pub(crate) fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("no interior NULs")
}

pub(crate) fn from_c(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: Carla returns valid NUL-terminated strings that live at
        // least until the next call into the same API.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

pub fn plugin_type(format: PluginFormat) -> sys::PluginType {
    match format {
        PluginFormat::Internal => sys::PLUGIN_INTERNAL,
        PluginFormat::Ladspa => sys::PLUGIN_LADSPA,
        PluginFormat::Lv2 => sys::PLUGIN_LV2,
        PluginFormat::Vst2 => sys::PLUGIN_VST2,
        PluginFormat::Vst3 => sys::PLUGIN_VST3,
        PluginFormat::Clap => sys::PLUGIN_CLAP_COMPAT,
    }
}

pub fn binary_type(arch: BinaryArch) -> sys::BinaryType {
    match arch {
        BinaryArch::Native => {
            if cfg!(target_pointer_width = "64") {
                sys::BINARY_POSIX64
            } else {
                sys::BINARY_POSIX32
            }
        }
        BinaryArch::Win32 => sys::BINARY_WIN32,
        BinaryArch::Win64 => sys::BINARY_WIN64,
    }
}

/// Folder that holds Carla's binaries (bridges, discovery tools).
pub fn library_folder() -> PathBuf {
    if let Some(p) = std::env::var_os("CARLA_LIB_DIR") {
        return PathBuf::from(p);
    }
    // SAFETY: plain getter returning a static string.
    PathBuf::from(from_c(unsafe { sys::carla_get_library_folder() }))
}

/// Folder with Carla's resources (internal plugin UIs etc.).
pub fn resources_folder() -> PathBuf {
    if let Some(p) = std::env::var_os("CARLA_RESOURCES_DIR") {
        return PathBuf::from(p);
    }
    let lib = library_folder();
    // Typical layouts: /usr/lib/carla -> /usr/share/carla/resources,
    // AppImage: usr/lib/carla -> usr/share/carla/resources.
    let candidate = lib.join("../../share/carla/resources");
    if candidate.exists() {
        return candidate;
    }
    PathBuf::from("/usr/share/carla/resources")
}

/// Path of the `carla-discovery-native` tool.
pub fn discovery_tool() -> PathBuf {
    if let Some(p) = std::env::var_os("CARLA_DISCOVERY") {
        return PathBuf::from(p);
    }
    library_folder().join("carla-discovery-native")
}

pub fn drivers() -> Vec<crate::DriverInfo> {
    let mut out = Vec::new();
    // SAFETY: global getters; returned arrays are NULL-terminated.
    unsafe {
        let n = sys::carla_get_engine_driver_count();
        for i in 0..n {
            let name = from_c(sys::carla_get_engine_driver_name(i));
            let mut devices = Vec::new();
            let list = sys::carla_get_engine_driver_device_names(i);
            if !list.is_null() {
                let mut j = 0;
                loop {
                    let d = *list.add(j);
                    if d.is_null() {
                        break;
                    }
                    devices.push(from_c(d));
                    j += 1;
                }
            }
            out.push(crate::DriverInfo { name, devices });
        }
    }
    out
}

/// The process-wide Carla host handle.
pub struct Host {
    h: sys::CarlaHostHandle,
}

// The handle is only used from the engine thread; it is created there too.
impl Host {
    pub fn new() -> Self {
        // SAFETY: returns Carla's static standalone handle.
        let h = unsafe { sys::carla_standalone_host_init() };
        Self { h }
    }

    /// Installs the engine callback. `sender` must outlive the host.
    pub fn set_callback(&self, sender: *const crossbeam_channel::Sender<RawCallback>) {
        // SAFETY: the pointer stays valid for the engine thread's lifetime.
        unsafe {
            sys::carla_set_engine_callback(self.h, Some(engine_callback), sender as *mut c_void)
        };
    }

    pub fn set_option(&self, option: sys::EngineOption, value: i32, text: Option<&str>) {
        let c = text.map(cstr);
        // SAFETY: valid handle; string copied by Carla.
        unsafe {
            sys::carla_set_engine_option(
                self.h,
                option,
                value,
                c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            )
        };
    }

    pub fn init(&self, driver: &str, client: &str) -> Result<(), String> {
        let (d, c) = (cstr(driver), cstr(client));
        // SAFETY: valid handle and strings.
        if unsafe { sys::carla_engine_init(self.h, d.as_ptr(), c.as_ptr()) } {
            Ok(())
        } else {
            Err(self.last_error())
        }
    }

    pub fn close(&self) {
        // SAFETY: valid handle.
        unsafe {
            sys::carla_set_engine_about_to_close(self.h);
            sys::carla_engine_close(self.h);
        }
    }

    pub fn idle(&self) {
        // SAFETY: valid handle.
        unsafe { sys::carla_engine_idle(self.h) }
    }

    pub fn last_error(&self) -> String {
        // SAFETY: valid handle.
        let e = from_c(unsafe { sys::carla_get_last_error(self.h) });
        if e.is_empty() {
            "unknown Carla error".into()
        } else {
            e
        }
    }

    pub fn plugin_count(&self) -> u32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_current_plugin_count(self.h) }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_plugin(
        &self,
        btype: sys::BinaryType,
        ptype: sys::PluginType,
        filename: &str,
        name: &str,
        label: &str,
        unique_id: i64,
    ) -> Result<(), String> {
        let f = (!filename.is_empty()).then(|| cstr(filename));
        let n = cstr(name);
        let l = cstr(label);
        // SAFETY: valid handle, strings copied by Carla.
        let ok = unsafe {
            sys::carla_add_plugin(
                self.h,
                btype,
                ptype,
                f.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                n.as_ptr(),
                l.as_ptr(),
                unique_id,
                std::ptr::null(),
                sys::PLUGIN_OPTIONS_NULL,
            )
        };
        if ok {
            Ok(())
        } else {
            Err(self.last_error())
        }
    }

    pub fn remove_plugin(&self, id: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_remove_plugin(self.h, id) }
    }

    pub fn remove_all(&self) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_remove_all_plugins(self.h) }
    }

    /// Makes the next `add_plugin` replace plugin `id` in place.
    pub fn replace_plugin(&self, id: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_replace_plugin(self.h, id) }
    }

    pub fn switch_plugins(&self, a: u32, b: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_switch_plugins(self.h, a, b) }
    }

    pub fn set_active(&self, id: u32, on: bool) {
        // SAFETY: valid handle.
        unsafe { sys::carla_set_active(self.h, id, on) }
    }

    pub fn set_drywet(&self, id: u32, value: f32) {
        // SAFETY: valid handle.
        unsafe { sys::carla_set_drywet(self.h, id, value) }
    }

    pub fn plugin_hints(&self, id: u32) -> u32 {
        // SAFETY: valid handle; struct read immediately.
        unsafe {
            let info = sys::carla_get_plugin_info(self.h, id);
            if info.is_null() {
                0
            } else {
                (*info).hints
            }
        }
    }

    pub fn audio_ports(&self, id: u32) -> (u32, u32) {
        // SAFETY: valid handle.
        unsafe {
            let p = sys::carla_get_audio_port_count_info(self.h, id);
            if p.is_null() {
                (0, 0)
            } else {
                ((*p).ins, (*p).outs)
            }
        }
    }

    pub fn midi_ports(&self, id: u32) -> (u32, u32) {
        // SAFETY: valid handle.
        unsafe {
            let p = sys::carla_get_midi_port_count_info(self.h, id);
            if p.is_null() {
                (0, 0)
            } else {
                ((*p).ins, (*p).outs)
            }
        }
    }

    pub fn parameter_count(&self, id: u32) -> u32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_parameter_count(self.h, id) }
    }

    pub fn parameter(&self, id: u32, index: u32) -> crate::ParameterInfo {
        // SAFETY: valid handle; returned structs are read immediately.
        unsafe {
            let info = sys::carla_get_parameter_info(self.h, id, index);
            let data = sys::carla_get_parameter_data(self.h, id, index);
            let ranges = sys::carla_get_parameter_ranges(self.h, id, index);
            let (name, unit) = if info.is_null() {
                (String::new(), String::new())
            } else {
                (from_c((*info).name), from_c((*info).unit))
            };
            let (hints, is_output) = if data.is_null() {
                (0, false)
            } else {
                ((*data).hints, (*data).type_ == sys::PARAMETER_OUTPUT)
            };
            let (min, max, def) = if ranges.is_null() {
                (0.0, 1.0, 0.0)
            } else {
                ((*ranges).min, (*ranges).max, (*ranges).def)
            };
            crate::ParameterInfo {
                index,
                name,
                unit,
                min,
                max,
                default: def,
                value: sys::carla_get_current_parameter_value(self.h, id, index),
                text: from_c(sys::carla_get_parameter_text(self.h, id, index)),
                is_boolean: hints & sys::PARAMETER_IS_BOOLEAN != 0,
                is_integer: hints & sys::PARAMETER_IS_INTEGER != 0,
                is_output: is_output || hints & sys::PARAMETER_IS_ENABLED == 0,
            }
        }
    }

    pub fn set_parameter(&self, id: u32, index: u32, value: f32) {
        // SAFETY: valid handle.
        unsafe { sys::carla_set_parameter_value(self.h, id, index, value) }
    }

    pub fn latency(&self, id: u32) -> u32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_plugin_latency(self.h, id) }
    }

    pub fn input_peak(&self, id: u32, left: bool) -> f32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_input_peak_value(self.h, id, left) }
    }

    pub fn output_peak(&self, id: u32, left: bool) -> f32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_output_peak_value(self.h, id, left) }
    }

    pub fn runtime_info(&self) -> (f32, u32) {
        // SAFETY: valid handle.
        unsafe {
            let i = sys::carla_get_runtime_engine_info(self.h);
            if i.is_null() {
                (0.0, 0)
            } else {
                ((*i).load, (*i).xruns)
            }
        }
    }

    pub fn sample_rate(&self) -> f64 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_sample_rate(self.h) }
    }

    pub fn buffer_size(&self) -> u32 {
        // SAFETY: valid handle.
        unsafe { sys::carla_get_buffer_size(self.h) }
    }

    pub fn save_state(&self, id: u32, path: &std::path::Path) -> bool {
        let p = cstr(&path.to_string_lossy());
        // SAFETY: valid handle.
        unsafe {
            sys::carla_prepare_for_save(self.h, id);
            sys::carla_save_plugin_state(self.h, id, p.as_ptr())
        }
    }

    pub fn load_state(&self, id: u32, path: &std::path::Path) -> bool {
        let p = cstr(&path.to_string_lossy());
        // SAFETY: valid handle.
        unsafe { sys::carla_load_plugin_state(self.h, id, p.as_ptr()) }
    }

    pub fn set_ui_title(&self, id: u32, title: &str) {
        let t = cstr(title);
        // SAFETY: valid handle.
        unsafe { sys::carla_set_custom_ui_title(self.h, id, t.as_ptr()) }
    }

    pub fn show_ui(&self, id: u32, show: bool) {
        // SAFETY: valid handle.
        unsafe { sys::carla_show_custom_ui(self.h, id, show) }
    }

    pub fn embed_ui(&self, id: u32, parent: u64) -> u64 {
        // SAFETY: valid handle; Carla treats the pointer as an X11 window id.
        unsafe {
            sys::carla_embed_custom_ui(self.h, id, parent as usize as *mut c_void) as usize as u64
        }
    }

    pub fn send_note(&self, id: u32, channel: u8, note: u8, velocity: u8) {
        // SAFETY: valid handle.
        unsafe { sys::carla_send_midi_note(self.h, id, channel, note, velocity) }
    }

    pub fn patchbay_refresh(&self) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_patchbay_refresh(self.h, true) }
    }

    pub fn patchbay_connect(&self, ga: u32, pa: u32, gb: u32, pb: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_patchbay_connect(self.h, true, ga, pa, gb, pb) }
    }

    pub fn patchbay_disconnect(&self, connection: u32) -> bool {
        // SAFETY: valid handle.
        unsafe { sys::carla_patchbay_disconnect(self.h, true, connection) }
    }
}

unsafe extern "C" fn engine_callback(
    ptr: *mut c_void,
    action: sys::EngineCallbackOpcode,
    plugin_id: sys::uint,
    value1: i32,
    value2: i32,
    _value3: i32,
    valuef: f32,
    value_str: *const c_char,
) {
    if ptr.is_null() {
        return;
    }
    // Skip the very chatty idle/debug callbacks.
    if action == sys::ENGINE_CALLBACK_IDLE || action == sys::ENGINE_CALLBACK_DEBUG {
        return;
    }
    // SAFETY: `ptr` is the Sender installed by `Host::set_callback`, which
    // outlives the engine. Sending never blocks (unbounded channel).
    let tx = unsafe { &*(ptr as *const crossbeam_channel::Sender<RawCallback>) };
    let _ = tx.send(RawCallback {
        action,
        plugin_id,
        value1,
        value2,
        valuef,
        text: from_c(value_str),
    });
}
