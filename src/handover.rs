//! Headless `--handover` runtime launched by the FrameworkCrateHandover task.
//!
//! Runs as SYSTEM ~30 s after sign-out (see `system_info::handover_task`).
//! No UI, no single-instance mutex, no tracing: reconnect the EC, hand the
//! fan back to firmware, re-apply the charge limit captured in the task
//! arguments (or the config for a manual/legacy `--config` run), and append a
//! trace to `%ProgramData%\framework-crate\handover.log` so the run is
//! verifiable after the next sign-in.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use crate::cli::{EcClient, EcHw};

/// Exit codes: 0 = done or deliberately skipped, 1 = cannot talk to the EC,
/// 2 = EC connected but a command failed.
const OK: i32 = 0;
const EC_UNAVAILABLE: i32 = 1;
const EC_PARTIAL: i32 = 2;

/// Bound for config loading, EC reconnect and the writes. The task itself
/// allows five minutes; without this, a wedged EC ioctl would keep the SYSTEM
/// process alive for the whole limit and leave no trace of what happened.
const EC_PHASE_TIMEOUT: Duration = Duration::from_secs(30);

fn parse_config_arg(args: &[String]) -> Option<PathBuf> {
    args.iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
}

/// Charge limit captured in the task action. The task no longer embeds a
/// user-profile config path, so SYSTEM never reads a user-writable config.
fn parse_charge_limit_arg(args: &[String]) -> Option<u8> {
    args.iter()
        .position(|a| a == "--charge-limit")
        .and_then(|i| args.get(i + 1))
        .and_then(|value| value.parse().ok())
}

/// `handover.log` lives under ProgramData: the handover runs as SYSTEM and
/// must not write into a user-writable profile directory. Users keep read
/// access through the DACL applied by `ensure_handover_log_dir`, so the trace
/// stays verifiable.
fn log_path_for(config_path: Option<&Path>) -> (PathBuf, bool) {
    if let Some(base) = std::env::var_os("ProgramData") {
        return (
            PathBuf::from(base)
                .join("framework-crate")
                .join("handover.log"),
            true,
        );
    }
    // Unusual environments without ProgramData: fall back to the config dir.
    // The fallback is not re-ACLed: it can be the user's own config directory.
    let dir = config_path
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .or_else(|| {
            crate::config::config_path()
                .ok()
                .and_then(|p| p.parent().map(Path::to_path_buf))
        })
        .unwrap_or_else(std::env::temp_dir);
    (dir.join("handover.log"), false)
}

