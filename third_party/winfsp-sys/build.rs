// Vendored winfsp-sys: this build.rs skips bindgen entirely and copies the
// pre-generated bindings (src/bindings.rs, the same file docs.rs uses).
//
// Why: bindgen needs libclang + platform headers and is notoriously picky
// cross-compiling to Windows on Linux hosts (llvm's mmintrin.h conflicts
// with mingw-w64 headers; LIBCLANG_PATH pinning did not stick). The WinFsp
// API is stable; regenerating bindings on every build buys nothing.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    println!("cargo:rerun-if-changed=src/bindings.rs");
    fs::copy(
        manifest.join("src/bindings.rs"),
        out_dir.join("bindings.rs"),
    )
    .expect("copy pre-generated bindings.rs");

    // Link against the bundled import library, with delay-loading so the
    // binary starts without WinFsp installed (cowt doctor reports it).
    println!("cargo:rustc-link-search={}", manifest.join("winfsp/lib").to_string_lossy());
    println!("cargo:rustc-link-lib=dylib=winfsp-x64");
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
}
