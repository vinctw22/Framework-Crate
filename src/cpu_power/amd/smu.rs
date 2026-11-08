//! RyzenSMU session: code name, SMU version, PM table, and the MP1/PSMU
//! mailbox used for writes.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use super::AmdPowerInfo;
use super::code_name::{AmdCodeName, AmdCpuFamily};
use super::commands::{self, Channel, Command, LimitRequest};
use crate::cpu_power::ffi::{PawnioHandle, exec_ioctl, open_handle};
use crate::cpu_power::modules::load_ryzen_smu_blob;

/// Number of qwords read from the PM table (enough for every known layout).
const PM_TABLE_WORDS: usize = 64;
const MAILBOX_TIMEOUT_MS: u64 = 200;

/// Serializes every SMU mailbox/PM-table session in this process.
static SMU_ACCESS_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// RAII ownership of the shared `Global\Access_PCI` mutex that PawnIO module
/// docs require around PCI/mailbox/PM-table access. Best-effort on reads;
/// writes fail closed when another tool holds it.
struct AccessPciGuard(HANDLE);

impl Drop for AccessPciGuard {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

fn acquire_access_pci() -> Result<AccessPciGuard, String> {
    let name: Vec<u16> = "Global\\Access_PCI".encode_utf16().chain(Some(0)).collect();
    // Initially unowned: acquire through WaitForSingleObject below.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err("cannot create Access_PCI mutex".to_string());
    }
    // WAIT_OBJECT_0 / WAIT_ABANDONED are success, WAIT_TIMEOUT is 0x102.
    match unsafe { WaitForSingleObject(handle, 1_000) } {
        0 | 0x80 => Ok(AccessPciGuard(handle)),
        _ => {
            unsafe {
                CloseHandle(handle);
            }
            Err("Access_PCI is held by another tool".to_string())
        }
    }
}

#[derive(Clone, Copy)]
struct Mailbox {
    cmd: u32,
    rsp: u32,
    args: u32,
}

fn mp1_addresses(family: AmdCpuFamily) -> Option<Mailbox> {
    let (cmd, rsp, args) = match family {
        AmdCpuFamily::Zen1Desktop => (0x03B1_0528, 0x03B1_0564, 0x03B1_0598),
        AmdCpuFamily::Raven | AmdCpuFamily::Renoir => (0x03B1_0528, 0x03B1_0564, 0x03B1_0998),
        AmdCpuFamily::Mobile => (0x03B1_0528, 0x03B1_0578, 0x03B1_0998),
        AmdCpuFamily::StrixPoint | AmdCpuFamily::StrixHalo => {
            (0x03B1_0928, 0x03B1_0978, 0x03B1_0998)
        }
        AmdCpuFamily::Matisse | AmdCpuFamily::Raphael => (0x03B1_0530, 0x03B1_057C, 0x03B1_09C4),
        _ => return None,
    };
    Some(Mailbox { cmd, rsp, args })
}

fn psmu_addresses(family: AmdCpuFamily) -> Option<Mailbox> {
    let (cmd, rsp, args) = match family {
        AmdCpuFamily::Zen1Desktop => (0x03B1_051C, 0x03B1_0568, 0x03B1_0590),
        AmdCpuFamily::Matisse | AmdCpuFamily::Raphael => (0x03B1_0524, 0x03B1_0570, 0x03B1_0A40),
        AmdCpuFamily::Raven
        | AmdCpuFamily::Renoir
        | AmdCpuFamily::Mobile
        | AmdCpuFamily::StrixPoint
        | AmdCpuFamily::StrixHalo => (0x03B1_0A20, 0x03B1_0A80, 0x03B1_0A88),
        AmdCpuFamily::ShimadaPeak => (0x03B1_0924, 0x03B1_0970, 0x03B1_0A40),
        _ => return None,
    };
    Some(Mailbox { cmd, rsp, args })
}

fn mailbox(family: AmdCpuFamily, channel: Channel) -> Option<Mailbox> {
    match channel {
        Channel::Mp1 => mp1_addresses(family),
        Channel::Psmu => psmu_addresses(family),
    }
}

fn read_reg(handle: &PawnioHandle, addr: u32) -> Result<u32, String> {
    let mut out = [0u64; 1];
    exec_ioctl(handle, "ioctl_read_smu_register", &[addr as u64], &mut out)
        .map_err(|_| format!("read SMU register 0x{addr:08X} failed"))?;
    Ok(out[0] as u32)
}

fn write_reg(handle: &PawnioHandle, addr: u32, value: u32) -> Result<(), String> {
    let mut none: [u64; 0] = [];
    exec_ioctl(
        handle,
        "ioctl_write_smu_register",
        &[addr as u64, value as u64],
        &mut none,
    )
    .map(|_| ())
    .map_err(|_| format!("write SMU register 0x{addr:08X} failed"))
}

