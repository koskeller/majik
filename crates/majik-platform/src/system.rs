//! What machine this is, for the metadata a telemetry batch and a crash report carry (Zed's
//! `os_name` / `os_version` in `client::telemetry`). Never anything that identifies the machine
//! or the user: the OS family, its version, and nothing else.

/// `"macOS"`, `"Windows"`, or `"Linux Wayland"` / `"Linux X11"` / `"Linux"`: the compositor
/// matters to a GPUI app, and gpui's own guess (`guess_compositor`) only exists on Linux, so the
/// same environment check lives here where it compiles everywhere.
pub fn os_name() -> String {
    if cfg!(target_os = "macos") {
        "macOS".to_owned()
    } else if cfg!(target_os = "windows") {
        "Windows".to_owned()
    } else {
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        if set("WAYLAND_DISPLAY") {
            "Linux Wayland".to_owned()
        } else if set("DISPLAY") {
            "Linux X11".to_owned()
        } else {
            "Linux".to_owned()
        }
    }
}

/// The OS version, e.g. `15.6.1`, `10.0.26100`, `ubuntu 24.04`. May do blocking IO: call it off
/// the UI thread. This crate's own tests get a constant, since asking macOS is slow.
pub fn os_version() -> String {
    if cfg!(test) {
        return "test binary".to_owned();
    }
    os_version_impl()
}

#[cfg(target_os = "macos")]
fn os_version_impl() -> String {
    use objc2_foundation::NSProcessInfo;
    // "Version 15.6.1 (Build 24G90)" → "15.6.1"; a beta ("26.0.0 (Build 25A5349a)") keeps its
    // build, since the letter at the end is what says it is one.
    let version = NSProcessInfo::processInfo().operatingSystemVersionString().to_string();
    strip_macos_build(version.trim_start_matches("Version "))
}

/// Drop a release build suffix (`(Build 24G90)`), keep a beta's (ends in a letter).
#[cfg(any(target_os = "macos", test))]
fn strip_macos_build(version: &str) -> String {
    let Some((number, build)) = version.split_once(" (Build ") else { return version.to_owned() };
    let build = build.trim_end_matches(')');
    if build.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        number.to_owned()
    } else {
        version.to_owned()
    }
}

#[cfg(target_os = "windows")]
fn os_version_impl() -> String {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    let mut info: windows::Win32::System::SystemInformation::OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<windows::Win32::System::SystemInformation::OSVERSIONINFOW>() as u32;
    // Unlike `GetVersionEx`, `RtlGetVersion` is not subject to the manifest's compatibility lie.
    if unsafe { RtlGetVersion(&mut info) }.is_ok() {
        format!("{}.{}.{}", info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
    } else {
        "unknown".to_owned()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_version_impl() -> String {
    ["/etc/os-release", "/usr/lib/os-release", "/var/run/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| parse_os_release(&content))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `ID` and `VERSION_ID` from an `os-release` file: `ubuntu 24.04`, `arch`.
#[cfg(any(not(any(target_os = "macos", target_os = "windows")), test))]
fn parse_os_release(content: &str) -> Option<String> {
    let mut id = None;
    let mut version_id = None;
    for line in content.lines() {
        match line.split_once('=') {
            Some(("ID", value)) => id = Some(value.trim_matches('"')),
            Some(("VERSION_ID", value)) => version_id = Some(value.trim_matches('"')),
            _ => {}
        }
    }
    let id = id?;
    Some(match version_id {
        Some(version) => format!("{id} {version}"),
        None => id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_name_is_the_family_for_this_build() {
        let name = os_name();
        if cfg!(target_os = "macos") {
            assert_eq!(name, "macOS");
        } else if cfg!(target_os = "windows") {
            assert_eq!(name, "Windows");
        } else {
            assert!(["Linux", "Linux X11", "Linux Wayland"].contains(&name.as_str()), "{name}");
        }
        assert_eq!(os_version(), "test binary", "tests never ask the OS");
    }

    #[test]
    fn macos_release_builds_are_dropped_and_betas_kept() {
        assert_eq!(strip_macos_build("15.6.1 (Build 24G90)"), "15.6.1");
        assert_eq!(strip_macos_build("26.0.0 (Build 25A5349a)"), "26.0.0 (Build 25A5349a)");
        assert_eq!(strip_macos_build("15.6.1"), "15.6.1");
    }

    #[test]
    fn os_release_yields_id_and_version() {
        assert_eq!(parse_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n"), Some("ubuntu 24.04".to_owned()));
        assert_eq!(parse_os_release("ID=arch\nBUILD_ID=rolling\n"), Some("arch".to_owned()));
        assert_eq!(parse_os_release("NAME=Nothing\n"), None);
    }
}
