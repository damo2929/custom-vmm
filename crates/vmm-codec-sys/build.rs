//! Locate and bind the codec libraries §7.1 needs.
//!
//! Four bindings, kept in separate modules so a link error names the library
//! that is actually missing:
//!
//! * `ffmpeg` — libavcodec/libavutil/libswscale: H.264 decode for the client,
//!   and the colour conversion both ends need.
//! * `x264` — the software H.264 encoder, used when VA-API is absent. Reached
//!   through a small C shim in `shim/`, because bindgen cannot generate
//!   x264's parameter struct; see `shim/vmm_x264.h` for the full reason.
//! * `vorbis` — libvorbis/libvorbisenc: the audio encoder.
//! * `va` — libva/libva-drm: hardware H.264 encode.

use std::path::PathBuf;
use vmm_sysdeps::{bindgen_builder, Sysroot};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrappers");
    println!("cargo:rerun-if-changed=shim");

    let sysroot = Sysroot::discover();
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));

    // Versions are the oldest this tree is known to build against, not the
    // newest available: pinning to what Fedora ships would break EL hosts.
    let ffmpeg = probe_all(
        &sysroot,
        &[
            ("libavcodec", "58"),
            ("libavutil", "56"),
            ("libswscale", "5"),
        ],
    );
    let x264 = probe_all(&sysroot, &[("x264", "0.155")]);
    let vorbis = probe_all(
        &sysroot,
        &[("vorbisenc", "1.3"), ("vorbis", "1.3"), ("ogg", "1.3")],
    );
    let va = probe_all(&sysroot, &[("libva", "1.8"), ("libva-drm", "1.8")]);

    generate("ffmpeg", &ffmpeg, &out, |b| {
        b.allowlist_function("av_.*")
            .allowlist_function("avcodec_.*")
            .allowlist_function("sws_.*")
            .allowlist_type("AV.*")
            .allowlist_type("Sws.*")
            .allowlist_var("AV_.*")
            .allowlist_var("SWS_.*")
            .allowlist_var("FF_.*")
    });
    build_x264_shim(&x264);
    generate("x264", &["shim".into()], &out, |b| {
        b.allowlist_function("vmm_x264_.*")
            .allowlist_type("vmm_x264_.*")
            .allowlist_var("VMM_X264_.*")
    });
    generate("vorbis", &vorbis, &out, |b| {
        b.allowlist_function("vorbis_.*")
            .allowlist_function("ogg_.*")
            .allowlist_type("vorbis_.*")
            .allowlist_type("ogg_.*")
            .allowlist_var("OV_.*")
    });
    generate("va", &va, &out, |b| {
        b.allowlist_function("va.*")
            .allowlist_type("VA.*")
            .allowlist_var("VA_.*")
            .allowlist_var("VAProfile.*")
    });
}

/// Compile the x264 shim and link it into this crate.
fn build_x264_shim(includes: &[PathBuf]) {
    let mut build = cc::Build::new();
    build
        .file("shim/vmm_x264.c")
        .include("shim")
        .warnings(true)
        .flag_if_supported("-Wextra")
        .flag_if_supported("-Wno-unused-parameter");
    for dir in includes {
        build.include(dir);
    }
    build.compile("vmm_x264_shim");
}

/// Probe every module of one library, returning the union of their include
/// directories. A failure here is fatal and says which package is missing.
fn probe_all(sysroot: &Sysroot, modules: &[(&str, &str)]) -> Vec<PathBuf> {
    let mut includes = sysroot.include_dirs();
    for (module, min) in modules {
        match sysroot.probe(module, min) {
            Ok(dirs) => includes.extend(dirs),
            Err(e) => panic!("{e}"),
        }
    }
    includes.sort();
    includes.dedup();
    includes
}

fn generate<F>(name: &str, includes: &[PathBuf], out: &std::path::Path, configure: F)
where
    F: FnOnce(vmm_sysdeps::bindgen::Builder) -> vmm_sysdeps::bindgen::Builder,
{
    let header = format!("wrappers/{name}.h");
    let builder = bindgen_builder(includes).header(&header);
    let bindings = configure(builder)
        .generate()
        .unwrap_or_else(|e| panic!("generating bindings for {name} from {header}: {e}"));
    bindings
        .write_to_file(out.join(format!("{name}.rs")))
        .unwrap_or_else(|e| panic!("writing {name}.rs: {e}"));
}
