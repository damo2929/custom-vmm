//! Build-script support shared by the `*-sys` crates.
//!
//! §1.1's no-C-linkage rule was lifted because TLS, both codecs, zstd and
//! RADOS had no pure-Rust replacement. This crate is the single place that
//! knows how to find those libraries, so the two sys crates cannot drift
//! apart on include paths, link flags or bindgen configuration.
//!
//! Two host layouts are supported:
//!
//! * the -devel packages installed system-wide, which is what
//!   `HOST-REQUIREMENTS.md` §6 asks for and what a production build uses;
//! * a local sysroot staged by `scripts/setup-local-sysroot.sh` and named by
//!   `VMM_SYSROOT`, for hosts where installing packages is not possible.

pub use bindgen;

use std::path::{Path, PathBuf};

/// Where the C headers and link stubs live.
pub struct Sysroot {
    /// `$VMM_SYSROOT`, when a locally staged sysroot is in use.
    root: Option<PathBuf>,
}

impl Sysroot {
    /// Read `VMM_SYSROOT` and put its pkg-config directory on the search
    /// path. Call this once, first, from a build script.
    pub fn discover() -> Self {
        println!("cargo:rerun-if-env-changed=VMM_SYSROOT");
        println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

        let root = std::env::var_os("VMM_SYSROOT")
            .map(PathBuf::from)
            .filter(|p| p.is_dir());

        if let Some(root) = &root {
            let pkgconfig = root.join("usr/lib64/pkgconfig");
            let existing = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
            let combined = if existing.is_empty() {
                pkgconfig.display().to_string()
            } else {
                format!("{}:{existing}", pkgconfig.display())
            };
            // SAFETY: build scripts are single-threaded before any spawn.
            std::env::set_var("PKG_CONFIG_PATH", combined);
        }

        Sysroot { root }
    }

    /// Include directories to hand the C preprocessor, most specific first.
    pub fn include_dirs(&self) -> Vec<PathBuf> {
        match &self.root {
            Some(root) => vec![root.join("usr/include")],
            None => vec![PathBuf::from("/usr/include")],
        }
    }

    /// Emit the link flags for a library that ships no pkg-config file.
    ///
    /// Ceph is the case that needs this: `librados-devel` and `librbd-devel`
    /// install headers and an unversioned `.so` but no `.pc`.
    pub fn link_unpackaged(&self, lib: &str, header: &str) -> Result<PathBuf, String> {
        let dir = self
            .include_dirs()
            .into_iter()
            .find(|d| d.join(header).is_file())
            .ok_or_else(|| {
                format!(
                    "{header} not found. Install the -devel package that provides \
                     lib{lib}, or run scripts/setup-local-sysroot.sh."
                )
            })?;

        if let Some(root) = &self.root {
            println!(
                "cargo:rustc-link-search=native={}",
                root.join("usr/lib64").display()
            );
        }
        println!("cargo:rustc-link-lib=dylib={lib}");
        Ok(dir)
    }

    /// Probe a pkg-config module, emitting its link flags, and return the
    /// include directories it reported.
    pub fn probe(&self, module: &str, min_version: &str) -> Result<Vec<PathBuf>, String> {
        pkg_config::Config::new()
            .atleast_version(min_version)
            .probe(module)
            .map(|lib| lib.include_paths)
            .map_err(|e| {
                format!(
                    "pkg-config could not find {module} >= {min_version}: {e}\n\
                     Install the -devel package that provides it (see \
                     HOST-REQUIREMENTS.md §6), or run \
                     scripts/setup-local-sysroot.sh."
                )
            })
    }
}

/// Start a bindgen builder configured the way every binding in this tree
/// wants it: no layout tests, `core` types, and a resolvable clang.
///
/// The `-resource-dir` handling matters on Fedora, where `clang-libs` puts
/// `libclang.so` in `/usr/lib64` but its builtin headers — `stdarg.h`,
/// `stddef.h` — in `/usr/lib/clang/<major>`. libclang derives the resource
/// directory from its own path, so loaded out of `/usr/lib64` it looks in the
/// wrong place and every header that includes `<stdarg.h>` fails to parse.
pub fn bindgen_builder(includes: &[PathBuf]) -> bindgen::Builder {
    let mut builder = bindgen::Builder::default()
        .layout_tests(false)
        .derive_debug(true)
        .derive_default(true)
        .generate_comments(false)
        .use_core()
        .ctypes_prefix("::core::ffi")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for dir in includes {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }
    if let Some(dir) = clang_resource_dir() {
        builder = builder.clang_arg(format!("-resource-dir={}", dir.display()));
    }
    builder
}

/// Find clang's resource directory, or `None` when libclang can locate its
/// own (in which case overriding it would do harm rather than good).
fn clang_resource_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLANG_RESOURCE_DIR") {
        return Some(PathBuf::from(dir));
    }
    // Highest version wins, matching what the clang driver itself would pick.
    ["/usr/lib/clang", "/usr/lib64/clang"]
        .iter()
        .filter_map(|base| std::fs::read_dir(base).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join("include/stdarg.h").is_file())
        .max_by_key(|path| version_key(path))
}

/// Sort key for a versioned directory name, so `22` beats `9` and `21`.
fn version_key(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('.').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}
