//! Build script. When targeting Windows only: enable delay-loading of the
//! WinFsp DLL so the binary starts (and `cowt doctor` can report availability)
//! even when WinFsp is not installed. The target OS comes from cargo's env
//! vars (the `cfg(windows)` check would fire on Windows *hosts* building for
//! Linux/macOS targets too).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winfsp::build::winfsp_link_delayload();
    }
}
