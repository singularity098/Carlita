//! Generates Rust bindings for Carla's C host API.
//!
//! Headers and the library are located with pkg-config (`carla-standalone`).
//! Set `CARLA_INCLUDE_DIR` / `CARLA_LIB_DIR` to point at a custom Carla build
//! (for example the one bundled into the AppImage) instead.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=CARLA_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=CARLA_LIB_DIR");

    let mut include_paths: Vec<PathBuf> = Vec::new();

    if let (Ok(inc), Ok(lib)) = (env::var("CARLA_INCLUDE_DIR"), env::var("CARLA_LIB_DIR")) {
        let inc = PathBuf::from(inc);
        include_paths.push(inc.clone());
        include_paths.push(inc.join("includes"));
        include_paths.push(inc.join("utils"));
        println!("cargo:rustc-link-search=native={lib}");
        println!("cargo:rustc-link-lib=dylib=carla_standalone2");
        println!("cargo:libdir={lib}");
    } else {
        let lib = pkg_config::Config::new()
            .atleast_version("2.5")
            .probe("carla-standalone")
            .expect(
                "Carla development files not found. Install `carla-dev` (Debian/Ubuntu) \
                 or set CARLA_INCLUDE_DIR and CARLA_LIB_DIR.",
            );
        include_paths.extend(lib.include_paths);
        if let Some(dir) = lib.link_paths.first() {
            // Dependent crates read this as DEP_CARLA_STANDALONE2_LIBDIR to
            // add an rpath, because Carla installs into a private lib folder.
            println!("cargo:libdir={}", dir.display());
        }
    }

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        // Carla's headers are C-compatible when compiled as C.
        .clang_arg("-xc")
        .allowlist_function("carla_.*")
        .allowlist_type("Carla.*|Engine.*|Plugin.*|Binary.*|Parameter.*|MidiProgramData|CustomData|InternalParameterIndex|FileCallback.*|NsmCallbackOpcode")
        .allowlist_var("PLUGIN_.*|ENGINE_.*|PARAMETER_.*|CARLA_.*|PATCHBAY_.*|AUDIO_PORT_.*|CUSTOM_DATA_.*|MAX_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for p in &include_paths {
        builder = builder.clang_arg(format!("-I{}", p.display()));
    }

    let bindings = builder
        .generate()
        .expect("unable to generate Carla bindings");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(out)
        .expect("unable to write bindings");
}
