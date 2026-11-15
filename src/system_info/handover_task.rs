//! Sign-out fan handover task registration (Plan B for elevated sign-out).
//!
//! Empirically (2026-11-10/11 tests) an elevated app did not receive any
//! in-session logoff notification before winlogon killed it. The tray pump now
//! also registers WTS notifications as a best-effort supplement, but the
//! reliable mechanism that runs *after* the session is gone is this Task
//! Scheduler task registered as SYSTEM and triggered by the sign-out event
//! itself.
//!
//! Trigger choice: the XML schema has no logoff value for
//! `SessionStateChangeTrigger` (only connect/disconnect/lock/unlock strings;
//! the COM API rejects numeric 6 as well), and the legacy `LogoffTrigger`
//! cannot be expressed in XML at all. What works is an `EventTrigger` on
//! `Microsoft-Windows-TerminalServices-LocalSessionManager/Operational`
//! event ID 23 (session logoff succeeded), which the event log confirms is
//! written on every sign-out and not on lock or disconnect. The 30 s delay
//! lets winlogon finish killing session processes first.
//!
//! The action runs `framework-crate.exe --handover --charge-limit <pct>` as
//! SYSTEM: reconnect the EC, hand the fan back to firmware (`autofanctrl`),
//! re-apply the limit captured at registration, and log to
//! `%ProgramData%\framework-crate\handover.log`. Passing the limit in the
//! action means SYSTEM never reads a user-writable config; manual/legacy
//! `--config` runs are still accepted by the headless mode.
//!
//! Security: because the action runs as SYSTEM, the exe must live under
//! Program Files (see `validate_system_exe_location`). Otherwise any user
//! who can replace the exe would execute code as SYSTEM at sign-out.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;

use crate::types::{Config, FanControlMode};

use super::startup_task::{
    cleanup_stale_task_xml, decode_console_bytes, is_elevated, schtasks_output,
    validate_task_command_path, write_task_xml, xml_escape,
};

pub const HANDOVER_TASK_NAME: &str = "FrameworkCrateHandover";

/// Serialized so a mode-change burst cannot race two `/Create /F` calls
/// against the same task name.
static HANDOVER_SYNC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// TerminalServices-LocalSessionManager Operational channel name.
const SIGNOUT_LOG: &str = "Microsoft-Windows-TerminalServices-LocalSessionManager/Operational";

/// Provider that writes event ID 23. This is the provider name, not the
/// channel path: `Provider[@Name=...]` must not include `/Operational` or the
/// subscription silently matches nothing.
const SIGNOUT_PROVIDER: &str = "Microsoft-Windows-TerminalServices-LocalSessionManager";

/// Directories standard users cannot replace the action exe in.
/// CSIDL_PROGRAM_FILES / CSIDL_PROGRAM_FILESX86; resolved through the shell
/// API, never from environment variables a user can override.
const PROTECTED_EXE_FOLDERS: [i32; 2] = [0x0026, 0x002A];

/// Development escape hatch for running from the build tree. Setting it
/// accepts that any user who can write the exe can execute code as SYSTEM.
const ALLOW_UNPROTECTED_ENV: &str = "FRAMEWORK_CONTROL_ALLOW_UNPROTECTED_HANDOVER";

/// Builds the EventTrigger subscription query. Event ID 23 is "session
/// logoff succeeded", written on every sign-out and not on lock/disconnect.
fn signout_subscription() -> String {
    format!(
        "<QueryList><Query Id=\"0\" Path=\"{log}\"><Select Path=\"{log}\">*[System[Provider[@Name='{provider}'] and (EventID=23)]]</Select></Query></QueryList>",
        log = SIGNOUT_LOG,
        provider = SIGNOUT_PROVIDER,
    )
}

/// True when the persisted mode needs the sign-out task: manual and curve
/// pin the fan, disabled leaves it to firmware.
fn handover_desired(mode: u8) -> bool {
    matches!(
        FanControlMode::from_u8(mode),
        FanControlMode::Manual | FanControlMode::Curve
    )
}

/// Component-wise, case-insensitive "is `path` inside `root`". String
/// `starts_with` alone would accept `C:\Program Files Evil`.
fn path_is_under(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().to_lowercase();
    let root = root.to_string_lossy().to_lowercase();
    let root = root.trim_end_matches(['\\', '/']);
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('\\') || rest.starts_with('/'))
}

