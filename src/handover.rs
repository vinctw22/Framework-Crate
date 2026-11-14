//! Headless `--handover` runtime launched by the FrameworkCrateHandover task.
//!
//! Runs as SYSTEM ~30 s after sign-out (see `system_info::handover_task`).
//! No UI, no single-instance mutex, no tracing: reconnect the EC, hand the
//! fan back to firmware, re-apply the configured charge limit, and append a
//! trace to `handover.log` next to the config so the run is verifiable after
//! the next sign-in.

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

/// `handover.log` lives next to the config: the user's config dir is the
/// one place both the app (post sign-in) and SYSTEM can read it.
fn log_path_for(config_path: Option<&Path>) -> PathBuf {
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether a user is actively logged in on the physical console.
///
/// Guards the quick re-login race: sign-out writes the trigger event, the
/// task fires 30 s later, and if the user already signed back in the running
/// app owns the fan and must not be clobbered (manual mode only re-asserts
/// its duty periodically, so a stray `autofanctrl` could stick). Fail-open:
/// any query problem proceeds with the handover, because leaving the fan
/// pinned is the worse outcome.
fn console_session_active() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTSConnectState, WTSFreeMemory, WTSGetActiveConsoleSessionId,
        WTSQuerySessionInformationW,
    };
    // WTS_CONNECTSTATE_CLASS::WTSActive
    const WTS_ACTIVE: i32 = 0;
    const NO_SESSION: u32 = 0xFFFF_FFFF;
    // SAFETY: no-arg WTS call.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == NO_SESSION {
        return false;
    }
    let mut buffer: *mut u16 = std::ptr::null_mut();
    let mut returned: u32 = 0;
    // SAFETY: null server handle means local; the returned buffer is freed below.
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session,
            WTSConnectState,
            &mut buffer,
            &mut returned,
        )
    };
    if ok == 0 || buffer.is_null() {
        return false;
    }
    // WTSConnectState returns a WTS_CONNECTSTATE_CLASS value.
    let active = returned >= 4 && (unsafe { *(buffer as *const i32) }) == WTS_ACTIVE;
    // SAFETY: buffer was allocated by WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(buffer as *mut core::ffi::c_void) };
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
    if console_session_active() {
        log_line(
            &log,
            "handover: console session is active (user signed back in); skipping",
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
    let mut rc = OK;
    match ec.autofanctrl() {
        Ok(()) => log_line(&log, "handover: autofanctrl ok (fan back to firmware)"),
        Err(e) => {
            log_line(&log, &format!("handover: autofanctrl failed: {e}"));
            rc = EC_PARTIAL;
        }
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
    fn log_path_prefers_config_parent() {
        let p = log_path_for(Some(Path::new("C:\\cfg\\framework-crate\\config.toml")));
        assert_eq!(p, PathBuf::from("C:\\cfg\\framework-crate\\handover.log"));
    }
}
