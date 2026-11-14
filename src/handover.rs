//! Headless `--handover` runtime launched by the FrameworkCrateHandover task.
//!
//! Runs as SYSTEM ~30 s after sign-out (see `system_info::handover_task`).
//! No UI, no single-instance mutex, no tracing: reconnect the EC, hand the
//! fan back to firmware, re-apply the configured charge limit, and append a
//! trace to `%ProgramData%\framework-crate\handover.log` so the run is
//! verifiable after the next sign-in.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::{EcClient, EcHw};

/// Exit codes: 0 = done or deliberately skipped, 1 = cannot talk to the EC,
/// 2 = EC connected but a command failed.
const OK: i32 = 0;
const EC_UNAVAILABLE: i32 = 1;
const EC_PARTIAL: i32 = 2;

fn parse_config_arg(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

/// `handover.log` lives under ProgramData: the handover runs as SYSTEM and
/// must not write into a user-writable profile directory. Users keep read
/// access through ProgramData's inherited ACL, so the trace stays verifiable.
fn log_path_for(config_path: Option<&Path>) -> PathBuf {
    if let Some(base) = std::env::var_os("ProgramData") {
        return PathBuf::from(base)
            .join("framework-crate")
            .join("handover.log");
    }
    // Unusual environments without ProgramData: fall back to the config dir.
    let dir = config_path
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .or_else(|| {
            crate::config::config_path()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        })
        .unwrap_or_else(std::env::temp_dir);
    dir.join("handover.log")
}

fn log_line(path: &Path, line: &str) {
    // The config directory can be missing after a profile reset; create it so
    // the SYSTEM run leaves a verifiable trace instead of failing silently.
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether any user session is active, i.e. someone is logged on and the
/// running app owns fan control.
///
/// Guards the quick re-login race: sign-out writes the trigger event, the
/// task fires 30 s later, and if a user already signed back in the running
/// app owns the fan and must not be clobbered (manual mode only re-asserts
/// its duty periodically, so a stray `autofanctrl` could stick). Checking
/// every session (not just the console session) covers RDP and fast user
/// switching. Fail-open: any query problem proceeds with the handover,
/// because leaving the fan pinned is the worse outcome.
fn user_session_active() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW,
        WTSFreeMemory,
    };
    let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: null server handle means local; the array is freed below.
    let ok =
        unsafe { WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut info, &mut count) };
    if ok == 0 || info.is_null() {
        return false;
    }
    // SAFETY: the API returned `count` initialized entries at `info`.
    let active = unsafe { std::slice::from_raw_parts(info, count as usize) }
        .iter()
        .any(|session| session.State == WTSActive);
    // SAFETY: array allocated by WTSEnumerateSessionsW.
    unsafe { WTSFreeMemory(info as *mut core::ffi::c_void) };
    active
}

pub fn run(args: &[String]) -> i32 {
    let config_path = parse_config_arg(args);
    let log = log_path_for(config_path.as_deref());
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    log_line(
        &log,
        &format!(
            "handover: start epoch_ms={epoch_ms} pid={} config={:?}",
            std::process::id(),
            config_path,
        ),
    );
    if user_session_active() {
        log_line(
            &log,
            "handover: a user session is active (user signed back in); skipping",
        );
        return OK;
    }
    let config = match config_path.as_deref() {
        Some(p) => crate::config::load_from(p),
        None => crate::config::load(),
    };
    let limit = match config {
        Ok(ref c) => c.battery.charge_limit_max_pct,
        Err(ref e) => {
            log_line(
                &log,
                &format!("handover: config load failed ({e}); charge limit untouched"),
            );
            None
        }
    };
    let ec = match EcClient::new() {
        Ok(ec) => ec,
        Err(e) => {
            log_line(&log, &format!("handover: EC connect failed: {e}"));
            return EC_UNAVAILABLE;
        }
    };
    // EC init can take long enough for a user to sign back in; re-check
    // before writing so the running app's fan control is not clobbered.
    if user_session_active() {
        log_line(
            &log,
            "handover: a user session became active before EC writes; skipping",
        );
        return OK;
    }
    let mut rc = OK;
    match ec.autofanctrl() {
        Ok(()) => log_line(&log, "handover: autofanctrl ok (fan back to firmware)"),
        Err(e) => {
            log_line(&log, &format!("handover: autofanctrl failed: {e}"));
            rc = EC_PARTIAL;
        }
    }
    // Read the fans back so the log shows what the EC reports after the
    // switch, not just that the command returned success.
    match ec.thermal() {
        Ok(data) if !data.fans.is_empty() => {
            let rpms: Vec<String> = data
                .fans
                .iter()
                .map(|fan| format!("{}={}rpm", fan.name, fan.rpm))
                .collect();
            log_line(&log, &format!("handover: fan readback {}", rpms.join(", ")));
        }
        Ok(_) => log_line(&log, "handover: fan readback returned no fans"),
        Err(e) => log_line(&log, &format!("handover: fan readback failed: {e}")),
    }
    if let Some(limit) = limit {
        let pct = if limit.enabled { limit.value } else { 100 };
        match ec.charge_limit_set(0, pct) {
            Ok(()) => log_line(&log, &format!("handover: charge limit {pct}% applied")),
            Err(e) => {
                log_line(&log, &format!("handover: charge limit {pct}% failed: {e}"));
                rc = EC_PARTIAL;
            }
        }
    }
    log_line(&log, &format!("handover: done rc={rc}"));
    rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_arg_extracts_value() {
        let args: Vec<String> = [
            "framework-crate.exe",
            "--handover",
            "--config",
            "C:\\c.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(parse_config_arg(&args), Some(PathBuf::from("C:\\c.toml")));
    }

    #[test]
    fn parse_config_arg_missing_or_dangling() {
        let args: Vec<String> = ["x", "--handover"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_config_arg(&args), None);
        let dangling: Vec<String> = ["x", "--handover", "--config"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_config_arg(&dangling), None);
    }

    #[test]
    fn log_path_uses_program_data() {
        let p = log_path_for(Some(Path::new("C:\\cfg\\framework-crate\\config.toml")));
        if let Some(base) = std::env::var_os("ProgramData") {
            assert_eq!(
                p,
                PathBuf::from(base)
                    .join("framework-crate")
                    .join("handover.log")
            );
        } else {
            assert_eq!(p, PathBuf::from("C:\\cfg\\framework-crate\\handover.log"));
        }
    }

    #[test]
    fn log_line_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("handover.log");
        log_line(&path, "hello");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }
}