fn status_message(status: u32) -> String {
    match status {
        0x01 => "SMU command failed".to_string(),
        0xFF => "SMU rejected the command (Failed)".to_string(),
        0xFE => "SMU rejected the command (UnknownCmd)".to_string(),
        0xFD => "SMU rejected the command (prerequisite missing)".to_string(),
        0xFC => "SMU mailbox busy (CmdRejectedBusy)".to_string(),
        other => format!("SMU rejected the command (status 0x{other:02X})"),
    }
}

/// Sends one command through the family's MP1 or PSMU mailbox, matching
/// G-Helper's register sequence. The module's `ioctl_send_smu_command` only
/// targets the PSMU mapping, so writes need explicit addresses.
fn send_mailbox(
    handle: &PawnioHandle,
    family: AmdCpuFamily,
    command: &Command,
    arg: u32,
) -> Result<(), String> {
    let mb = mailbox(family, command.channel)
        .ok_or_else(|| "no SMU mailbox for this CPU family".to_string())?;
    let deadline = Instant::now() + Duration::from_millis(MAILBOX_TIMEOUT_MS);

    loop {
        if read_reg(handle, mb.rsp)? != 0 {
            break;
        }
        if Instant::now() >= deadline {
            return Err("SMU mailbox stayed busy".to_string());
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    write_reg(handle, mb.rsp, 0)?;
    for i in 0..6 {
        write_reg(handle, mb.args + i * 4, if i == 0 { arg } else { 0 })?;
    }
    write_reg(handle, mb.cmd, command.id as u32)?;

    loop {
        let status = read_reg(handle, mb.rsp)?;
        if status != 0 {
            if status == 0x01 {
                return Ok(());
            }
            return Err(status_message(status));
        }
        if Instant::now() >= deadline {
            return Err("SMU command timed out".to_string());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn send_commands(
    handle: &PawnioHandle,
    family: AmdCpuFamily,
    commands: &[Command],
    arg: u32,
    label: &str,
) -> Result<(), String> {
    for command in commands {
        if let Err(e) = send_mailbox(handle, family, command, arg) {
            if command.required {
                return Err(format!("{label}: {e}"));
            }
            tracing::warn!("{label} optional SMU mirror failed: {e}");
        }
    }
    Ok(())
}

/// Float index of tctl_temp for a PM table version, matching RyzenAdj's
/// `get_tctl_temp()` switch (also used by G-Helper).
fn tctl_index(pm_table_version: u32) -> usize {
    match pm_table_version >> 16 {
        // Raven / Picasso / Dali and StrixHalo: offset 0x58.
        0x1E | 0x64 => 22,
        // Renoir/Lucienne, Cezanne, Rembrandt/Phoenix, StrixPoint/Krackan.
        0x37 | 0x3F | 0x40 | 0x45 | 0x4C | 0x5D | 0x65 => 16,
        // DragonRange/Raphael and FireRange/GraniteRidge.
        0x54 | 0x62 => 10,
        // Safe fallback for a future table: same slot as most APUs.
        _ => 16,
    }
}

fn has_apu_slow(pm_table_version: u32) -> bool {
    let hi = pm_table_version >> 16;
    hi != 0x1E && hi != 0x54
}

fn floats_from_words(words: &[u64]) -> Vec<f32> {
    let mut floats = Vec::with_capacity(words.len() * 2);
    for word in words {
        floats.push(f32::from_bits(*word as u32));
        floats.push(f32::from_bits((*word >> 32) as u32));
    }
    floats
}

fn read_code_name(handle: &PawnioHandle) -> Result<AmdCodeName, String> {
    let mut out = [0u64; 1];
    exec_ioctl(handle, "ioctl_get_code_name", &[], &mut out)
        .map_err(|_| "RyzenSMU get_code_name failed".to_string())?;
    Ok(AmdCodeName::from_raw(out[0] as u32))
}

fn read_smu_version(handle: &PawnioHandle) -> Result<u32, String> {
    let mut out = [0u64; 1];
    exec_ioctl(handle, "ioctl_get_smu_version", &[], &mut out)
        .map_err(|_| "RyzenSMU get_smu_version failed".to_string())?;
    Ok(out[0] as u32)
}

fn read_info_from_handle(
    handle: &PawnioHandle,
    code_name: AmdCodeName,
    smu_version: u32,
) -> Result<AmdPowerInfo, String> {
    let mut resolved = [0u64; 2];
    exec_ioctl(handle, "ioctl_resolve_pm_table", &[], &mut resolved)
        .map_err(|_| "RyzenSMU resolve_pm_table failed".to_string())?;
    let pm_table_version = resolved[0] as u32;

    // After resume the first update may return busy while the SMU mailbox is
    // still processing wake-up state; retry briefly like G-Helper.
    let mut none: [u64; 0] = [];
    if exec_ioctl(handle, "ioctl_update_pm_table", &[], &mut none).is_err() {
        std::thread::sleep(Duration::from_millis(100));
    }
    exec_ioctl(handle, "ioctl_update_pm_table", &[], &mut none)
        .map_err(|_| "RyzenSMU update_pm_table failed".to_string())?;
    std::thread::sleep(Duration::from_millis(200));

    let mut words = [0u64; PM_TABLE_WORDS];
    exec_ioctl(handle, "ioctl_read_pm_table", &[], &mut words)
        .map_err(|_| "RyzenSMU read_pm_table failed".to_string())?;
    let floats = floats_from_words(&words);
    if floats.first().copied().unwrap_or(0.0) == 0.0 {
        return Err("RyzenSMU PM table is zeroed (unsupported or busy)".to_string());
    }

    let tctl = *floats
        .get(tctl_index(pm_table_version))
        .ok_or("RyzenSMU PM table is shorter than the tctl index")?;
    let apu_slow = has_apu_slow(pm_table_version)
        .then(|| floats.get(6).copied())
        .flatten();

    Ok(AmdPowerInfo {
        available: true,
        error: None,
        code_name,
        smu_version,
        pm_table_version,
        stapm_w: floats[0],
        fast_w: floats.get(2).copied().unwrap_or(0.0),
        slow_w: floats.get(4).copied().unwrap_or(0.0),
        tctl_c: tctl,
        apu_slow_w: apu_slow,
        pm_table: floats,
    })
}

fn open_module_handle() -> Result<PawnioHandle, String> {
    let blob = load_ryzen_smu_blob()
        .map_err(|e| format!("RyzenSMU module unavailable: {e} (download PawnIO Modules)"))?;
    open_handle(&blob).map_err(|e| format!("load RyzenSMU failed: {e}"))
}

/// Reads the current AMD power limits. Blocking PawnIO calls; call from a
/// blocking worker, not the UI thread.
pub fn read_amd_power() -> Result<AmdPowerInfo, String> {
    let _guard = SMU_ACCESS_LOCK.lock();
    let _pci = match acquire_access_pci() {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::warn!("AMD read continuing without Access_PCI: {}", e);
            None
        }
    };
    let handle = open_module_handle()?;
    let code_name = read_code_name(&handle)?;
    let smu_version = read_smu_version(&handle)?;
    read_info_from_handle(&handle, code_name, smu_version)
}

/// Applies STAPM/PPT Fast/PPT Slow/Tctl and returns the PM-table readback.
pub fn apply_amd_limits(request: LimitRequest) -> Result<AmdPowerInfo, String> {
    commands::validate_request(&request)?;
    let _guard = SMU_ACCESS_LOCK.lock();
    let _pci = acquire_access_pci().map_err(|e| format!("PCI access busy: {e}"))?;
    let handle = open_module_handle()?;
    let code_name = read_code_name(&handle)?;
    let family = code_name.family();
    let commands = commands::commands_for(family)
        .ok_or_else(|| format!("AMD writes are not supported for {}", code_name.name()))?;
    let smu_version = read_smu_version(&handle)?;

    // Read first and only write fields that actually differ; a no-op Apply
    // must not disturb the SMU mailbox.
    let before = read_info_from_handle(&handle, code_name, smu_version)?;
    let needs = |actual: f32, requested: f64| (f64::from(actual) - requested).abs() > 0.5;
    let mut changed = false;
    if needs(before.stapm_w, request.stapm_w) {
        let mw = (request.stapm_w * 1000.0).round() as u32;
        send_commands(&handle, family, commands.stapm, mw, "STAPM")?;
        changed = true;
    }
    if needs(before.fast_w, request.fast_w) {
        let mw = (request.fast_w * 1000.0).round() as u32;
        send_commands(&handle, family, commands.fast, mw, "PPT Fast")?;
        changed = true;
    }
    if needs(before.slow_w, request.slow_w) {
        let mw = (request.slow_w * 1000.0).round() as u32;
        send_commands(&handle, family, commands.slow, mw, "PPT Slow")?;
        changed = true;
    }
    if (f64::from(before.tctl_c) - request.tctl_c).abs() > 0.5 {
        send_commands(
            &handle,
            family,
            commands.tctl,
            request.tctl_c.round() as u32,
            "Tctl",
        )?;
        changed = true;
    }
    if !changed {
        tracing::debug!("AMD limits already match; no SMU writes sent");
        return Ok(before);
    }

    let mut info = read_info_from_handle(&handle, code_name, smu_version)?;
    if let Some(mismatch) = readback_mismatch(&info, &request) {
        // A single stale PM-table update right after a mailbox write is
        // possible; re-read once before treating the apply as failed.
        tracing::warn!("AMD readback mismatch, retrying once: {mismatch}");
        std::thread::sleep(Duration::from_millis(250));
        info = read_info_from_handle(&handle, code_name, smu_version)?;
        if let Some(mismatch) = readback_mismatch(&info, &request) {
            tracing::warn!("AMD readback still differs after retry: {mismatch}");
            return Err(mismatch);
        }
    }
    Ok(info)
}

/// Describes every control whose readback differs from the request by more
/// than the encoder tolerance. `None` means the SMU accepted the values.
fn readback_mismatch(info: &AmdPowerInfo, request: &LimitRequest) -> Option<String> {
    let close = |actual: f32, requested: f64| (f64::from(actual) - requested).abs() <= 1.0;
    let mut fields = Vec::new();
    if !close(info.stapm_w, request.stapm_w) {
        fields.push(format!(
            "STAPM {:.1}W (asked {:.1}W)",
            info.stapm_w, request.stapm_w
        ));
    }
    if !close(info.fast_w, request.fast_w) {
        fields.push(format!(
            "PPT Fast {:.1}W (asked {:.1}W)",
            info.fast_w, request.fast_w
        ));
    }
    if !close(info.slow_w, request.slow_w) {
        fields.push(format!(
            "PPT Slow {:.1}W (asked {:.1}W)",
            info.slow_w, request.slow_w
        ));
    }
    if (f64::from(info.tctl_c) - request.tctl_c).abs() > 1.0 {
        fields.push(format!(
            "Tctl {:.1}C (asked {:.1}C)",
            info.tctl_c, request.tctl_c
        ));
    }
    (!fields.is_empty()).then(|| format!("AMD readback mismatch: {}", fields.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_index_matches_known_table_versions() {
        assert_eq!(tctl_index(0x001E_0000), 22);
        assert_eq!(tctl_index(0x0064_0000), 22);
        assert_eq!(tctl_index(0x0037_0000), 16);
        assert_eq!(tctl_index(0x004C_0000), 16);
        assert_eq!(tctl_index(0x005D_0000), 16);
        assert_eq!(tctl_index(0x0054_0000), 10);
        assert_eq!(tctl_index(0x0062_0000), 10);
        assert_eq!(tctl_index(0x0099_0000), 16);
    }

    #[test]
    fn readback_mismatch_names_each_differing_control() {
        let info = AmdPowerInfo {
            stapm_w: 25.0,
            fast_w: 45.0,
            slow_w: 35.0,
            tctl_c: 90.0,
            ..Default::default()
        };
        let request = LimitRequest {
            stapm_w: 25.0,
            fast_w: 45.0,
            slow_w: 35.0,
            tctl_c: 90.0,
        };
        assert!(readback_mismatch(&info, &request).is_none());

        let mut changed = info;
        changed.fast_w = 40.0;
        changed.tctl_c = 85.0;
        let message = readback_mismatch(&changed, &request).expect("mismatch");
        assert!(message.contains("PPT Fast"), "{message}");
        assert!(message.contains("Tctl"), "{message}");
        assert!(!message.contains("PPT Slow"), "{message}");
    }

    #[test]
    fn apu_slow_is_absent_on_raven_and_raphael() {
        assert!(!has_apu_slow(0x001E_0000));
        assert!(!has_apu_slow(0x0054_0000));
        assert!(has_apu_slow(0x004C_0000));
        assert!(has_apu_slow(0x0064_0000));
    }

    #[test]
    fn words_decode_little_endian_float_pairs() {
        let words = [0x41C8_0000u64, 0x0000_0000u64]; // 25.0 in the low dword
        let floats = floats_from_words(&words);
        assert_eq!(floats[0], 25.0);
        assert_eq!(floats[1], 0.0);
    }

    #[test]
    fn mailbox_addresses_match_gh_helper() {
        let mp1 = mp1_addresses(AmdCpuFamily::Mobile).unwrap();
        assert_eq!(
            (mp1.cmd, mp1.rsp, mp1.args),
            (0x03B1_0528, 0x03B1_0578, 0x03B1_0998)
        );
        let strix_mp1 = mp1_addresses(AmdCpuFamily::StrixPoint).unwrap();
        assert_eq!(strix_mp1.cmd, 0x03B1_0928);
        let psmu = psmu_addresses(AmdCpuFamily::Renoir).unwrap();
        assert_eq!(
            (psmu.cmd, psmu.rsp, psmu.args),
            (0x03B1_0A20, 0x03B1_0A80, 0x03B1_0A88)
        );
        assert!(mailbox(AmdCpuFamily::Unknown, Channel::Mp1).is_none());
    }
}
