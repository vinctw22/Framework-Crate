use super::{App, Message, PL_RESET_RETRY_MS, refresh_cpu_power_task, stop_sync_task};
use crate::types::PowerProfile;
use iced::Task;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{error, warn};

fn sync_restart_after_reset(
    was_sync: bool,
    reset_ok: bool,
    previous: Option<crate::cpu_power::PowerLimitParams>,
) -> Option<crate::cpu_power::PowerLimitParams> {
    if was_sync && !reset_ok {
        previous
    } else {
        None
    }
}

impl App {
    pub(crate) fn handle_cpu_power_message(&mut self, message: &Message) -> Option<Task<Message>> {
        match message {
            Message::InstallPawnIO => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                if !self.begin_pawnio_task("Installing PawnIO...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::install_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOInstalled,
                ))
            }
            Message::PawnIOInstalled(result) => {
                self.pawnio_busy = false;
                self.pawnio_status = None;
                if let Err(e) = result {
                    error!("PawnIO install failed: {}", e);
                    self.cpu_power_error = Some(format!("PawnIO install failed: {}", e));
                    self.mark_dirty();
                    Some(Task::none())
                } else {
                    // Reset DLL pointers so next read picks up upgraded DLL.
                    self.cpu_power_error = None;

                    crate::cpu_power::reset_dll_fns();
                    Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {
                        crate::cpu_power::pawnio_version();
                    }))
                }
            }
            Message::DownloadPawnIOModules => {
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                if !self.begin_pawnio_task("Downloading PawnIO Modules...") {
                    return Some(Task::none());
                }
                Some(Task::perform(
                    async {
                        crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || {
                                crate::cpu_power::download_and_extract_modules()
                                    .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(Err)
                    },
                    Message::PawnIOModulesDownloaded,
                ))
            }
            Message::PawnIOModulesDownloaded(result) => {
                self.pawnio_busy = false;
                self.pawnio_status = None;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;

                        Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
                    }
                    Err(e) => {
                        error!("PawnIO Modules download failed: {}", e);
                        self.modules_download_error = Some(e.to_string());
                        self.mark_dirty();
                        Some(Task::none())
                    }
                }
            }
            Message::OpenModulesDir => {
                if let Err(e) = crate::cpu_power::open_modules_dir() {
                    self.modules_download_error = Some(e);
                    self.mark_dirty();
                }
                Some(Task::none())
            }
            Message::RedetectModules => {
                let ok = crate::cpu_power::redetect_modules(
                    self.state.system.amd_cpu.load(Ordering::Acquire),
                );
                if ok {
                    self.modules_download_error = None;

                    Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
                } else {
                    self.modules_download_error = Some("Modules not found or hash mismatch".into());
                    self.mark_dirty();
                    Some(Task::none())
                }
            }
            Message::UpdatePawnIOAll => {
                tracing::info!("UpdatePawnIOAll triggered");
                if !self
                    .begin_pawnio_task("Updating PawnIO & Modules... Please wait up to 2 minutes")
                {
                    return Some(Task::none());
                }
                let amd_cpu = self.state.system.amd_cpu.load(Ordering::Acquire);
                Some(Task::perform(
                    async move {
                        let r1 = crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            || crate::cpu_power::update_pawnio().map_err(|e| e.to_string()),
                        )
                        .await
                        .unwrap_or_else(Err);
                        let r2 = crate::util::spawn_blocking_with_timeout(
                            crate::util::PAWNIO_INSTALL_TIMEOUT,
                            move || {
                                crate::cpu_power::update_pawnio_modules(amd_cpu)
                                    .map_err(|e| e.to_string())
                            },
                        )
                        .await
                        .unwrap_or_else(Err);
                        match (r1, r2) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(e1), Err(e2)) => Err(format!("PawnIO: {}; Modules: {}", e1, e2)),
                            (Err(e), _) => Err(format!("PawnIO: {}", e)),
                            (_, Err(e)) => Err(format!("Modules: {}", e)),
                        }
                    },
                    Message::UpdatePawnIOAllDone,
                ))
            }
            Message::UpdatePawnIOAllDone(result) => {
                self.pawnio_busy = false;
                self.pawnio_status = None;
                match result {
                    Ok(()) => {
                        self.modules_download_error = None;
                        self.cpu_power_error = None;

                        crate::cpu_power::reset_dll_fns();
                        return Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}));
                    }
                    Err(e) => {
                        error!("Update PawnIO & Modules failed: {}", e);
                        self.modules_download_error = Some(e.clone());
                        self.cpu_power_error = Some(e.clone());
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1Changed(val) => {
                self.pl1_edit = val.clone();

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2Changed(val) => {
                self.pl2_edit = val.clone();

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1TimeChanged(val) => {
                self.pl1_time_edit = val.clone();

                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1EnabledToggled(v) => {
                self.pl1_enabled = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2EnabledToggled(v) => {
                self.pl2_enabled = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl1ClampedToggled(v) => {
                self.pl1_clamped = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerPl2ClampedToggled(v) => {
                self.pl2_clamped = *v;

                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerApply => {
                // Belt and suspenders: the controls only render when a live
                // snapshot exists, but the handler re-checks so a stale or
                // raced message can never reach the write path.
                if !self.cpu_power_supported() || !self.state.cpu_power.snapshot().available {
                    return Some(Task::none());
                }
                let profile = match self.profile_from_inputs() {
                    Ok(profile) => profile,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                // Apply always saves the selected profile, even when it is not
                // the active source; only a matching source writes the MSR.
                self.store_profile(self.pl_profile_is_ac, profile.clone());
                self.save_config();
                self.cpu_power_error = None;
                self.cpu_power_status = None;
                if self
                    .current_power_source()
                    .is_some_and(|ac| ac != self.pl_profile_is_ac)
                {
                    self.cpu_power_status = Some(format!(
                        "Saved {} profile; it will apply on {}.",
                        source_label(self.pl_profile_is_ac),
                        source_label(self.pl_profile_is_ac)
                    ));
                    self.mark_dirty();
                    return Some(Task::none());
                }
                let params = match self.profile_params(&profile) {
                    Ok(params) => params,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                // Invalidate an older Start/Stop/Reset task: this explicit
                // Apply is the newest sync-affecting intent.
                self.pl_reset_disable_keep = false;
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                self.pl_profile_busy = true;
                self.pl_profile_retry_failures = 0;
                self.pl_profile_retry_at_ms =
                    crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                Some(self.apply_profile_task(self.pl_profile_is_ac, params, intent))
            }
            Message::AmdStapmChanged(val) => {
                self.amd_stapm_edit = val.clone();
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdFastChanged(val) => {
                self.amd_fast_edit = val.clone();
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdSlowChanged(val) => {
                self.amd_slow_edit = val.clone();
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdTctlChanged(val) => {
                self.amd_tctl_edit = val.clone();
                self.cpu_power_error = None;
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdWriteEnabledToggled(enabled) => {
                if self.amd_preview {
                    return Some(Task::none());
                }
                self.mutate_config(|cfg| cfg.cpu_power.amd_write_experimental = *enabled);
                self.save_config();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdCopyDiagnostics => {
                let info = self.state.cpu_power.amd_snapshot();
                if !info.available {
                    self.cpu_power_error = Some("AMD diagnostics unavailable".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                Some(iced::clipboard::write(info.diagnostics_text()))
            }
            Message::AmdApply => {
                if self.amd_preview {
                    self.cpu_power_error = Some("AMD preview mode: writes disabled".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                if !self.state.system.amd_cpu.load(Ordering::Acquire)
                    || self.amd_apply_busy
                    || !self.state.cpu_power.amd_snapshot().available
                {
                    return Some(Task::none());
                }
                if !self.amd_write_enabled() {
                    self.cpu_power_error = Some("Enable experimental AMD writes first".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                self.amd_reset_disable_keep = false;
                self.amd_apply_failures = 0;
                let profile = match self.parse_amd_profile() {
                    Ok(profile) => profile,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                let ac = self.amd_profile_is_ac;
                // Apply always saves the selected profile; writing only happens
                // when the selected source is active (or still unknown).
                self.store_amd_profile(ac, profile);
                self.save_config();
                self.cpu_power_error = None;
                if self
                    .current_power_source()
                    .is_some_and(|current| current != ac)
                {
                    self.cpu_power_status = Some(format!(
                        "Saved {} AMD profile; it will apply on {}.",
                        source_label(ac),
                        source_label(ac)
                    ));
                    self.mark_dirty();
                    return Some(Task::none());
                }
                self.amd_apply_busy = true;
                let request = profile.to_request();
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(self.amd_apply_task(ac, request, intent))
            }
            Message::AmdReset => {
                if self.amd_preview {
                    self.cpu_power_error = Some("AMD preview mode: writes disabled".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                if !self.state.system.amd_cpu.load(Ordering::Acquire)
                    || self.amd_apply_busy
                    || !self.state.cpu_power.amd_snapshot().available
                {
                    return Some(Task::none());
                }
                if !self.amd_write_enabled() {
                    self.cpu_power_error = Some("Enable experimental AMD writes first".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                let ac = self
                    .current_power_source()
                    .unwrap_or(self.amd_profile_is_ac);
                let Some(initial) = self.saved_amd_initial(ac) else {
                    self.cpu_power_error =
                        Some(format!("No {} AMD baseline captured yet", source_label(ac)));
                    self.mark_dirty();
                    return Some(Task::none());
                };
                self.amd_apply_busy = true;
                self.amd_reset_disable_keep = true;
                self.amd_apply_failures = 0;
                self.cpu_power_error = None;
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(self.amd_apply_task(ac, initial.to_request(), intent))
            }
            Message::AmdApplied(ac, intent, result) => {
                if *intent != self.sync_intent.load(Ordering::Acquire) {
                    // A Stop/import/newer Apply owns the state now.
                    return Some(Task::none());
                }
                self.amd_apply_busy = false;
                match result {
                    Ok(info) => {
                        let info = info.clone();
                        self.amd_fields_primed = true;
                        self.amd_apply_failures = 0;
                        if self.amd_reset_disable_keep {
                            self.amd_reset_disable_keep = false;
                            self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                            self.save_config();
                            self.amd_active_source = None;
                            self.amd_last_reassert_ms = 0;
                            self.cpu_power_status =
                                Some("Reset complete; keep-alive off.".to_string());
                        } else {
                            self.amd_active_source = Some(*ac);
                            self.amd_last_reassert_ms = crate::util::monotonic_ms();
                            self.cpu_power_status =
                                Some(format!("{} AMD profile applied.", source_label(*ac)));
                        }
                        if self.amd_profile_is_ac == *ac {
                            self.apply_amd_fields_from_snapshot(&info);
                        }
                        self.state.cpu_power.set_amd_info(info);
                        self.cpu_power_error = None;
                    }
                    Err(e) if e == PROFILE_SKIPPED_SOURCE_CHANGED => {
                        self.amd_reset_disable_keep = false;
                        self.amd_active_source = None;
                        self.amd_last_reassert_ms = 0;
                        self.amd_apply_failures = 0;
                        self.cpu_power_status = Some(
                            "Power source changed; applying the matching AMD profile.".to_string(),
                        );
                        self.cpu_power_error = None;
                    }
                    Err(e) => {
                        self.amd_reset_disable_keep = false;
                        error!("AMD power apply failed: {}", e);
                        self.amd_apply_failures = self.amd_apply_failures.saturating_add(1);
                        if self.amd_apply_failures >= 3 {
                            self.cpu_power_status = Some(
                                "AMD auto-apply failed 3 times; press Apply to retry.".to_string(),
                            );
                        }
                        self.cpu_power_error = Some(e.to_string());
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::AmdProfileSourceChanged(ac) => {
                self.amd_profile_is_ac = *ac;
                self.amd_profile_user_selected = true;
                self.cpu_power_status = None;
                self.cpu_power_error = None;
                self.load_amd_fields_for_source();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerProfileSourceChanged(ac) => {
                self.pl_profile_is_ac = *ac;
                self.pl_profile_user_selected = true;
                self.cpu_power_status = None;
                self.cpu_power_error = None;
                self.load_pl_fields_for_source();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerProfileApplied(ac, intent, result) => {
                if *intent != self.sync_intent.load(Ordering::Acquire) {
                    // A newer Apply/Start/Stop/Reset already owns the state.
                    return Some(Task::none());
                }
                self.pl_profile_busy = false;
                match result {
                    Ok(()) => {
                        self.pl_active_source = Some(*ac);
                        self.pl_profile_retry_at_ms = 0;
                        self.pl_profile_retry_failures = 0;
                        self.set_pl_custom_applied(true);
                        self.cpu_power_error = None;
                        self.cpu_power_status =
                            Some(format!("{} profile applied.", source_label(*ac)));
                        if self.pl_profile_is_ac == *ac {
                            self.load_pl_fields_for_source();
                        }
                        return Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}));
                    }
                    Err(e) if e == PROFILE_SKIPPED_SOURCE_CHANGED => {
                        // Let the next tick pick up the profile for the new
                        // source immediately instead of showing a red error.
                        self.pl_active_source = None;
                        self.pl_profile_retry_at_ms = 0;
                        self.pl_profile_retry_failures = 0;
                        self.cpu_power_status = Some(
                            "Power source changed; applying the matching profile.".to_string(),
                        );
                        self.cpu_power_error = None;
                    }
                    Err(e) if e == PROFILE_SKIPPED_STALE_INTENT => {
                        // Superseded by a newer action; leave its state alone.
                    }
                    Err(e) => {
                        error!("Failed to apply {} power profile: {}", source_label(*ac), e);
                        self.cpu_power_error = Some(e.to_string());
                        self.pl_profile_retry_failures =
                            self.pl_profile_retry_failures.saturating_add(1);
                        if self.pl_profile_retry_failures >= MAX_PROFILE_AUTO_RETRIES {
                            self.pl_profile_retry_at_ms = 0;
                            self.cpu_power_status = Some(
                                "Profile apply failed 3 times; press Apply to retry.".to_string(),
                            );
                        } else {
                            self.pl_profile_retry_at_ms =
                                crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                        }
                    }
                }
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStart => {
                if self.state.system.amd_cpu.load(Ordering::Acquire) {
                    if self.amd_preview {
                        self.cpu_power_error =
                            Some("AMD preview mode: writes disabled".to_string());
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                    if self.amd_apply_busy || !self.state.cpu_power.amd_snapshot().available {
                        return Some(Task::none());
                    }
                    if !self.amd_write_enabled() {
                        self.cpu_power_error =
                            Some("Enable experimental AMD writes first".to_string());
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                    let ac = self
                        .current_power_source()
                        .unwrap_or(self.amd_profile_is_ac);
                    let saved = self
                        .saved_amd_profile(ac)
                        .or_else(|| self.saved_amd_initial(ac));
                    // Only capture the visible edit fields when they belong to
                    // the source being started; otherwise enabling keep-alive
                    // while viewing the other profile would copy it across.
                    let profile = if self.amd_profile_is_ac == ac {
                        saved.or_else(|| self.parse_amd_profile().ok())
                    } else {
                        saved
                    };
                    let Some(profile) = profile else {
                        self.cpu_power_error = Some(format!(
                            "No {} AMD profile saved; select it and press Apply first",
                            source_label(ac)
                        ));
                        self.mark_dirty();
                        return Some(Task::none());
                    };
                    self.store_amd_profile(ac, profile);
                    self.mutate_config(|cfg| cfg.cpu_power.keep_limits = true);
                    self.save_config();
                    self.amd_profile_is_ac = ac;
                    self.amd_profile_user_selected = true;
                    self.amd_apply_busy = true;
                    self.amd_last_reassert_ms = crate::util::monotonic_ms();
                    self.amd_apply_failures = 0;
                    self.cpu_power_error = None;
                    self.cpu_power_status = Some("AMD keep-alive on.".to_string());
                    let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                    return Some(self.amd_apply_task(ac, profile.to_request(), intent));
                }
                if !self.cpu_power_supported() || !self.state.cpu_power.snapshot().available {
                    return Some(Task::none());
                }
                let ac = self.current_power_source().unwrap_or(self.pl_profile_is_ac);
                // When the card is editing the active source, enabling the
                // toggle captures the current fields (unsaved edits included).
                // Otherwise use the saved profile for the active source.
                let profile = if self.pl_profile_is_ac == ac {
                    match self.profile_from_inputs() {
                        Ok(profile) => profile,
                        Err(e) => {
                            self.cpu_power_error = Some(e);
                            self.mark_dirty();
                            return Some(Task::none());
                        }
                    }
                } else {
                    match self.saved_profile(ac) {
                        Some(profile) => profile,
                        None => {
                            self.cpu_power_error =
                                Some(format!("No {} profile saved yet.", source_label(ac)));
                            self.mark_dirty();
                            return Some(Task::none());
                        }
                    }
                };
                let params = match self.profile_params(&profile) {
                    Ok(params) => params,
                    Err(e) => {
                        self.cpu_power_error = Some(e);
                        self.mark_dirty();
                        return Some(Task::none());
                    }
                };
                self.store_profile(ac, profile);
                self.mutate_config(|cfg| cfg.cpu_power.keep_limits = true);
                self.save_config();
                self.pl_profile_is_ac = ac;
                self.pl_profile_user_selected = true;
                self.cpu_power_error = None;
                self.cpu_power_status = Some("Keep-alive on.".to_string());
                self.pl_profile_busy = true;
                self.pl_profile_retry_failures = 0;
                self.pl_profile_retry_at_ms =
                    crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS);
                self.pl_reset_disable_keep = false;
                // Invalidate an older Stop/Reset queued before this start.
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(self.apply_profile_task(ac, params, intent))
            }
            Message::CpuPowerSyncStop => {
                if self.state.system.amd_cpu.load(Ordering::Acquire) {
                    self.sync_intent.fetch_add(1, Ordering::AcqRel);
                    self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                    self.save_config();
                    self.amd_active_source = None;
                    self.amd_last_reassert_ms = 0;
                    self.amd_reset_disable_keep = false;
                    self.amd_apply_busy = false;
                    self.amd_apply_failures = 0;
                    self.cpu_power_status = Some("AMD keep-alive off.".to_string());
                    self.mark_dirty();
                    return Some(Task::none());
                }
                if !self.cpu_power_supported() {
                    return Some(Task::none());
                }
                self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                self.save_config();
                self.pl_active_source = None;
                self.pl_profile_busy = false;
                self.pl_profile_retry_failures = 0;
                self.pl_reset_disable_keep = false;
                self.cpu_power_status = Some("Keep-alive off.".to_string());
                self.mark_dirty();
                let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
                Some(stop_sync_task(
                    self.state.cpu_power.clone(),
                    Arc::clone(&self.sync_intent),
                    intent,
                ))
            }
            Message::CpuPowerSyncReset => {
                // A queued Start/Stop from before this Reset must not run after it.
                self.sync_intent.fetch_add(1, Ordering::AcqRel);
                self.pl_profile_busy = false;
                // A successful manual Reset also turns the persisted
                // keep-alive policy off, so the app does not immediately
                // re-apply the profile behind the user's back.
                self.pl_reset_disable_keep = true;
                Some(self.handle_cpu_power_sync_reset())
            }
            Message::CpuPowerResetDone(intent, result) => {
                if *intent != self.sync_intent.load(Ordering::Acquire) {
                    // A newer Apply/Start/Stop/import owns the state.
                    return Some(Task::none());
                }
                match result {
                    Ok(()) => {
                        self.cpu_power_error = None;
                        self.set_pl_custom_applied(false);
                        self.state
                            .lifecycle
                            .pl_reset_pending
                            .store(false, Ordering::Release);
                        self.state
                            .lifecycle
                            .pl_reset_retry_at_ms
                            .store(0, Ordering::Release);
                        if self.pl_reset_disable_keep {
                            self.pl_reset_disable_keep = false;
                            self.mutate_config(|cfg| cfg.cpu_power.keep_limits = false);
                            self.save_config();
                            self.pl_active_source = None;
                            self.cpu_power_status =
                                Some("Reset complete; keep-alive off.".to_string());
                        } else {
                            self.cpu_power_status = Some("Reset to BIOS defaults.".to_string());
                        }
                    }
                    Err(e) => {
                        self.pl_reset_disable_keep = false;
                        warn!("CPU power reset failed: {}", e);
                        self.cpu_power_error = Some(format!("Reset failed: {}", e));
                        self.state.lifecycle.pl_reset_retry_at_ms.store(
                            crate::util::monotonic_ms().saturating_add(PL_RESET_RETRY_MS),
                            Ordering::Release,
                        );
                    }
                }
                self.mark_dirty();
                Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
            }
            Message::CpuPowerDataRefreshed => {
                let info = self.state.cpu_power.snapshot();
                self.pl_capture_in_flight = false;
                if info.available {
                    if !self.pl_profile_user_selected {
                        // Follow the live source until the user picks a profile.
                        self.pl_profile_is_ac =
                            self.current_power_source().unwrap_or(self.pl_profile_is_ac);
                    }
                    if let Some(ac) = self.current_power_source() {
                        self.pl_capture_source = Some(ac);
                        // Record the firmware wall for this source, updating
                        // only when the MMIO mirror actually changed.
                        self.capture_initial_profile(ac, &info);
                    }
                }
                self.load_pl_fields_for_source();
                if info.available {
                    // Latched once live hardware has populated the boxes;
                    // the tick loop uses it to fire exactly one catch-up
                    // refresh when hardware appears without a message.
                    self.pl_fields_primed = true;
                } else {
                    // Hardware dropped: reopen the latch so the next
                    // appearance fires a catch-up refresh again.
                    self.pl_fields_primed = false;
                }
                self.infer_pl_custom_applied();
                self.mark_dirty();
                Some(Task::none())
            }
            Message::CpuPowerSyncStopped => {
                self.mark_dirty();
                Some(Task::none())
            }
            Message::RefreshCpuPower => {
                // Retry must bypass the 60s Unavailable TTL, otherwise a
                // transient PowerShell failure makes the button a no-op.
                crate::cpu_power::invalidate_authenticode();
                Some(refresh_cpu_power_task(self.state.cpu_power.clone(), || {}))
            }
            _ => None,
        }
    }
}

fn source_label(ac: bool) -> &'static str {
    if ac { "AC" } else { "Battery" }
}

fn mock_amd_profile(ac: bool) -> crate::types::AmdPowerProfile {
    if ac {
        crate::types::AmdPowerProfile {
            stapm_watts: 25.0,
            fast_watts: 45.0,
            slow_watts: 35.0,
            tctl_c: 90.0,
        }
    } else {
        crate::types::AmdPowerProfile {
            stapm_watts: 15.0,
            fast_watts: 30.0,
            slow_watts: 25.0,
            tctl_c: 85.0,
        }
    }
}

/// Sentinel returned when the power source changes while a profile write is
/// waiting on the hardware mutex; the write must not land on the new source.
const PROFILE_SKIPPED_SOURCE_CHANGED: &str = "power source changed before the profile write";
/// Sentinel for an apply superseded by a newer start/stop/reset.
const PROFILE_SKIPPED_STALE_INTENT: &str = "profile apply superseded by a newer action";

/// Automatic profile retries before waiting for a manual Apply.
const MAX_PROFILE_AUTO_RETRIES: u32 = 3;

/// True when the live source differs from the one whose profile is active.
fn profile_switch_due(active: Option<bool>, current: Option<bool>, busy: bool) -> bool {
    current.is_some() && active != current && !busy
}

fn auto_retry_allowed(failures: u32, now_ms: u64, retry_at_ms: u64) -> bool {
    failures < MAX_PROFILE_AUTO_RETRIES && now_ms >= retry_at_ms
}

/// The MMIO mirror tracking the MSR value suggests it is reflecting a custom
/// write rather than the firmware's own wall.
fn mmio_matches_msr(info: &crate::cpu_power::CpuPowerInfo) -> bool {
    (info.pl1_mmio - info.pl1_msr).abs() <= 0.05 && (info.pl2_mmio - info.pl2_msr).abs() <= 0.05
}

impl App {
    pub(crate) fn cpu_power_supported(&self) -> bool {
        self.state.system.intel_cpu.load(Ordering::Acquire)
    }

    /// Whether the About keep-alive toggle is enabled (persisted intent).
    pub(crate) fn keep_limits_enabled(&self) -> bool {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .keep_limits
    }

    fn saved_profile(&self, ac: bool) -> Option<PowerProfile> {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .profile(ac)
            .cloned()
    }

    fn saved_initial(&self, ac: bool) -> Option<PowerProfile> {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .initial(ac)
            .cloned()
    }

    fn store_profile(&mut self, ac: bool, profile: PowerProfile) {
        self.mutate_config(|cfg| *cfg.cpu_power.profile_mut(ac) = Some(profile));
    }

    /// Records the MMIO wall seen for a source, updating the stored default
    /// only when the mirror actually changes (e.g. after a BIOS update).
    fn capture_initial_profile(&mut self, ac: bool, info: &crate::cpu_power::CpuPowerInfo) {
        let Some(profile) = crate::cpu_power::initial_profile_from_mmio(info) else {
            return;
        };
        // Do not record a wall the writer would reject (e.g. a zero time
        // window); it would only make the automatic apply retry forever.
        if crate::cpu_power::validate_limit_params(&crate::cpu_power::params_for_profile(
            &profile, info,
        ))
        .is_err()
        {
            return;
        }
        // If a custom MSR write is active and the mirror tracks it exactly,
        // treat the value as the user's own limit, not a firmware baseline.
        if self.pl_custom_applied.load(Ordering::Acquire) && mmio_matches_msr(info) {
            tracing::debug!(
                "Skipping {} MMIO wall capture while custom limits mirror the MSR",
                source_label(ac)
            );
            return;
        }
        let previous = self.saved_initial(ac);
        if previous.as_ref() == Some(&profile) {
            return;
        }
        match previous {
            Some(old) => tracing::info!(
                "Updated {} MMIO power wall: PL1 {:.1}W ({:.1}s) -> {:.1}W ({:.1}s), \
                 PL2 {:.1}W -> {:.1}W",
                source_label(ac),
                old.pl1_watts,
                old.pl1_time_s,
                profile.pl1_watts,
                profile.pl1_time_s,
                old.pl2_watts,
                profile.pl2_watts
            ),
            None => tracing::info!(
                "Recorded {} MMIO power wall: PL1={:.1}W ({:.1}s, en={}, clamped={}) \
                 PL2={:.1}W (en={}, clamped={})",
                source_label(ac),
                profile.pl1_watts,
                profile.pl1_time_s,
                profile.pl1_enabled,
                profile.pl1_clamped,
                profile.pl2_watts,
                profile.pl2_enabled,
                profile.pl2_clamped
            ),
        }
        self.mutate_config(|cfg| *cfg.cpu_power.initial_mut(ac) = Some(profile));
        self.save_config();
        self.mark_dirty();
        // A freshly captured wall is the default profile for this source:
        // let the next tick apply it when keep-alive is on and the user has
        // not saved a profile yet.
        if self.keep_limits_enabled()
            && self.current_power_source() == Some(ac)
            && self.saved_profile(ac).is_none()
        {
            self.pl_active_source = None;
        }
    }

    fn profile_from_inputs(&self) -> Result<PowerProfile, String> {
        let (pl1, pl2, pl1_time) = self.validate_cpu_power_inputs()?;
        Ok(PowerProfile {
            pl1_watts: pl1,
            pl1_enabled: self.pl1_enabled,
            pl1_clamped: self.pl1_clamped,
            pl1_time_s: pl1_time,
            pl2_watts: pl2,
            pl2_enabled: self.pl2_enabled,
            pl2_clamped: self.pl2_clamped,
        })
    }

    fn profile_params(
        &self,
        profile: &PowerProfile,
    ) -> Result<crate::cpu_power::PowerLimitParams, String> {
        let info = self.state.cpu_power.snapshot();
        let params = crate::cpu_power::params_for_profile(profile, &info);
        crate::cpu_power::validate_limit_params(&params).map(|()| params)
    }

    fn show_profile_fields(&mut self, profile: &PowerProfile) {
        self.pl1_edit = format!("{:.1}", profile.pl1_watts);
        self.pl2_edit = format!("{:.1}", profile.pl2_watts);
        self.pl1_time_edit = format!("{:.1}", profile.pl1_time_s);
        self.pl1_enabled = profile.pl1_enabled;
        self.pl2_enabled = profile.pl2_enabled;
        self.pl1_clamped = profile.pl1_clamped;
        self.pl2_clamped = profile.pl2_clamped;
    }

    /// Loads the selected profile into the edit boxes: the user's saved
    /// profile first, then the source's one-time MMIO capture, then the live
    /// registers. This keeps the default wall aligned with what the firmware
    /// enforces when plugged in or on battery.
    pub(crate) fn load_pl_fields_for_source(&mut self) {
        if let Some(profile) = self
            .saved_profile(self.pl_profile_is_ac)
            .or_else(|| self.saved_initial(self.pl_profile_is_ac))
        {
            self.show_profile_fields(&profile);
        } else {
            let info = self.state.cpu_power.snapshot();
            self.apply_edit_fields_from_snapshot(&info);
        }
    }

    pub(crate) fn current_power_source(&self) -> Option<bool> {
        crate::cpu_power::read_ac_present()
    }

    fn apply_profile_task(
        &self,
        ac: bool,
        params: crate::cpu_power::PowerLimitParams,
        intent: u64,
    ) -> Task<Message> {
        let cpu_power = self.state.cpu_power.clone();
        let config = Arc::clone(&self.state.lifecycle.config);
        let sync_intent = Arc::clone(&self.sync_intent);
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_SYNC_TIMEOUT,
                    move || {
                        if sync_intent.load(Ordering::Acquire) != intent {
                            return Err(PROFILE_SKIPPED_STALE_INTENT.to_string());
                        }
                        // The source can change while this task waits for the
                        // mutex; never write the old source's profile onto the
                        // new state. Leave sync untouched in that case.
                        if crate::cpu_power::read_ac_present().is_some_and(|current| current != ac)
                        {
                            return Err(PROFILE_SKIPPED_SOURCE_CHANGED.to_string());
                        }
                        let was_sync = cpu_power.sync_enabled.load(Ordering::Acquire);
                        let previous_sync = cpu_power.desired_sync_params();
                        cpu_power.stop_sync();
                        let result = crate::cpu_power::write_msr_pl1_pl2_public(params)
                            .map_err(|e| e.to_string());
                        if result.is_ok() {
                            // Re-read the toggle after taking the mutex: a Stop
                            // clicked while this task waited must win.
                            let keep = crate::util::read_lock(&config).cpu_power.keep_limits;
                            if (keep || was_sync)
                                && let Err(e) = cpu_power.start_sync(params)
                            {
                                warn!("Failed to restart CPU power sync after profile apply: {e}");
                            }
                        } else if was_sync
                            && let Some(previous) = previous_sync
                            && let Err(e) = cpu_power.start_sync(previous)
                        {
                            warn!("Failed to restore CPU power sync after profile apply: {e}");
                        }
                        result
                    },
                )
                .await
                .unwrap_or_else(Err);
                Message::CpuPowerProfileApplied(ac, intent, result)
            },
            |msg| msg,
        )
    }

    /// Applies the saved profile for the current power source when the
    /// keep-alive toggle is on and the source changed (or at startup). A
    /// source without a profile falls back to the existing BIOS reset path.
    pub(crate) fn maybe_auto_apply_profile(&mut self, now_ms: u64) -> Option<Task<Message>> {
        if !self.cpu_power_supported()
            || !self.state.cpu_power.snapshot().available
            || !self.keep_limits_enabled()
        {
            return None;
        }
        let current = self.current_power_source();
        if !profile_switch_due(self.pl_active_source, current, self.pl_profile_busy) {
            return None;
        }
        let ac = current?;
        if !auto_retry_allowed(
            self.pl_profile_retry_failures,
            now_ms,
            self.pl_profile_retry_at_ms,
        ) {
            return None;
        }
        // Use the user profile when present, otherwise the one-time MMIO wall
        // recorded for this source.
        let Some(profile) = self.saved_profile(ac).or_else(|| self.saved_initial(ac)) else {
            self.pl_active_source = Some(ac);
            if self.pl_custom_applied.load(Ordering::Acquire) {
                // Never leave the previous source's limits enforced after a
                // switch: fall back to the factory values like the legacy
                // AC→battery path.
                self.state
                    .lifecycle
                    .pl_reset_pending
                    .store(true, Ordering::Release);
                self.cpu_power_status = Some(format!(
                    "No {} profile saved; resetting to BIOS defaults.",
                    source_label(ac)
                ));
            } else {
                self.cpu_power_status = Some(format!("No {} profile saved yet.", source_label(ac)));
            }
            self.mark_dirty();
            return None;
        };
        let params = match self.profile_params(&profile) {
            Ok(params) => params,
            Err(e) => {
                self.cpu_power_error = Some(e);
                self.pl_profile_retry_at_ms = now_ms.saturating_add(PL_RESET_RETRY_MS);
                self.mark_dirty();
                return None;
            }
        };
        self.cpu_power_error = None;
        self.pl_profile_busy = true;
        self.pl_profile_retry_at_ms = now_ms.saturating_add(PL_RESET_RETRY_MS);
        let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
        Some(self.apply_profile_task(ac, params, intent))
    }

    /// Updates the in-memory flag and persists it so an app restart on battery
    /// still knows that a custom MSR value may be active.
    pub(crate) fn set_pl_custom_applied(&mut self, applied: bool) {
        if self.pl_custom_applied.swap(applied, Ordering::Release) == applied {
            return;
        }
        self.mutate_config(|cfg| cfg.pl_custom_applied = applied);
        self.save_config();
    }

    /// Recover the memory-only custom-limit flag by comparing the live
    /// registers with the persisted BIOS snapshot. Called at startup and on
    /// EC recovery so an app restart does not disarm the AC->battery reset.
    pub(crate) fn infer_pl_custom_applied(&mut self) {
        if !self.cpu_power_supported() {
            return;
        }
        // Only compare on AC. Firmware may apply different limits on battery,
        // and treating that as a user custom write would make the AC->battery
        // reset overwrite the firmware's battery-mode limits with AC values.
        if crate::cpu_power::read_ac_present() != Some(true) {
            return;
        }
        let info = self.state.cpu_power.snapshot();
        let custom = self
            .state
            .cpu_power
            .bios_defaults()
            .is_some_and(|bios| crate::cpu_power::pl_limits_differ_from_bios(&info, &bios));
        self.set_pl_custom_applied(custom);
    }

    /// Marks a PawnIO install/download/update as in flight. Returns false when
    /// one is already running, so repeated clicks cannot spawn overlapping
    /// blocking tasks (and UAC prompts).
    fn begin_pawnio_task(&mut self, status: &str) -> bool {
        if self.pawnio_busy {
            return false;
        }
        self.pawnio_busy = true;
        self.pawnio_status = Some(status.to_string());
        // A retry should not keep showing the previous failure next to the new
        // in-progress status.
        self.modules_download_error = None;
        self.cpu_power_error = None;
        self.mark_dirty();
        true
    }

    pub(crate) fn handle_cpu_power_sync_reset(&mut self) -> Task<Message> {
        if !self.cpu_power_supported() {
            // A manual Reset that never starts must not leave the flag armed
            // for a later automatic reset.
            self.pl_reset_disable_keep = false;
            return Task::none();
        }
        self.cpu_power_error = None;
        let bios = match self.state.cpu_power.bios_defaults() {
            Some(b) => b,
            None => {
                self.pl_reset_disable_keep = false;
                self.cpu_power_error = Some(
                    "Cannot reset: BIOS defaults not available (PawnIO may not be running)".into(),
                );
                self.mark_dirty();
                return Task::none();
            }
        };
        let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
        let cpu_power = self.state.cpu_power.clone();
        Task::perform(
            async move {
                let _guard = crate::util::cpu_power_mutex().lock().await;
                let write_result = crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_SYNC_TIMEOUT,
                    move || {
                        // Re-read intent after acquiring the mutex so a Stop
                        // pressed while we waited is not undone.
                        let was_sync = cpu_power.sync_enabled.load(Ordering::Acquire);
                        let previous_sync = cpu_power.desired_sync_params();
                        cpu_power.stop_sync();
                        let result =
                            crate::cpu_power::write_bios_defaults(&bios).map_err(|e| e.to_string());
                        if let Some(params) =
                            sync_restart_after_reset(was_sync, result.is_ok(), previous_sync)
                            && let Err(e) = cpu_power.start_sync(params)
                        {
                            warn!("Failed to restart CPU power sync after reset: {}", e);
                        }
                        result
                    },
                )
                .await;
                let result = match write_result {
                    Ok(inner) => inner,
                    Err(e) => Err(e),
                };
                Message::CpuPowerResetDone(intent, result)
            },
            |msg| msg,
        )
    }

    /// Validates CPU power inputs; returns Ok((pl1, pl2, pl1_time)) or Err.
    ///
    /// String parsing and finiteness are UI-specific. All range/order rules
    /// defer to the write boundary (`validate_limit_values`) so the UI
    /// verdict and the MSR encoder can never drift apart. Units stay
    /// unchecked here: they come from the live snapshot and may be zero
    /// before the first read; `validate_limit_params` re-checks everything,
    /// including units, at write time.
    pub(crate) fn validate_cpu_power_inputs(&self) -> Result<(f64, f64, f64), String> {
        let pl1: f64 = self
            .pl1_edit
            .parse()
            .map_err(|_| "PL1 is not a valid number".to_string())?;
        let pl2: f64 = self
            .pl2_edit
            .parse()
            .map_err(|_| "PL2 is not a valid number".to_string())?;
        let pl1_time: f64 = self
            .pl1_time_edit
            .parse()
            .map_err(|_| "PL1 time is not a valid number".to_string())?;
        if !pl1.is_finite() {
            return Err("PL1 must be a finite number".to_string());
        }
        if !pl2.is_finite() {
            return Err("PL2 must be a finite number".to_string());
        }
        if !pl1_time.is_finite() {
            return Err("PL1 time must be a finite number".to_string());
        }
        // Same construction as the Apply/SyncStart handlers below (snapshot
        // supplies the fields the edit boxes do not cover). Before the first
        // successful read there is no snapshot yet (Apply no-ops then too),
        // so judge the inputs symmetrically instead of failing on the zeroed
        // placeholder PL2 time.
        let info = self.state.cpu_power.snapshot();
        let pl2_time = if info.available {
            info.pl2_time_s
        } else {
            pl1_time
        };
        let params = crate::cpu_power::PowerLimitParams {
            pl1_watts: pl1,
            pl1_enabled: self.pl1_enabled,
            pl1_clamped: self.pl1_clamped,
            pl1_time_s: pl1_time,
            pl2_watts: pl2,
            pl2_enabled: self.pl2_enabled,
            pl2_clamped: self.pl2_clamped,
            pl2_time_s: pl2_time,
            max_time_s: info.max_time_s,
            power_unit: info.power_unit,
            time_unit: info.time_unit,
        };
        crate::cpu_power::validate_limit_values(&params)?;
        Ok((pl1, pl2, pl1_time))
    }

    /// Populates edit fields from CPU power snapshot. Always overwrites:
    /// this only runs on explicit refresh paths (Refresh button, Apply /
    /// Reset completion, module events), never on the background poll, so
    /// in-progress typing is never clobbered while the card always shows
    /// the latest hardware values after hardware-changing operations.
    fn amd_write_enabled(&self) -> bool {
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .amd_write_experimental
    }

    fn saved_amd_profile(&self, ac: bool) -> Option<crate::types::AmdPowerProfile> {
        if self.amd_preview_full {
            return Some(mock_amd_profile(ac));
        }
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .amd_profile(ac)
            .copied()
    }

    fn saved_amd_initial(&self, ac: bool) -> Option<crate::types::AmdPowerProfile> {
        if self.amd_preview_full {
            return Some(mock_amd_profile(ac));
        }
        crate::util::read_lock(&self.state.lifecycle.config)
            .cpu_power
            .amd_initial(ac)
            .copied()
    }

    fn store_amd_profile(&mut self, ac: bool, profile: crate::types::AmdPowerProfile) {
        self.mutate_config(|cfg| *cfg.cpu_power.amd_profile_mut(ac) = Some(profile));
    }

    fn parse_amd_profile(&self) -> Result<crate::types::AmdPowerProfile, String> {
        let parse = |name: &str, raw: &str| {
            raw.parse::<f64>()
                .map_err(|_| format!("{name} is not a valid number"))
        };
        let profile = crate::types::AmdPowerProfile {
            stapm_watts: parse("STAPM", &self.amd_stapm_edit)?,
            fast_watts: parse("PPT Fast", &self.amd_fast_edit)?,
            slow_watts: parse("PPT Slow", &self.amd_slow_edit)?,
            tctl_c: parse("Tctl", &self.amd_tctl_edit)?,
        };
        if !profile.is_valid() {
            return Err("AMD limits out of range (5-150 W, Tctl 40-105 °C)".to_string());
        }
        Ok(profile)
    }

    fn apply_amd_fields_from_snapshot(&mut self, info: &crate::cpu_power::AmdPowerInfo) {
        self.amd_stapm_edit = format!("{:.1}", info.stapm_w);
        self.amd_fast_edit = format!("{:.1}", info.fast_w);
        self.amd_slow_edit = format!("{:.1}", info.slow_w);
        self.amd_tctl_edit = format!("{:.1}", info.tctl_c);
    }

    fn show_amd_fields(&mut self, profile: &crate::types::AmdPowerProfile) {
        self.amd_stapm_edit = format!("{:.1}", profile.stapm_watts);
        self.amd_fast_edit = format!("{:.1}", profile.fast_watts);
        self.amd_slow_edit = format!("{:.1}", profile.slow_watts);
        self.amd_tctl_edit = format!("{:.1}", profile.tctl_c);
    }

    pub(crate) fn load_amd_fields_for_source(&mut self) {
        if let Some(profile) = self
            .saved_amd_profile(self.amd_profile_is_ac)
            .or_else(|| self.saved_amd_initial(self.amd_profile_is_ac))
        {
            self.show_amd_fields(&profile);
        } else {
            let info = self.state.cpu_power.amd_snapshot();
            if info.available {
                self.apply_amd_fields_from_snapshot(&info);
            }
        }
    }

    fn amd_apply_task(
        &self,
        ac: bool,
        request: crate::cpu_power::LimitRequest,
        intent: u64,
    ) -> Task<Message> {
        Task::perform(
            async move {
                crate::util::spawn_blocking_with_timeout(
                    crate::util::PAWNIO_SYNC_TIMEOUT,
                    move || {
                        // The source can change while this task waits for the
                        // SMU lock; never write the old source's profile onto
                        // the new state.
                        if crate::cpu_power::read_ac_present().is_some_and(|current| current != ac)
                        {
                            return Err(PROFILE_SKIPPED_SOURCE_CHANGED.to_string());
                        }
                        crate::cpu_power::apply_amd_limits(request)
                    },
                )
                .await
                .unwrap_or_else(Err)
            },
            move |result| Message::AmdApplied(ac, intent, result),
        )
    }

    /// Captures each source's first PM-table values, follows the live source,
    /// and primes the edit boxes once. Runs every tick.
    pub(crate) fn prime_amd_fields(&mut self) {
        if !self.state.system.amd_cpu.load(Ordering::Acquire) {
            return;
        }
        if self.amd_preview {
            if !self.amd_fields_primed {
                let info = self.state.cpu_power.amd_snapshot();
                self.apply_amd_fields_from_snapshot(&info);
                self.amd_fields_primed = true;
                self.mark_dirty();
            }
            return;
        }
        let info = self.state.cpu_power.amd_snapshot();
        if !info.available {
            self.amd_fields_primed = false;
            return;
        }
        let mut source_changed = false;
        if let Some(ac) = self.current_power_source() {
            if self.saved_amd_initial(ac).is_none() {
                let profile = crate::types::AmdPowerProfile::from_info(&info);
                if profile.is_valid() {
                    self.mutate_config(|cfg| *cfg.cpu_power.amd_initial_mut(ac) = Some(profile));
                    self.save_config();
                }
            }
            if !self.amd_profile_user_selected && self.amd_profile_is_ac != ac {
                self.amd_profile_is_ac = ac;
                source_changed = true;
            }
        }
        if !self.amd_fields_primed || source_changed {
            self.load_amd_fields_for_source();
            self.amd_fields_primed = true;
            self.mark_dirty();
        }
    }

    /// Applies the saved AMD profile on startup, source changes, and every
    /// five minutes while the keep-alive toggle is on. Requires the explicit
    /// experimental-write opt-in.
    pub(crate) fn maybe_auto_apply_amd(&mut self, now_ms: u64) -> Option<Task<Message>> {
        if self.amd_preview
            || !self.state.system.amd_cpu.load(Ordering::Acquire)
            || !self.keep_limits_enabled()
            || !self.amd_write_enabled()
            || self.amd_apply_busy
            || self.amd_apply_failures >= 3
        {
            return None;
        }
        let info = self.state.cpu_power.amd_snapshot();
        if !info.available {
            return None;
        }
        let ac = self.current_power_source()?;
        const REASSERT_MS: u64 = 5 * 60 * 1000;
        if self.amd_active_source == Some(ac)
            && now_ms.saturating_sub(self.amd_last_reassert_ms) < REASSERT_MS
        {
            return None;
        }
        let profile = self
            .saved_amd_profile(ac)
            .or_else(|| self.saved_amd_initial(ac))?;
        self.amd_apply_busy = true;
        self.amd_last_reassert_ms = now_ms;
        self.cpu_power_error = None;
        let intent = self.sync_intent.fetch_add(1, Ordering::AcqRel) + 1;
        Some(self.amd_apply_task(ac, profile.to_request(), intent))
    }

    fn apply_edit_fields_from_snapshot(&mut self, info: &crate::cpu_power::CpuPowerInfo) {
        let (pl1, pl2, p1en, p2en, p1cl, p2cl, t1, _t2) = info.init_edit_fields();
        self.pl1_edit = pl1;
        self.pl2_edit = pl2;
        self.pl1_time_edit = t1;
        self.pl1_enabled = p1en;
        self.pl2_enabled = p2en;
        self.pl1_clamped = p1cl;
        self.pl2_clamped = p2cl;
    }
}

#[cfg(test)]
mod tests {
    use crate::cpu_power::CpuPowerInfo;
    use std::sync::atomic::Ordering;

    #[test]
    fn refresh_always_overwrites_edit_fields() {
        // Edit boxes follow the hardware on every explicit refresh; there is
        // no dirty flag to get stuck (background polls never touch them).
        let (mut app, _) = crate::App::new(false);
        app.pl1_edit = "stale".into();
        app.pl1_enabled = false;
        let info = CpuPowerInfo {
            available: true,
            pl1_msr: 30.0,
            pl1_msr_enabled: true,
            pl2_msr: 35.0,
            ..Default::default()
        };
        app.apply_edit_fields_from_snapshot(&info);
        assert_eq!(app.pl1_edit, "30.0");
        assert_eq!(app.pl2_edit, "35.0");
        assert!(app.pl1_enabled);
    }

    #[test]
    fn primed_latches_only_on_live_hardware() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        assert!(!app.pl_fields_primed);
        // Unavailable snapshot: boxes cleared, latch stays open so the tick
        // loop keeps firing catch-up refreshes.
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        assert!(!app.pl_fields_primed);
    }

    #[test]
    fn primed_reopens_when_hardware_drops() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        // Flap: a latched latch must reopen when hardware drops so the next
        // appearance fires another catch-up refresh (test harness has no
        // PawnIO, so the snapshot reads unavailable).
        app.pl_fields_primed = true;
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        assert!(!app.pl_fields_primed);
    }

    #[test]
    fn profile_switch_is_due_only_for_a_known_different_source() {
        use super::profile_switch_due;
        assert!(profile_switch_due(None, Some(true), false));
        assert!(profile_switch_due(Some(true), Some(false), false));
        assert!(!profile_switch_due(Some(true), Some(true), false));
        assert!(!profile_switch_due(None, None, false));
        assert!(!profile_switch_due(None, Some(true), true));
    }

    #[test]
    fn auto_retry_stops_after_three_failures_and_respects_backoff() {
        use super::auto_retry_allowed;
        assert!(auto_retry_allowed(0, 1000, 1000));
        assert!(!auto_retry_allowed(0, 999, 1000));
        assert!(auto_retry_allowed(2, 2000, 1000));
        assert!(!auto_retry_allowed(3, 2000, 1000));
    }

    #[test]
    fn mirror_match_has_a_small_tolerance() {
        use super::mmio_matches_msr;
        let mut info = CpuPowerInfo {
            pl1_msr: 30.0,
            pl1_mmio: 30.01,
            pl2_msr: 60.0,
            pl2_mmio: 59.99,
            ..Default::default()
        };
        assert!(mmio_matches_msr(&info));
        info.pl2_mmio = 60.2;
        assert!(!mmio_matches_msr(&info));
    }

    #[test]
    fn data_refresh_prefers_saved_profile_then_the_mmio_capture() {
        use crate::app::Message;
        use crate::types::PowerProfile;
        let (mut app, _) = crate::App::new(false);
        app.mutate_config(|cfg| {
            cfg.cpu_power.ac = None;
            cfg.cpu_power.ac_initial = Some(PowerProfile {
                pl1_watts: 28.0,
                pl2_watts: 55.0,
                ..Default::default()
            });
        });
        app.pl_profile_is_ac = true;
        app.pl_profile_user_selected = true;
        app.pl1_edit = "stale".into();
        let _ = app.handle_cpu_power_message(&Message::CpuPowerDataRefreshed);
        // No PawnIO in the test harness: the saved/captured default is the
        // only source for the boxes, and it must win over stale fields.
        assert_eq!(app.pl1_edit, "28.0");
        assert_eq!(app.pl2_edit, "55.0");
    }

    #[test]
    fn switching_source_loads_the_saved_profile_fields() {
        use crate::types::PowerProfile;
        let (mut app, _) = crate::App::new(false);
        app.store_profile(
            false,
            PowerProfile {
                pl1_watts: 22.0,
                pl1_enabled: false,
                pl1_clamped: true,
                pl1_time_s: 15.0,
                pl2_watts: 44.0,
                pl2_enabled: true,
                pl2_clamped: false,
            },
        );
        app.pl_profile_is_ac = false;
        app.load_pl_fields_for_source();
        assert_eq!(app.pl1_edit, "22.0");
        assert_eq!(app.pl2_edit, "44.0");
        assert_eq!(app.pl1_time_edit, "15.0");
        assert!(!app.pl1_enabled);
        assert!(app.pl1_clamped);
    }

    #[test]
    fn reset_that_cannot_start_disarms_the_manual_keep_flag() {
        let (mut app, _) = crate::App::new(false);
        app.pl_reset_disable_keep = true;
        let _ = app.handle_cpu_power_sync_reset();
        assert!(!app.pl_reset_disable_keep);
    }

    #[tokio::test]
    async fn begin_pawnio_task_uses_status_not_error() {
        let (mut app, _) = crate::App::new(false);
        assert!(app.begin_pawnio_task("Installing PawnIO..."));
        assert!(app.pawnio_busy);
        assert_eq!(app.pawnio_status.as_deref(), Some("Installing PawnIO..."));
        assert!(app.modules_download_error.is_none());
        assert!(!app.begin_pawnio_task("again"));
    }

    #[test]
    fn imported_config_resets_stale_power_state() {
        let (mut app, _) = crate::App::new(false);
        app.pl_active_source = Some(true);
        app.pl_profile_busy = true;
        app.pl_profile_retry_at_ms = 42;
        app.pl_profile_retry_failures = 3;
        app.pl_capture_in_flight = true;
        app.pl_capture_source = Some(true);
        app.pl_reset_disable_keep = true;
        app.pl_fields_primed = true;
        app.amd_apply_busy = true;
        app.amd_active_source = Some(false);
        app.amd_last_reassert_ms = 42;
        app.amd_reset_disable_keep = true;
        app.amd_apply_failures = 3;
        app.amd_fields_primed = true;
        let before_intent = app.sync_intent.load(Ordering::Acquire);

        let mut cfg = crate::types::Config::default();
        cfg.cpu_power.keep_limits = true;
        let task = app.apply_imported_config(&cfg);

        assert!(task.is_none(), "no sync thread is running in this test");
        assert!(app.sync_intent.load(Ordering::Acquire) > before_intent);
        assert_eq!(app.pl_active_source, None);
        assert!(!app.pl_profile_busy);
        assert_eq!(app.pl_profile_retry_at_ms, 0);
        assert_eq!(app.pl_profile_retry_failures, 0);
        assert!(!app.pl_capture_in_flight);
        assert_eq!(app.pl_capture_source, None);
        assert!(!app.pl_reset_disable_keep);
        assert!(!app.pl_fields_primed);
        assert!(!app.amd_apply_busy);
        assert_eq!(app.amd_active_source, None);
        assert_eq!(app.amd_last_reassert_ms, 0);
        assert!(!app.amd_reset_disable_keep);
        assert_eq!(app.amd_apply_failures, 0);
        assert!(!app.amd_fields_primed);
    }

    #[test]
    fn stale_amd_result_is_ignored() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        app.amd_apply_busy = true;
        let stale_intent = app.sync_intent.load(Ordering::Acquire).wrapping_add(1);
        let _ = app.handle_cpu_power_message(&Message::AmdApplied(
            true,
            stale_intent,
            Err("old result".to_string()),
        ));
        assert!(app.amd_apply_busy);
        assert!(app.cpu_power_error.is_none());
    }

    #[test]
    fn stale_reset_result_is_ignored() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        app.pl_custom_applied.store(true, Ordering::Release);
        app.state
            .lifecycle
            .pl_reset_pending
            .store(true, Ordering::Release);
        let stale_intent = app.sync_intent.load(Ordering::Acquire).wrapping_add(1);
        let _ = app.handle_cpu_power_message(&Message::CpuPowerResetDone(stale_intent, Ok(())));
        assert!(app.pl_custom_applied.load(Ordering::Acquire));
        assert!(app.state.lifecycle.pl_reset_pending.load(Ordering::Acquire));
    }

    #[test]
    fn amd_stop_invalidates_the_inflight_result() {
        use crate::app::Message;
        let (mut app, _) = crate::App::new(false);
        app.state.system.amd_cpu.store(true, Ordering::Release);
        app.amd_apply_busy = true;
        app.amd_active_source = Some(true);
        app.amd_reset_disable_keep = true;
        let old_intent = app.sync_intent.load(Ordering::Acquire);
        let _ = app.handle_cpu_power_message(&Message::CpuPowerSyncStop);
        assert!(!app.amd_apply_busy);
        assert_eq!(app.amd_active_source, None);
        assert!(!app.amd_reset_disable_keep);
        assert!(app.sync_intent.load(Ordering::Acquire) > old_intent);
        let _ = app.handle_cpu_power_message(&Message::AmdApplied(
            true,
            old_intent,
            Err("old result".to_string()),
        ));
        assert!(!app.amd_apply_busy);
    }
}

#[cfg(test)]
mod reset_tests {
    use super::sync_restart_after_reset;
    use crate::cpu_power::PowerLimitParams;

    fn params(pl1: f64) -> PowerLimitParams {
        PowerLimitParams {
            pl1_watts: pl1,
            pl1_enabled: true,
            pl1_clamped: false,
            pl1_time_s: 28.0,
            pl2_watts: 35.0,
            pl2_enabled: true,
            pl2_clamped: false,
            pl2_time_s: 28.0,
            max_time_s: None,
            power_unit: 0.125,
            time_unit: 0.0009765625,
        }
    }

    #[test]
    fn successful_reset_leaves_sync_stopped() {
        assert_eq!(
            sync_restart_after_reset(true, true, Some(params(28.0))),
            None
        );
    }

    #[test]
    fn failed_reset_restores_previous_sync() {
        assert_eq!(
            sync_restart_after_reset(true, false, Some(params(28.0))),
            Some(params(28.0))
        );
        assert_eq!(sync_restart_after_reset(true, false, None), None);
    }

    #[test]
    fn reset_does_not_start_sync_when_it_was_disabled() {
        assert_eq!(
            sync_restart_after_reset(false, false, Some(params(28.0))),
            None
        );
    }
}
