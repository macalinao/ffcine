//! Generates bindings for the libplacebo headers in `wrapper.h`, located via pkg-config. With the
//! `libav` feature, also compiles libplacebo's header-only libav helpers (`src/libav.c`).

use std::env;
use std::path::PathBuf;

// pl_vulkan_queue.flags (ffmpeg 9's internally synchronized queues on NVIDIA).
const MIN_VERSION: &str = "7.365";

fn main() {
    println!("cargo:rerun-if-changed=wrapper.h");
    let lib = pkg_config::Config::new()
        .atleast_version(MIN_VERSION)
        .probe("libplacebo")
        .unwrap_or_else(|e| panic!("libplacebo >= {MIN_VERSION} not found via pkg-config: {e}"));

    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .allowlist_function("pl_.*")
        .allowlist_type("pl_.*")
        .allowlist_var("pl_.*|PL_.*")
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .derive_default(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for dir in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    builder
        .generate()
        .expect("bindgen libplacebo")
        .write_to_file(out.join("bindings.rs"))
        .expect("write bindings.rs");

    #[cfg(feature = "libav")]
    build_libav(&lib);
}

#[cfg(feature = "libav")]
fn build_libav(placebo: &pkg_config::Library) {
    println!("cargo:rerun-if-changed=src/libav.c");
    let mut cc = cc::Build::new();
    cc.file("src/libav.c").includes(&placebo.include_paths);
    for name in ["libavutil", "libavcodec", "libavformat", "vulkan"] {
        // Linking is ffmpeg-sys-next's (libav*) and libplacebo's (vulkan) job; only headers here.
        let l = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe(name)
            .unwrap_or_else(|e| panic!("{name} not found via pkg-config: {e}"));
        cc.includes(&l.include_paths);
    }
    cc.warnings(false).compile("placebo_libav");
}