/// Case-insensitive path equality for canonical Windows paths.
fn paths_equal_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Rejects a path whose DACL grants broad-group write or delete rights.
fn reject_broad_acl(path: &Path) -> Result<(), String> {
    let broad = crate::config::has_broad_modify_ace(path)
        .map_err(|e| format!("cannot verify the ACL of {}: {e}", path.display()))?;
    if broad {
        return Err(format!(
            "{} grants write or delete access to a broad group; refusing to register a SYSTEM task that runs a replaceable exe",
            path.display()
        ));
    }
    Ok(())
}

/// Walks from the exe's directory up to the Program Files root and checks
/// every object on the way, then the exe itself.
///
/// A writable ancestor is enough to replace the whole subtree, so checking
/// only the exe and its parent is not sufficient. Fail-closed: a query error
/// aborts registration rather than assuming the path is safe.
fn check_exe_acl_chain(exe: &Path, root: &Path) -> Result<(), String> {
    let mut dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", exe.display()))?
        .to_path_buf();
    loop {
        reject_broad_acl(&dir)?;
        if paths_equal_ci(&dir, root) {
            break;
        }
        dir = dir
            .parent()
            .ok_or_else(|| {
                format!(
                    "{} is not inside the protected root {}",
                    exe.display(),
                    root.display()
                )
            })?
            .to_path_buf();
    }
    reject_broad_acl(exe)
}

/// Development escape hatch. Release builds ignore it: honoring a user-set
/// environment variable would defeat the Program Files gate they rely on.
fn unprotected_handover_allowed() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var_os(ALLOW_UNPROTECTED_ENV).is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        if std::env::var_os(ALLOW_UNPROTECTED_ENV).is_some() {
            tracing::warn!("{ALLOW_UNPROTECTED_ENV} is ignored in release builds");
        }
        false
    }
}

/// Result of the exe location gate.
enum ExeLocation {
    /// Program Files and a clean ACL chain: register the task.
    Allowed,
    /// Outside Program Files: deliberately skip without an error. A portable
    /// install must not emit a warning on every launch.
    Portable,
}

/// Checks an action exe a standard user could replace.
///
/// A SYSTEM task that runs a user-writable binary is a privilege escalation:
/// the user swaps the exe and gets SYSTEM at the next sign-out. Requiring a
/// canonical path under Program Files removes the attack, and the fail-closed
/// broad-group ACL check catches installs whose folder ACLs were loosened.
///
/// A path outside Program Files is an expected `Portable` skip, not an error;
/// real failures (unresolvable roots, ACL query errors, unsafe ACLs) stay
/// errors so the caller can warn.
fn validate_system_exe_location(exe: &Path) -> Result<ExeLocation, String> {
    if unprotected_handover_allowed() {
        tracing::warn!(
            "{ALLOW_UNPROTECTED_ENV} is set: registering a SYSTEM task for {} without the Program Files check",
            exe.display()
        );
        return Ok(ExeLocation::Allowed);
    }
    let canonical = std::fs::canonicalize(exe)
        .map_err(|e| format!("cannot resolve exe path for handover task: {e}"))?;
    let mut roots = Vec::with_capacity(PROTECTED_EXE_FOLDERS.len());
    for csidl in PROTECTED_EXE_FOLDERS {
        if let Some(dir) = super::startup_task::known_folder(csidl)
            && let Ok(dir) = std::fs::canonicalize(dir)
        {
            roots.push(dir);
        }
    }
    if roots.is_empty() {
        return Err("cannot resolve a Program Files directory for the handover task".to_string());
    }
    let Some(root) = roots.iter().find(|root| path_is_under(&canonical, root)) else {
        return Ok(ExeLocation::Portable);
    };
    check_exe_acl_chain(&canonical, root)?;
    Ok(ExeLocation::Allowed)
}

