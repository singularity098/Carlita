fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint UI failed to compile");

    // Carla lives in a private lib folder; embed an rpath for dev builds.
    // The AppImage sets its own ($ORIGIN-relative) path.
    if let Ok(dir) = std::env::var("DEP_CARLA_STANDALONE2_LIBDIR") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{dir}");
    }
}
