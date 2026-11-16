//! Headless hardware restore for the uninstaller.
//!
//! Runs before the uninstaller removes files. Restores firmware fan control,
//! the EC/BIOS charge limit, and the persisted BIOS CPU power limits. It never
//! deletes or rewrites the user config under `%APPDATA%\framework-crate`.

use crate::cli::{EcClient, EcHw};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run() -> i32 {
    crate::fallback_log("uninstall-cleanup: start");
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_inner());
    });
    let rc = match rx.recv_timeout(CLEANUP_TIMEOUT) {
        Ok(rc) => rc,
        Err(RecvTimeoutError::Timeout) => {
            crate::fallback_log("uninstall-cleanup: timed out; EC may be wedged");
            1
        }
        Err(RecvTimeoutError::Disconnected) => {
            crate::fallback_log("uninstall-cleanup: worker exited without a result");
            1
        }
    };
    crate::fallback_log(&format!("uninstall-cleanup: done rc={rc}"));
    rc
}

fn run_inner() -> i32 {
    let mut rc = 0;

    match EcClient::new() {
        Ok(ec) => {
            if let Err(e) = ec.autofanctrl() {
                crate::fallback_log(&format!("uninstall-cleanup: fan restore failed: {e}"));
                rc = 1;
            } else {
                crate::fallback_log("uninstall-cleanup: fan restored to firmware");
            }

            if let Err(e) = ec.charge_limit_restore() {
                crate::fallback_log(&format!(
                    "uninstall-cleanup: charge limit restore failed: {e}"
                ));
                rc = 1;
            } else {
                match ec.charge_limit_get() {
                    Ok((min, max)) => crate::fallback_log(&format!(
                        "uninstall-cleanup: charge limit disabled; EC reports min={min}% max={max}%"
                    )),
                    Err(e) => crate::fallback_log(&format!(
                        "uninstall-cleanup: charge limit disabled; readback failed: {e}"
                    )),
                }
            }
        }
        Err(e) => {
            crate::fallback_log(&format!("uninstall-cleanup: EC connect failed: {e}"));
            rc = 1;
        }
    }

    match crate::cpu_power::CpuPowerState::restore_persisted_bios_defaults() {
        Ok(()) => crate::fallback_log("uninstall-cleanup: CPU power restored to BIOS defaults"),
        Err(e) => {
            crate::fallback_log(&format!("uninstall-cleanup: CPU power left unchanged: {e}"));
            rc = 1;
        }
    }

    // AMD writes are opt-in and experimental, but the first-run snapshot is
    // the factory profile. Restore it when one was captured. This only reads
    // the config; it never deletes or rewrites it.
    match crate::config::load() {
        Ok(cfg) => {
            let ac = crate::cpu_power::read_ac_present().unwrap_or(true);
            if let Some(initial) = cfg.cpu_power.amd_initial(ac) {
                match crate::cpu_power::apply_amd_limits((*initial).to_request()) {
                    Ok(_) => crate::fallback_log(
                        "uninstall-cleanup: AMD CPU power restored to initial profile",
                    ),
                    Err(e) => {
                        crate::fallback_log(&format!(
                            "uninstall-cleanup: AMD CPU power restore failed: {e}"
                        ));
                        rc = 1;
                    }
                }
            }
        }
        Err(e) => crate::fallback_log(&format!(
            "uninstall-cleanup: config load failed; AMD CPU power untouched: {e}"
        )),
    }

    rc
}