/// Builds the handover task document. `command` is already quote-wrapped,
/// `args` is the literal argument string for the action.
fn build_handover_task_xml(command: &str, args: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Framework Crate</Author>
    <Description>Return Framework fan control to firmware when the user signs out. Created by Framework Crate; switching the fan mode to Disabled removes it.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-18</UserId>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT5M</ExecutionTimeLimit>
    <Priority>7</Priority>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
  </Settings>
  <Triggers>
    <EventTrigger>
      <Enabled>true</Enabled>
      <Subscription>{subscription}</Subscription>
      <Delay>PT30S</Delay>
    </EventTrigger>
  </Triggers>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        subscription = xml_escape(&signout_subscription()),
        command = xml_escape(command),
        args = xml_escape(args),
    )
}

/// Builds the handover task document for `exe` with the effective charge
/// limit captured at registration (`None` = no charge-limit write).
///
/// Pure: no I/O, so the document is verifiable by `cargo test` without
/// administrator rights. NOTE: no `RestartOnFailure` on purpose — a retry
/// minutes later could run after the user signed back in and clobber the
/// running app's fan control (the headless mode's app-running check limits but
/// does not eliminate that window, so retries stay off).
pub(crate) fn build_handover_task(exe: &str, charge_limit: Option<u8>) -> Result<String, String> {
    validate_task_command_path(exe)?;
    let args = match charge_limit {
        Some(pct) => format!(
            "--handover --charge-limit {}",
            pct.clamp(
                crate::style::CHARGE_LIMIT_MIN as u8,
                crate::style::CHARGE_LIMIT_MAX as u8
            )
        ),
        None => "--handover".to_string(),
    };
    Ok(build_handover_task_xml(&format!("\"{exe}\""), &args))
}

/// Removes the `\\?\` verbatim prefix without corrupting a UNC path.
///
/// `current_exe()` can return `\\?\C:\...` or
/// `\\?\UNC\server\share\...` when started via a long path. The task action
/// runs in the normal Win32 namespace, so the prefix is stripped; a blind
/// `strip_prefix(r"\\?\")` would turn `\\?\UNC\...` into the relative
/// `UNC\...`.
fn plain_path_for_task(path: &str) -> String {
    // `str::get` keeps the slice on a character boundary; a non-ASCII path
    // must not panic before the prefix check.
    if let Some(prefix) = path.get(..8)
        && prefix.eq_ignore_ascii_case(r"\\?\UNC\")
    {
        return format!(r"\\{}", &path[8..]);
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    path.to_string()
}

/// Queries the task. Unlike a `bool` helper, this keeps an I/O/schtasks
/// failure distinct from "the task does not exist".
fn query_handover_task() -> Result<std::process::Output, String> {
    schtasks_output(&["/Query", "/TN", HANDOVER_TASK_NAME])
}

fn schtasks_error_detail(output: &std::process::Output) -> String {
    let stderr = decode_console_bytes(&output.stderr);
    let stdout = decode_console_bytes(&output.stdout);
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if !stdout.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        format!(
            "schtasks failed with no output (exit code {})",
            output.status.code().unwrap_or(-1)
        )
    }
}

/// Secures the ProgramData log directory while the elevated app is registering
/// the task. The handover process also calls this, but the directory may exist
/// long before the first sign-out (or have been pre-created by a user), so the
/// app must establish the protected ACL up front.
fn secure_handover_log_dir() -> Result<(), String> {
    let base =
        std::env::var_os("ProgramData").ok_or_else(|| "ProgramData is not set".to_string())?;
    let dir = PathBuf::from(base).join("framework-crate");
    super::ensure_handover_log_dir(&dir)
}

/// Registers the handover task (idempotent `/Create /F`, also refreshes the
/// action path after the exe was moved).
fn create_handover_task(charge_limit: Option<u8>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve exe path: {e}"))?;
    match validate_system_exe_location(&exe)? {
        ExeLocation::Allowed => {}
        ExeLocation::Portable => {
            tracing::debug!(
                "handover task skipped: {} is outside Program Files",
                exe.display()
            );
            return Ok(());
        }
    }
    // Do this before the first handover: a pre-created directory must be
    // re-owned/re-ACLed (or rejected if it is a junction), not trusted.
    if let Err(e) = secure_handover_log_dir() {
        tracing::warn!("cannot secure handover log directory: {e}");
    }
    let exe_str = exe.to_str().ok_or("exe path is not valid UTF-8")?;
    // current_exe() can be verbatim when launched via a long/UNC path; the
    // task action runs in the normal namespace.
    let exe_str = plain_path_for_task(exe_str);
    let xml = build_handover_task(&exe_str, charge_limit)?;
    cleanup_stale_task_xml();
    let xml_path = write_task_xml(&xml)?;
    let created = schtasks_output(&[
        "/Create",
        "/TN",
        HANDOVER_TASK_NAME,
        "/XML",
        &xml_path,
        "/F",
    ]);
    let _ = std::fs::remove_file(&xml_path);
    let out = created?;
    if !out.status.success() {
        let detail = schtasks_error_detail(&out);
        if !is_elevated() {
            return Err(format!(
                "sign-out handover task needs administrator privileges; run Framework Crate as administrator ({detail})"
            ));
        }
        return Err(detail);
    }
    match query_handover_task() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(_) => Err("handover task was created but cannot be queried".to_string()),
        Err(e) => Err(format!(
            "handover task was created but the follow-up query failed: {e}"
        )),
    }
}

