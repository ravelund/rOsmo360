//! Bindings to the system's ffmpeg libraries for the `zero-copy` feature;
//! nothing without it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ROSMO360_LIBAV_STATIC");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    #[cfg(feature = "zero-copy")]
    bind();
}

#[cfg(feature = "zero-copy")]
fn bind() {
    let mut includes = Vec::new();
    for lib in ["libavformat", "libavcodec", "libavutil"] {
        // The release build links its own small ffmpeg in (packaging/libav.sh);
        // otherwise the system's, as a shared library.
        let found = pkg_config::Config::new()
            .statik(std::env::var_os("ROSMO360_LIBAV_STATIC").is_some())
            .probe(lib)
            .unwrap_or_else(|e| panic!("the zero-copy feature needs {lib}'s development files: {e}"));
        includes.extend(found.include_paths);
    }
    let bindings = bindgen::Builder::default()
        .header_contents("libav.h", "#include <libavformat/avformat.h>\n#include <libavcodec/avcodec.h>\n#include <libavutil/hwcontext.h>\n#include <libavutil/hwcontext_vulkan.h>\n#include <libavutil/pixdesc.h>\n")
        .clang_args(includes.iter().map(|p| format!("-I{}", p.display())))
        .allowlist_function("av_.*|avcodec_.*|avformat_.*")
        .allowlist_type("AV.*")
        .allowlist_var("AV_.*|AVERROR.*")
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("binding the ffmpeg libraries");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("libav.rs");
    bindings.write_to_file(out).expect("writing the ffmpeg bindings");
}
