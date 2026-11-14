//! Sign-out fan handover task registration (Plan B for elevated sign-out).
//!
//! Empirically (2026-11-10/11 tests) an elevated app did not receive any
//! in-session logoff notification before winlogon killed it. The tray pump now
//! also registers WTS notifications and a hidden-console ctrl handler as
//! best-effort supplements, but the reliable mechanism that runs *after* the
//! session is gone is this Task Scheduler task registered as SYSTEM and
//! triggered by the sign-out event itself.
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
//! The action runs `framework-crate.exe --handover --config <path>` as
//! SYSTEM: reconnect the EC, hand the fan back to firmware (`autofanctrl`),
//! re-apply the configured charge limit, and log to `handover.log` next to
//! the config. The config path is passed explicitly because the default
//! path would resolve against the SYSTEM profile.
//!
//! Security: because the action runs as SYSTEM, the exe must live under
//! Program Files (see `validate_system_exe_location`). Otherwise any user
//! who can replace the exe would execute code as SYSTEM at sign-out.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::FanControlMode;

use super::startup_task::{
    cleanup_stale_task_xml, decode_console_bytes, is_elevated, schtasks_output,
    validate_task_arg_path, validate_task_command_path, write_task_xml, xml_escape,
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
const PROTECTED_EXE_ROOTS: [&str; 3] = ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"];

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

/// Rejects an action exe a standard user could replace.
///
/// A SYSTEM task that runs a user-writable binary is a privilege escalation:
/// the user swaps the exe and gets SYSTEM at the next sign-out. Requiring a
/// canonical path under Program Files removes the attack, and the broad-group
/// write check catches installs whose folder ACLs were loosened.
fn validate_system_exe_location(exe: &Path) -> Result<(), String> {
    if std::env::var_os(ALLOW_UNPROTECTED_ENV).is_some() {
        tracing::warn!(
            "{ALLOW_UNPROTECTED_ENV} is set: registering a SYSTEM task for {} without the Program Files check",
            exe.display()
        );
        return Ok(());
    }
    let canonical = std::fs::canonicalize(exe)
        .map_err(|e| format!("cannot resolve exe path for handover task: {e}"))?;
    let mut roots = Vec::with_capacity(PROTECTED_EXE_ROOTS.len());
    for var in PROTECTED_EXE_ROOTS {
        if let Some(dir) = std::env::var_os(var)
            && let Ok(dir) = std::fs::canonicalize(dir)
        {
            roots.push(dir);
        }
    }
    if roots.is_empty() {
        return Err("cannot resolve a Program Files directory for the handover task".to_string());
    }
    if !roots.iter().any(|root| path_is_under(&canonical, root)) {
        return Err(format!(
            "{} is outside Program Files; refusing to register a SYSTEM task that runs a user-replaceable exe (install to Program Files, or set {ALLOW_UNPROTECTED_ENV}=1 for development)",
            exe.display()
        ));
    }
    let parent = canonical
        .parent()
        .ok_or_else(|| "exe path has no parent directory".to_string())?;
    if crate::config::file_has_world_write_ace(&canonical)
        || crate::config::file_has_world_write_ace(parent)
    {
        return Err(format!(
            "{} or its directory grants write access to non-administrators; refusing to register a SYSTEM handover task",
            exe.display()
        ));
    }
    Ok(())
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

/// Builds the handover task document for `exe` acting on `config_path`.
///
/// Pure: no I/O, so the document is verifiable by `cargo test` without
/// administrator rights. NOTE: no `RestartOnFailure` on purpose — a retry
/// minutes later could run after the user signed back in and clobber the
/// running app's fan control (the headless mode's session check limits but
/// does not eliminate that window, so retries stay off).
pub(crate) fn build_handover_task(exe: &str, config_path: &str) -> Result<String, String> {
    validate_task_command_path(exe)?;
    validate_task_arg_path(config_path, "config path")?;
    Ok(build_handover_task_xml(
        &format!("\"{exe}\""),
        &format!("--handover --config \"{config_path}\""),
    ))
}

/// Whether the handover task is registered.
pub fn handover_task_enabled() -> bool {
    schtasks_output(&["/Query", "/TN", HANDOVER_TASK_NAME])
        .map(|o| o.status.success())
        .unwrap_or(false)
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

/// Registers the handover task (idempotent `/Create /F`, also refreshes the
/// action path after the exe or config was moved).
fn create_handover_task() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve exe path: {e}"))?;
    validate_system_exe_location(&exe)?;
    let exe_str = exe.to_str().ok_or("exe path is not valid UTF-8")?;
    let config = crate::config::config_path()?;
    let config_str = config.to_str().ok_or("config path is not valid UTF-8")?;
    // config_path() canonicalizes, which prefixes `\\?\`; that prefix is a
    // Win32-internal spelling that schtasks, cmd and our own path validator
    // (which rejects `?`) all choke on, and the task action runs through the
    // normal path namespace anyway.
    let config_str = config_str.strip_prefix(r"\\?\").unwrap_or(config_str);
    let xml = build_handover_task(exe_str, config_str)?;
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
    if !handover_task_enabled() {
        return Err("handover task was created but cannot be queried".to_string());
    }
    Ok(())
}

fn delete_handover_task() -> Result<(), String> {
    let out = schtasks_output(&["/Delete", "/TN", HANDOVER_TASK_NAME, "/F"])?;
    if out.status.success() {
        return Ok(());
    }
    // A concurrent uninstall or manual deletion already did our work.
    if !handover_task_enabled() {
        return Ok(());
    }
    Err(schtasks_error_detail(&out))
}

/// Brings the handover task in line with the fan mode: manual/curve need it
/// (re-created every call so a moved exe path self-heals), disabled removes a
/// stale one. The mode is read under the sync lock, so sync threads that
/// acquire the lock out of order still apply the user's latest choice.
/// Failures are returned for the caller to log; the in-session session-end
/// hook still covers graceful exits.
pub fn sync_handover_task(mode: &AtomicU64) -> Result<(), String> {
    let _guard = match HANDOVER_SYNC_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if handover_desired(mode.load(Ordering::Acquire) as u8) {
        create_handover_task()
    } else if handover_task_enabled() {
        delete_handover_task()
    } else if is_elevated() {
        Ok(())
    } else {
        // Task Scheduler denies reading a SYSTEM task to a non-elevated
        // caller, so the failed query above is not evidence that no task
        // exists. Report instead of silently leaving a stale task behind.
        Err(
            "cannot verify or remove the SYSTEM handover task without administrator privileges"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_task_xml_targets_signout_event_and_system() {
        let xml =
            build_handover_task(r"C:\app\framework-crate.exe", r"C:\cfg\config.toml").expect("ok");
        assert!(xml.contains("<UserId>S-1-5-18<"));
        assert!(xml.contains("<RunLevel>HighestAvailable<"));
        assert!(xml.contains("<EventTrigger>"));
        assert!(xml.contains("EventID=23"));
        assert!(xml.contains("TerminalServices-LocalSessionManager"));
        assert!(xml.contains("<Delay>PT30S<"));
        assert!(xml.contains(r"<Command>&quot;C:\app\framework-crate.exe&quot;<"));
        assert!(xml.contains("--handover --config &quot;C:\\cfg\\config.toml&quot;"));
        // A retry could clobber a freshly signed-in app's fan control.
        assert!(!xml.contains("<RestartOnFailure>"));
        // Laptop: battery conditions must not block the handover.
        assert!(xml.contains("<DisallowStartIfOnBatteries>false<"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false<"));
    }

    #[test]
    fn handover_task_subscription_is_xml_escaped() {
        let xml =
            build_handover_task(r"C:\app\framework-crate.exe", r"C:\cfg\config.toml").expect("ok");
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
        let base = r"C:\app\framework-crate.exe";
        // Config arguments only need to survive quote-wrapping: Task
        // Scheduler does not run the action through cmd.exe.
        for bad in [
            r#"C:\a"b\config.toml"#,
            "C:\\a\nconfig.toml",
            "C:\\a\rconfig.toml",
            r"C:\cfg\",
        ] {
            assert!(
                build_handover_task(base, bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
        // The exe path keeps the stricter set because it is embedded as the
        // task Command and run directly.
        for bad in [r#"C:\a"b.exe"#, r"C:\a&b.exe", r"C:\app\"] {
            assert!(
                build_handover_task(bad, r"C:\cfg\config.toml").is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn handover_task_accepts_valid_profile_characters() {
        // Legal in Windows user/profile names and inert to CreateProcess
        // because the path is quote-wrapped in Arguments.
        for config in [
            r"C:\Users\Tom & Jerry\AppData\Roaming\framework-crate\config.toml",
            r"C:\Users\John (Work)\AppData\Roaming\framework-crate\config.toml",
            r"C:\Users\O'Brien\AppData\Roaming\framework-crate\config.toml",
            r"C:\Users\Jane!Doe\AppData\Roaming\framework-crate\config.toml",
        ] {
            assert!(
                build_handover_task(
                    r"C:\Program Files\framework-crate\framework-crate.exe",
                    config
                )
                .is_ok(),
                "expected acceptance for {config:?}"
            );
        }
    }

    #[test]
    fn handover_task_accepts_paths_with_spaces() {
        assert!(
            build_handover_task(
                r"C:\Program Files\framework-crate\framework-crate.exe",
                r"C:\Users\Jane Doe\AppData\Roaming\framework-crate\config.toml"
            )
            .is_ok()
        );
    }
}
