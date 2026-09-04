//! What happens after a crash, Zed's `reliability.rs`: on the next launch, every
//! `<session>.dmp` + `<session>.json` pair the crash server left in the logs folder is uploaded
//! through the telemetry transport and deleted, provided crash reports are on. A report without
//! a commit SHA is deleted unsent: nothing could symbolicate it.
//!
//! The pair is written by `majik_crashes::crash_server` regardless of the setting; the setting
//! only gates the upload, so turning it on later sends what was kept.

use crate::telemetry::Telemetry;
use majik_crashes::CrashInfo;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the launch found: the panics it sent (their messages, for the toast) and whether any
/// report was kept on disk instead.
#[derive(Debug, Default, PartialEq)]
pub struct CrashUploadOutcome {
    pub uploaded: usize,
    pub kept: usize,
}

/// Upload every crash report in `logs_dir`. Blocking IO and HTTP: call it off the UI thread.
pub fn upload_previous_minidumps(telemetry: &Arc<Telemetry>, logs_dir: &Path, diagnostics_enabled: bool) -> CrashUploadOutcome {
    let mut outcome = CrashUploadOutcome::default();
    for (dump_path, json_path) in crash_reports_in(logs_dir) {
        let metadata = match std::fs::read(&json_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(target: "majik", "reading {}: {e}", json_path.display());
                outcome.kept += 1;
                continue;
            }
        };
        let Ok(info) = serde_json::from_slice::<CrashInfo>(&metadata) else {
            tracing::warn!(target: "majik", "{} is not a crash report; leaving it", json_path.display());
            outcome.kept += 1;
            continue;
        };
        if !diagnostics_enabled {
            outcome.kept += 1;
            continue;
        }
        if info.init.commit_sha.is_none() {
            tracing::warn!(target: "majik", "dropping a crash report from a build without a commit: nothing could read it");
            remove_pair(&dump_path, &json_path);
            continue;
        }
        let minidump = std::fs::read(&dump_path).unwrap_or_default();
        match telemetry.send_crash(metadata, minidump) {
            Ok(()) => {
                majik_telemetry::event!(
                    "Minidump Uploaded",
                    panic_message = info.panic.as_ref().map(|panic| panic.message.clone()),
                    crashed_version = info.init.app_version,
                    commit_sha = info.init.commit_sha,
                );
                remove_pair(&dump_path, &json_path);
                outcome.uploaded += 1;
            }
            Err(e) => {
                tracing::warn!(target: "majik", "uploading the crash report {}: {e:#}", json_path.display());
                outcome.kept += 1;
            }
        }
    }
    outcome
}

/// The `(dump, json)` pairs in `logs_dir`. A dump without its report is worthless and is left
/// alone; a report whose dump failed to write has an empty dump beside it and is still sent.
fn crash_reports_in(logs_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(logs_dir) else { return Vec::new() };
    let mut pairs: Vec<(PathBuf, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "dmp"))
        .filter_map(|dump| {
            let json = dump.with_extension("json");
            json.exists().then_some((dump, json))
        })
        .collect();
    pairs.sort();
    pairs
}

fn remove_pair(dump_path: &Path, json_path: &Path) {
    for path in [dump_path, json_path] {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!(target: "majik", "removing {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::TelemetryRequest;
    use crate::test_support::env;
    use gpui::TestAppContext;
    use majik_crashes::{CrashPanic, InitCrashHandler};

    fn write_report(dir: &Path, session: &str, commit_sha: Option<&str>) -> (PathBuf, PathBuf) {
        let info = CrashInfo {
            init: InitCrashHandler {
                session_id: session.into(),
                app_version: "0.1.0".into(),
                binary: "majik".into(),
                release_channel: "stable".into(),
                commit_sha: commit_sha.map(String::from),
            },
            panic: Some(CrashPanic { message: "boom".into(), span: "src/main.rs:1".into() }),
            minidump_error: None,
            abort_message: None,
            active_gpu: None,
        };
        let dump = dir.join(session).with_extension("dmp");
        let json = dir.join(session).with_extension("json");
        std::fs::write(&dump, b"minidump bytes").unwrap();
        std::fs::write(&json, serde_json::to_vec(&info).unwrap()).unwrap();
        (dump, json)
    }

    #[gpui::test]
    fn reports_are_uploaded_and_deleted(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let dir = tempfile::tempdir().unwrap();
        let (dump, json) = write_report(dir.path(), "session-a", Some("abc123"));
        let outcome = upload_previous_minidumps(&e.telemetry, dir.path(), true);
        assert_eq!(outcome, CrashUploadOutcome { uploaded: 1, kept: 0 });
        assert!(!dump.exists() && !json.exists(), "the pair is gone once sent");
        let requests = e.transport.requests();
        assert_eq!(requests.len(), 1);
        let TelemetryRequest::Crash { metadata, minidump } = &requests[0] else { panic!("a crash request") };
        assert_eq!(minidump, b"minidump bytes");
        let info: CrashInfo = serde_json::from_slice(metadata).unwrap();
        assert_eq!(info.panic.unwrap().message, "boom");
        cx.run_until_parked();
        let uploaded = e.events_named("Minidump Uploaded");
        assert_eq!(uploaded.len(), 1);
        assert_eq!(uploaded[0].event_properties["panic_message"], "boom");
        assert_eq!(uploaded[0].event_properties["commit_sha"], "abc123");
    }

    #[gpui::test]
    fn reports_are_kept_while_crash_reports_are_off(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let dir = tempfile::tempdir().unwrap();
        let (dump, json) = write_report(dir.path(), "session-a", Some("abc123"));
        let outcome = upload_previous_minidumps(&e.telemetry, dir.path(), false);
        assert_eq!(outcome, CrashUploadOutcome { uploaded: 0, kept: 1 });
        assert!(dump.exists() && json.exists(), "kept for a later launch with the switch on");
        assert!(e.transport.requests().is_empty());
    }

    #[gpui::test]
    fn a_failed_upload_keeps_the_report(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        e.transport.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let (dump, json) = write_report(dir.path(), "session-a", Some("abc123"));
        let outcome = upload_previous_minidumps(&e.telemetry, dir.path(), true);
        assert_eq!(outcome, CrashUploadOutcome { uploaded: 0, kept: 1 });
        assert!(dump.exists() && json.exists());
    }

    #[gpui::test]
    fn a_report_without_a_commit_is_dropped_unsent(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let dir = tempfile::tempdir().unwrap();
        let (dump, json) = write_report(dir.path(), "session-a", None);
        let outcome = upload_previous_minidumps(&e.telemetry, dir.path(), true);
        assert_eq!(outcome, CrashUploadOutcome::default());
        assert!(!dump.exists() && !json.exists());
        assert!(e.transport.requests().is_empty());
    }

    #[gpui::test]
    fn a_dump_without_its_report_and_a_missing_folder_are_left_alone(cx: &mut TestAppContext) {
        let e = env(cx, 0, "Mock");
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join("orphan.dmp");
        std::fs::write(&orphan, b"x").unwrap();
        assert_eq!(upload_previous_minidumps(&e.telemetry, dir.path(), true), CrashUploadOutcome::default());
        assert!(orphan.exists());
        assert_eq!(upload_previous_minidumps(&e.telemetry, &dir.path().join("nowhere"), true), CrashUploadOutcome::default());
    }
}
