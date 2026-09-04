//! The crash path end to end: `majik --crash-test <dir>` brings up the crash server the way the
//! app does, then panics; the server writes the minidump and the report into `<dir>`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_panic_leaves_a_minidump_and_a_report() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_majik")).arg("--crash-test").arg(dir.path()).stdout(Stdio::null()).stderr(Stdio::null()).status().expect("the binary runs");
    assert!(!status.success(), "the process died: {status}");

    let report = dir.path().join("crash-test.json");
    let dump = dir.path().join("crash-test.dmp");
    wait_for(&report);
    let info: majik_crashes::CrashInfo = serde_json::from_slice(&std::fs::read(&report).unwrap()).expect("a crash report");
    assert_eq!(info.init.session_id, "crash-test");
    assert_eq!(info.init.binary, "majik");
    assert_eq!(info.init.app_version, env!("CARGO_PKG_VERSION"));
    let panic = info.panic.expect("the panic was recorded");
    assert_eq!(panic.message, "crash test");
    assert!(panic.span.contains("main.rs:"), "{}", panic.span);
    assert_eq!(info.minidump_error, None, "the dump was written");
    let bytes = std::fs::read(&dump).expect("a minidump beside the report");
    assert!(bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]), "zstd-compressed, {} bytes", bytes.len());
    assert!(!zstd::decode_all(bytes.as_slice()).unwrap().is_empty());
}

fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(started.elapsed() < Duration::from_secs(20), "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}
