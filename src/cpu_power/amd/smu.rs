//! Read-only RyzenSMU session: code name, SMU version, and PM table.

use super::AmdPowerInfo;
use super::code_name::AmdCodeName;
use crate::cpu_power::ffi::{exec_ioctl, open_handle};
use crate::cpu_power::modules::load_ryzen_smu_blob;

/// Number of qwords read from the PM table (enough for every known layout).
const PM_TABLE_WORDS: usize = 64;

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

/// Reads the current AMD power limits. Blocking PawnIO calls; call from a
/// blocking worker, not the UI thread.
pub fn read_amd_power() -> Result<AmdPowerInfo, String> {
    let blob = load_ryzen_smu_blob()
        .map_err(|e| format!("RyzenSMU module unavailable: {e} (download PawnIO Modules)"))?;
    let handle = open_handle(&blob).map_err(|e| format!("load RyzenSMU failed: {e}"))?;

    let mut code = [0u64; 1];
    exec_ioctl(&handle, "ioctl_get_code_name", &[], &mut code)
        .map_err(|_| "RyzenSMU get_code_name failed".to_string())?;
    let code_name = AmdCodeName::from_raw(code[0] as u32);

    let mut version = [0u64; 1];
    exec_ioctl(&handle, "ioctl_get_smu_version", &[], &mut version)
        .map_err(|_| "RyzenSMU get_smu_version failed".to_string())?;
    let smu_version = version[0] as u32;

    let mut resolved = [0u64; 2];
    exec_ioctl(&handle, "ioctl_resolve_pm_table", &[], &mut resolved)
        .map_err(|_| "RyzenSMU resolve_pm_table failed".to_string())?;
    let pm_table_version = resolved[0] as u32;

    // After resume the first update may return busy while the SMU mailbox is
    // still processing wake-up state; retry briefly like G-Helper.
    let first = exec_ioctl(&handle, "ioctl_update_pm_table", &[], &mut []);
    if first.is_err() {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    exec_ioctl(&handle, "ioctl_update_pm_table", &[], &mut [])
        .map_err(|_| "RyzenSMU update_pm_table failed".to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut words = [0u64; PM_TABLE_WORDS];
    exec_ioctl(&handle, "ioctl_read_pm_table", &[], &mut words)
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
    })
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
}
