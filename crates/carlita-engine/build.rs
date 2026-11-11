// Carla lives in a private folder (e.g. /usr/lib/carla); embed an rpath so
// tests find libcarla_standalone2.so without LD_LIBRARY_PATH.
fn main() {
    if let Ok(dir) = std::env::var("DEP_CARLA_STANDALONE2_LIBDIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    }
}
