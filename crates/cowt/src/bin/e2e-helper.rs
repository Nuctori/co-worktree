//! Helper executable for the E2E suite: acts as the "application under
//! isolation" (`cowt run <id> -- cowt-e2e-helper sleep 30`). Kept separate
//! from the test binary because libtest owns the test binary's main.

use std::process::exit;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("sleep") => {
            let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
        Some("crash") => {
            let path = args.next().expect("crash needs a file path");
            std::fs::write(&path, "crash-data\n").expect("write crash file");
            // Hard abort, no cleanup — simulates an app killed by a crash.
            std::process::abort();
        }
        Some("perf") => {
            use std::io::Write;
            // Sequential write: <dir> <file> <count-4MiB-blocks>; prints ms.
            let dir = std::path::PathBuf::from(args.next().expect("perf needs a dir"));
            let file = args.next().expect("perf needs a file name");
            let blocks: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(128);
            let start = std::time::Instant::now();
            let mut f = std::fs::File::create(dir.join(&file)).expect("create perf file");
            let buf = vec![0u8; 4 * 1024 * 1024];
            for _ in 0..blocks {
                f.write_all(&buf).expect("write");
                f.sync_data().expect("fsync");
            }
            f.sync_all().expect("final fsync");
            drop(f);
            println!("PERF_MS={}", start.elapsed().as_millis());
        }
        _ => {
            eprintln!("usage: cowt-e2e-helper <sleep SECS | crash PATH | perf DIR FILE BLOCKS>");
            exit(2);
        }
    }
}
