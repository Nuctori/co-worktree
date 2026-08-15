//! Build script. When targeting Windows only: enable delay-loading of the
//! WinFsp DLL so the binary starts (and `cowt doctor` can report availability)
//! even when WinFsp is not installed.
//!
//! The flags are emitted by hand instead of calling
//! `winfsp::build::winfsp_link_delayload()`: the winfsp *build-dependency* is
//! only available on Windows hosts, but this script must compile everywhere
//! (cross-compile checks build it on Linux hosts too).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "winfsp-x64.dll",
        Ok("x86") => "winfsp-x86.dll",
        Ok("aarch64") => "winfsp-a64.dll",
        _ => return,
    };
    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => {
            println!("cargo:rustc-link-lib=dylib=delayimp");
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        Ok("gnu") => {
            println!("cargo:rustc-link-arg=-Wl,--delayload={dll}");
        }
        _ => {}
    }
}