/// Appends one line. The caller must have secured the parent directory;
/// `FILE_FLAG_OPEN_REPARSE_POINT` additionally prevents a planted file symlink
/// from redirecting the SYSTEM write.
fn append_line(path: &Path, line: &str) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if let Ok(mut f) = opts.open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Whether an existing log path is a plain single-link file.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` makes a symlink/junction fail this check
/// instead of being followed; `nNumberOfLinks` rejects a hardlink planted
/// before the directory was secured. Query errors are returned so the caller
/// can disable the trace rather than append as SYSTEM.
fn log_file_is_plain(path: &Path) -> Result<bool, String> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: null-terminated path; the handle is owned by `file` below.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!(
            "cannot inspect {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: handle came from CreateFileW; File closes it.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: live handle and a correctly sized out-parameter.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(format!(
            "cannot inspect {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(
        info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) == 0
            && info.nNumberOfLinks == 1,
    )
}

/// Append-only trace. Disabled when the log directory cannot be secured, so a
/// pre-created junction is never followed by the SYSTEM process.
#[derive(Clone)]
struct Trace {
    path: Option<PathBuf>,
}

impl Trace {
    fn open(path: PathBuf, protected: bool) -> Self {
        let ready = match path.parent() {
            Some(dir) if protected => match crate::system_info::ensure_handover_log_dir(dir) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("handover: {e}; trace disabled");
                    false
                }
            },
            Some(dir) => std::fs::create_dir_all(dir).is_ok(),
            None => false,
        };
        // A pre-created file may still be a symlink/hardlink or carry a
        // permissive DACL from before the directory was secured. Fail closed
        // instead of appending as SYSTEM.
        if ready && protected && path.symlink_metadata().is_ok() {
            match log_file_is_plain(&path) {
                Ok(true) => {}
                Ok(false) => {
                    eprintln!(
                        "handover: {} is not a plain single-link file; trace disabled",
                        path.display()
                    );
                    return Self { path: None };
                }
                Err(e) => {
                    eprintln!("handover: {e}; trace disabled");
                    return Self { path: None };
                }
            }
            match crate::config::has_broad_modify_ace(&path) {
                Ok(true) => {
                    eprintln!(
                        "handover: {} is writable by a broad group; trace disabled",
                        path.display()
                    );
                    return Self { path: None };
                }
                Ok(false) => {}
                Err(e) => {
                    eprintln!(
                        "handover: cannot verify {}: {e}; trace disabled",
                        path.display()
                    );
                    return Self { path: None };
                }
            }
        }
        Self {
            path: ready.then_some(path),
        }
    }

    fn line(&self, line: &str) {
        if let Some(path) = &self.path {
            append_line(path, line);
        }
    }
}

/// Whether another instance of this exe is running in any session.
///
/// The handover runs in session 0, so a per-session mutex cannot see the app in
/// a fast-user-switched or RDP-disconnected session. Enumerating by image name
/// catches those cases and also avoids two processes driving the EC at once.
/// The current handover process is excluded by PID.
fn app_process_running() -> Result<bool, String> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let exe_name = std::env::current_exe()
        .map_err(|e| format!("cannot resolve exe name: {e}"))?
        .file_name()
        .ok_or_else(|| "exe path has no file name".to_string())?
        .to_string_lossy()
        .into_owned();
    // SAFETY: system-wide snapshot; the handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "cannot enumerate processes: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut result = Ok(false);
    // SAFETY: entry is correctly sized and the snapshot is live.
    let mut has_entry = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    loop {
        if !has_entry {
            let err = unsafe { GetLastError() };
            if err != ERROR_NO_MORE_FILES {
                result = Err(format!(
                    "cannot enumerate processes: {}",
                    std::io::Error::from_raw_os_error(err as i32)
                ));
            }
            break;
        }
        if entry.th32ProcessID != std::process::id() {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case(&exe_name) {
                result = Ok(true);
                break;
            }
        }
        // SAFETY: same live snapshot and entry buffer.
        has_entry = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: snapshot came from CreateToolhelp32Snapshot.
    unsafe { CloseHandle(snapshot) };
    result
}

/// Whether the handover should yield to the running app.
///
/// Primary check: any other instance of this exe, in any session. If process
/// enumeration fails, fall back to the old WTSActive check; both checks are
/// fail-open when they cannot answer, because leaving the fan pinned is worse
/// than a possible transient clobber.
fn handover_blocked() -> Option<String> {
    match app_process_running() {
        Ok(true) => Some("another Framework Crate instance is running".to_string()),
        Ok(false) => None,
        Err(e) => {
            if user_session_active() {
                Some(format!(
                    "a user session is active (process check failed: {e})"
                ))
            } else {
                None
            }
        }
    }
}

/// Fallback session check used only when process enumeration fails.
///
/// Only `WTSActive` sessions count; a disconnected session is not counted and
/// can therefore be missed by this fallback. The primary check above detects
/// the app process itself, including in disconnected sessions.
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
    let charge_limit = parse_charge_limit_arg(args);
    let (log_path, protected) = log_path_for(config_path.as_deref());
    let trace = Trace::open(log_path, protected);
    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    trace.line(&format!(
        "handover: start epoch_ms={epoch_ms} pid={} config={:?} charge_limit={:?}",
        std::process::id(),
        config_path,
        charge_limit,
    ));
    if let Some(reason) = handover_blocked() {
        trace.line(&format!("handover: {reason}; skipping"));
        return OK;
    }

    // EC driver calls have no internal timeout. Run the hardware phase on a
    // worker so a wedged ioctl cannot leave the SYSTEM task hanging for the
    // full five-minute Task Scheduler limit.
    let (tx, rx) = channel();
    let worker_trace = trace.clone();
    std::thread::spawn(move || {
        let rc = run_ec_phase(config_path.as_deref(), charge_limit, &worker_trace);
        let _ = tx.send(rc);
    });
    match rx.recv_timeout(EC_PHASE_TIMEOUT) {
        Ok(rc) => rc,
        Err(RecvTimeoutError::Timeout) => {
            trace.line(&format!(
                "handover: timed out after {}s; EC may be wedged",
                EC_PHASE_TIMEOUT.as_secs()
            ));
            EC_PARTIAL
        }
        Err(RecvTimeoutError::Disconnected) => {
            trace.line("handover: worker exited without a result");
            EC_PARTIAL
        }
    }
}

fn run_ec_phase(config_path: Option<&Path>, charge_limit: Option<u8>, trace: &Trace) -> i32 {
    let limit = if let Some(pct) = charge_limit {
        Some(pct.clamp(
            crate::style::CHARGE_LIMIT_MIN as u8,
            crate::style::CHARGE_LIMIT_MAX as u8,
        ))
    } else {
        // Legacy task or manual `--handover --config`: read the persisted
        // value. New tasks pass `--charge-limit` so SYSTEM does not read a
        // user-writable config at all.
        if let Some(p) = config_path
            && !p.exists()
        {
            trace.line(&format!(
                "handover: config file not found ({}); charge limit untouched",
                p.display()
            ));
        }
        let config = match config_path {
            Some(p) => crate::config::load_from(p),
            None => crate::config::load(),
        };
        match config {
            Ok(ref c) => c
                .battery
                .charge_limit_max_pct
                .map(|l| if l.enabled { l.value } else { 100 }),
            Err(ref e) => {
                trace.line(&format!(
                    "handover: config load failed ({e}); charge limit untouched"
                ));
                None
            }
        }
    };
    let ec = match EcClient::new() {
        Ok(ec) => ec,
        Err(e) => {
            trace.line(&format!("handover: EC connect failed: {e}"));
            return EC_UNAVAILABLE;
        }
    };
    // EC init can take long enough for the app to start; re-check before
    // writing so the running app's fan control is not clobbered.
    if let Some(reason) = handover_blocked() {
        trace.line(&format!("handover: {reason} before EC writes; skipping"));
        return OK;
    }
    let mut rc = OK;
    match ec.autofanctrl() {
        Ok(()) => trace.line("handover: autofanctrl ok (fan back to firmware)"),
        Err(e) => {
            trace.line(&format!("handover: autofanctrl failed: {e}"));
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
            trace.line(&format!("handover: fan readback {}", rpms.join(", ")));
        }
        Ok(_) => trace.line("handover: fan readback returned no fans"),
        Err(e) => trace.line(&format!("handover: fan readback failed: {e}")),
    }
    if let Some(pct) = limit {
        match ec.charge_limit_set(0, pct) {
            Ok(()) => trace.line(&format!("handover: charge limit {pct}% applied")),
            Err(e) => {
                trace.line(&format!("handover: charge limit {pct}% failed: {e}"));
                rc = EC_PARTIAL;
            }
        }
    }
    trace.line(&format!("handover: done rc={rc}"));
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
    fn parse_charge_limit_arg_extracts_value() {
        let args: Vec<String> = ["--handover", "--charge-limit", "80"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_charge_limit_arg(&args), Some(80));
        let invalid: Vec<String> = ["--charge-limit", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_charge_limit_arg(&invalid), None);
        let dangling: Vec<String> = ["--charge-limit"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_charge_limit_arg(&dangling), None);
    }

    #[test]
    fn log_path_uses_program_data() {
        let (p, protected) = log_path_for(Some(Path::new("C:\\cfg\\framework-crate\\config.toml")));
        if let Some(base) = std::env::var_os("ProgramData") {
            assert!(protected);
            assert_eq!(
                p,
                PathBuf::from(base)
                    .join("framework-crate")
                    .join("handover.log")
            );
        } else {
            assert!(!protected);
            assert_eq!(p, PathBuf::from("C:\\cfg\\framework-crate\\handover.log"));
        }
    }

    #[test]
    fn append_line_appends_to_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("handover.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        append_line(&path, "hello");
        append_line(&path, "world");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn log_file_must_be_a_single_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("handover.log");
        std::fs::write(&path, "x").unwrap();
        assert!(log_file_is_plain(&path).expect("inspect plain file"));

        let link = dir.path().join("handover-link.log");
        if std::fs::hard_link(&path, &link).is_err() {
            return; // filesystem without hardlink support
        }
        assert!(
            !log_file_is_plain(&path).expect("inspect linked file"),
            "a pre-planted hardlink must disable the trace"
        );
    }

    #[test]
    fn log_file_inspection_fails_closed_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(log_file_is_plain(&dir.path().join("missing.log")).is_err());
    }

    #[test]
    fn process_probe_ignores_itself() {
        // The test exe has a unique name and is excluded by PID; no other
        // process with this image name should be running.
        assert!(!app_process_running().expect("process snapshot"));
    }
}