fn delete_handover_task() -> Result<(), String> {
    let out = schtasks_output(&["/Delete", "/TN", HANDOVER_TASK_NAME, "/F"])?;
    if out.status.success() {
        return Ok(());
    }
    let detail = schtasks_error_detail(&out);
    // A concurrent uninstall or manual deletion already did our work. A
    // failed query is not evidence of absence, so report it instead of
    // silently leaving a stale SYSTEM task behind.
    match query_handover_task() {
        Ok(query) if query.status.success() => Err(detail),
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "{detail}; cannot verify whether the task still exists: {e}"
        )),
    }
}

/// Brings the handover task in line with the fan mode and charge limit:
/// manual/curve need it (re-created every call so a moved exe path
/// self-heals), disabled removes a stale one. Both values are read under the
/// sync lock, so sync threads that acquire the lock out of order still apply
/// the user's latest choice.
///
/// The limit is embedded in the task action; SYSTEM never reads the user's
/// config. No-op when the process is not elevated: Task Scheduler denies
/// SYSTEM-task creation/query/delete to a non-elevated caller. Failures are
/// returned for the caller to log; the in-session session-end hook still
/// covers graceful exits.
pub fn sync_handover_task(mode: &AtomicU64, config: &RwLock<Arc<Config>>) -> Result<(), String> {
    let _guard = match HANDOVER_SYNC_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if !is_elevated() {
        tracing::debug!("handover task sync skipped: process is not elevated");
        return Ok(());
    }
    let (desired, charge_limit) = {
        let cfg = config.read();
        (
            handover_desired(mode.load(Ordering::Acquire) as u8),
            cfg.battery
                .charge_limit_max_pct
                .map(|limit| if limit.enabled { limit.value } else { 100 }),
        )
    };
    if desired {
        create_handover_task(charge_limit)
    } else {
        match query_handover_task() {
            Ok(out) if out.status.success() => delete_handover_task(),
            Ok(_) => Ok(()),
            Err(e) => Err(format!("cannot query the handover task: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_task_xml_targets_signout_event_and_system() {
        let xml = build_handover_task(r"C:\app\framework-crate.exe", Some(80)).expect("ok");
        assert!(xml.contains("<UserId>S-1-5-18<"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<EventTrigger>"));
        assert!(xml.contains("EventID=23"));
        assert!(xml.contains("TerminalServices-LocalSessionManager"));
        assert!(xml.contains("<Delay>PT30S<"));
        assert!(xml.contains(r"<Command>&quot;C:\app\framework-crate.exe&quot;<"));
        assert!(xml.contains("<Arguments>--handover --charge-limit 80<"));
        // The task must not embed a user-profile path for SYSTEM to read.
        assert!(!xml.contains("--config"));
        // A retry could clobber a freshly signed-in app's fan control.
        assert!(!xml.contains("<RestartOnFailure>"));
        // Laptop: battery conditions must not block the handover.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
    }

    #[test]
    fn handover_task_subscription_is_xml_escaped() {
        let xml = build_handover_task(r"C:\app\framework-crate.exe", Some(80)).expect("ok");
        // The raw query text must not appear unescaped inside Subscription.
        assert!(!xml.contains("<Subscription><QueryList>"));
        assert!(xml.contains("&lt;QueryList&gt;"));
    }

    #[test]
    fn subscription_filters_on_provider_not_channel() {
        let sub = signout_subscription();
        assert!(
            sub.contains(
                "Provider[@Name='Microsoft-Windows-TerminalServices-LocalSessionManager']"
            )
        );
        // The channel path includes /Operational; the provider name must not,
        // or the filter matches nothing and the task never fires.
        assert!(!sub.contains("LocalSessionManager/Operational']"));
        assert!(sub.contains("EventID=23"));
        assert!(sub.contains(
            "Select Path=\"Microsoft-Windows-TerminalServices-LocalSessionManager/Operational\""
        ));
    }

    #[test]
    fn handover_desired_follows_fan_mode() {
        use crate::types::FanControlMode;
        assert!(!handover_desired(FanControlMode::Disabled.to_u8()));
        assert!(handover_desired(FanControlMode::Manual.to_u8()));
        assert!(handover_desired(FanControlMode::Curve.to_u8()));
    }

    #[test]
    fn protected_root_check_is_component_wise_and_case_insensitive() {
        let root = Path::new(r"C:\Program Files");
        assert!(path_is_under(
            Path::new(r"C:\Program Files\App\framework-crate.exe"),
            root
        ));
        assert!(path_is_under(
            Path::new(r"c:\program files\app\framework-crate.exe"),
            root
        ));
        assert!(!path_is_under(
            Path::new(r"C:\Program Files Evil\framework-crate.exe"),
            root
        ));
        assert!(!path_is_under(
            Path::new(r"C:\Users\Jane\framework-crate.exe"),
            root
        ));
    }

    #[test]
    fn handover_task_rejects_dangerous_paths() {
        // The exe path is embedded as the task Command and run directly, so
        // it keeps the strict metacharacter set.
        for bad in [r#"C:\a"b.exe"#, r"C:\a&b.exe", r"C:\app\"] {
            assert!(
                build_handover_task(bad, Some(80)).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn handover_task_omits_unset_charge_limit() {
        let xml = build_handover_task(r"C:\app\framework-crate.exe", None).expect("ok");
        assert!(xml.contains("<Arguments>--handover<"));
        assert!(!xml.contains("--charge-limit"));
    }

    #[test]
    fn handover_task_clamps_charge_limit() {
        let xml = build_handover_task(r"C:\app\framework-crate.exe", Some(0)).expect("ok");
        assert!(xml.contains("<Arguments>--handover --charge-limit 25<"));
    }

    #[test]
    fn portable_exe_is_skipped_without_error() {
        // A temp path is outside Program Files. The gate must report a
        // deliberate skip, not an error the startup/mode-change caller logs
        // as a warning.
        if std::env::var_os(ALLOW_UNPROTECTED_ENV).is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("framework-crate.exe");
        std::fs::write(&exe, b"x").unwrap();
        assert!(matches!(
            validate_system_exe_location(&exe),
            Ok(ExeLocation::Portable)
        ));
    }

    #[test]
    fn task_path_strips_verbatim_prefix_without_breaking_unc() {
        assert_eq!(
            plain_path_for_task(r"\\?\C:\cfg\config.toml"),
            r"C:\cfg\config.toml"
        );
        assert_eq!(
            plain_path_for_task(r"\\?\UNC\server\share\config.toml"),
            r"\\server\share\config.toml"
        );
        assert_eq!(
            plain_path_for_task(r"\\?\unc\server\share\config.toml"),
            r"\\server\share\config.toml"
        );
        // Paths that were already in the normal namespace stay untouched.
        assert_eq!(
            plain_path_for_task(r"C:\cfg\config.toml"),
            r"C:\cfg\config.toml"
        );
        assert_eq!(
            plain_path_for_task(r"\\server\share\config.toml"),
            r"\\server\share\config.toml"
        );
        // Multi-byte input shorter than the UNC prefix must not panic while
        // checking the first eight bytes.
        assert_eq!(
            plain_path_for_task("\u{4e00}\u{4e01}\u{4e02}"),
            "\u{4e00}\u{4e01}\u{4e02}"
        );
    }

    #[test]
    fn handover_task_accepts_paths_with_spaces() {
        assert!(
            build_handover_task(
                r"C:\Program Files\Framework Crate\framework-crate.exe",
                Some(80)
            )
            .is_ok()
        );
    }
}
