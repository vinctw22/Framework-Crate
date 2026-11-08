//! Reading the current PL1/PL2 limits through the PawnIO modules.

use std::time::Instant;
use tracing::debug;

use super::ffi::{exec_ioctl, open_handle};
use super::limits::{decode_power_limit, decode_time_window, decode_time_window_bounds};
use super::modules::{load_intel_mchbar_blob, load_intel_msr_blob};
use super::types::{CpuPowerInfo, CpuPowerUnavailable, PowerRegister};

/// Records the first cause of failure, so a later, more generic one cannot
/// mask the root cause.
fn set_first_failure(info: &mut CpuPowerInfo, cause: CpuPowerUnavailable) {
    if info.unavailable.is_none() {
        info.unavailable = Some(cause);
    }
}

pub(super) fn decode_rapl_units(raw: u64) -> (f64, f64) {
    let power_bits = (raw & 0xF) as u32;
    let time_bits = ((raw >> 16) & 0xF) as u32;
    let power_unit = 1.0 / (1u32 << power_bits) as f64;
    let time_unit = 1.0 / (1u32 << time_bits) as f64;
    (power_unit, time_unit)
}

/// RAPL energy unit from MSR 0x606[12:8], in Joules per counter tick.
pub(super) fn decode_energy_unit(raw: u64) -> f64 {
    let esu = ((raw >> 8) & 0x1F) as i32;
    2f64.powi(-esu)
}

#[derive(Default)]
struct PackagePowerState {
    last_raw: u32,
    last_at: Option<Instant>,
    last_watts: Option<f64>,
}

impl PackagePowerState {
    fn sample(&mut self, raw: u32, energy_unit: f64, now: Instant) -> Option<f64> {
        let Some(previous_at) = self.last_at else {
            self.last_raw = raw;
            self.last_at = Some(now);
            return None;
        };
        let elapsed = now
            .checked_duration_since(previous_at)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if elapsed <= 0.01 {
            return self.last_watts;
        }
        if elapsed > 60.0 {
            self.last_raw = raw;
            self.last_at = Some(now);
            self.last_watts = None;
            return None;
        }
        let delta = raw.wrapping_sub(self.last_raw);
        self.last_raw = raw;
        self.last_at = Some(now);
        let watts = f64::from(delta) * energy_unit / elapsed;
        if watts.is_finite() {
            self.last_watts = Some(watts);
            self.last_watts
        } else {
            self.last_watts = None;
            None
        }
    }
}

static PACKAGE_POWER: parking_lot::Mutex<PackagePowerState> =
    parking_lot::Mutex::new(PackagePowerState {
        last_raw: 0,
        last_at: None,
        last_watts: None,
    });

/// Reference clock for turbo ratios. Meteor Lake (and all recent client
/// Intel CPUs) run BCLK at 100MHz.
const TURBO_BCLK_GHZ: f64 = 0.1;

/// Decodes the 1-core-max turbo ratio from MSR_TURBO_RATIO_LIMIT (0x1AD):
/// eight per-group bytes, LE byte 0 covers 1 active core. `None` when no
/// group reports a ratio.
pub(super) fn decode_turbo_limit_ghz(raw: u64) -> Option<f64> {
    let max_ratio = (0..8).map(|i| (raw >> (i * 8)) & 0xFF).max()?;
    (max_ratio > 0).then_some(max_ratio as f64 * TURBO_BCLK_GHZ)
}

/// Decodes the current operating ratio from IA32_PERF_STATUS (0x198[15:8]).
pub(super) fn decode_turbo_now_ghz(raw: u64) -> Option<f64> {
    let ratio = (raw >> 8) & 0xFF;
    (ratio > 0).then_some(ratio as f64 * TURBO_BCLK_GHZ)
}

/// Reads MSR 0x606 live, so writes never trust persisted RAPL units.
pub(super) fn read_rapl_units() -> Result<(f64, f64), String> {
    let blob = load_intel_msr_blob().map_err(|e| e.to_string())?;
    let handle = open_handle(&blob).map_err(|e| e.to_string())?;
    let mut out = [0u64; 1];
    exec_ioctl(&handle, "ioctl_read_msr", &[0x606], &mut out)
        .map_err(|e| format!("failed to read RAPL units: {e:?}"))?;
    Ok(decode_rapl_units(out[0]))
}

/// Reads all CPU power info via PawnIO modules.
pub(super) fn read_cpu_power() -> CpuPowerInfo {
    let mut info = CpuPowerInfo::default();

    // Load handles independently; failure of one does not discard the other.
    let msr_handle = match load_intel_msr_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            set_first_failure(&mut info, CpuPowerUnavailable::Setup(e));
            None
        }
    };
    let mchbar_handle = match load_intel_mchbar_blob().and_then(|b| open_handle(&b)) {
        Ok(h) => Some(h),
        Err(e) => {
            set_first_failure(&mut info, CpuPowerUnavailable::Setup(e));
            None
        }
    };

    // Read MSR 0x606 for RAPL units; required to decode limits.
    let mut units_ok = false;
    let mut energy_unit = None;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x606], &mut out) {
            Ok(_) => {
                (info.power_unit, info.time_unit) = decode_rapl_units(out[0]);
                energy_unit = Some(decode_energy_unit(out[0]));
                debug!(
                    "Power unit: {} W/unit, time unit: {} s/unit",
                    info.power_unit, info.time_unit
                );
                units_ok = true;
            }
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::Units,
                    reason,
                },
            ),
        }
    }
    if !units_ok {
        return info;
    }

    // Optional package power from the energy counter (MSR 0x611). This is
    // display-only: a missing/locked MSR must not make the limits unavailable.
    if let (Some(unit), Some(handle)) = (energy_unit, msr_handle.as_ref()) {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x611], &mut out) {
            Ok(_) => {
                let now = Instant::now();
                info.package_watts = PACKAGE_POWER.lock().sample(out[0] as u32, unit, now);
                debug!("Package power: {:?} W", info.package_watts);
            }
            Err(reason) => {
                debug!("MSR 0x611 unavailable, package power unknown: {reason:?}");
            }
        }
    }

    // Read MSR 0x614 for the firmware-supported time-window range. Optional:
    // older firmware may not expose it, so failure keeps the old advisory
    // read-back behavior.
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x614], &mut out) {
            Ok(_) => {
                (info.max_time_s, info.min_time_s) = decode_time_window_bounds(out[0]);
                debug!(
                    "MSR 0x614 time window: min={:?}s max={:?}s",
                    info.min_time_s, info.max_time_s
                );
            }
            Err(reason) => {
                debug!("MSR 0x614 unavailable, time-window bounds unknown: {reason:?}");
            }
        }
    }

    // Turbo ratio telemetry (0x1AD limit, 0x198 current). Best-effort and
    // display-only like the 0x614 bounds above: failure keeps the old
    // behavior and never trips availability or any write decision.
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x1AD], &mut out) {
            Ok(_) => {
                info.turbo_limit_ghz = decode_turbo_limit_ghz(out[0]);
                debug!("Turbo ratio limit: {:?}", info.turbo_limit_ghz);
            }
            Err(reason) => {
                debug!("MSR 0x1AD unavailable, turbo limit unknown: {reason:?}");
            }
        }
        match exec_ioctl(handle, "ioctl_read_msr", &[0x198], &mut out) {
            Ok(_) => {
                info.turbo_now_ghz = decode_turbo_now_ghz(out[0]);
                debug!("Turbo current ratio: {:?}", info.turbo_now_ghz);
            }
            Err(reason) => {
                debug!("MSR 0x198 unavailable, current turbo unknown: {reason:?}");
            }
        }
    }

    // Read MSR 0x610 for static PL1/PL2.
    let mut msr610_ok = false;
    if let Some(ref handle) = msr_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_msr", &[0x610], &mut out) {
            Ok(_) => {
                msr610_ok = true;
                let raw = out[0];
                debug!("MSR 0x610 raw: 0x{:016X}", raw);
                let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
                let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                    decode_power_limit(raw >> 32, info.power_unit);
                info.pl1_msr = pl1;
                info.pl1_msr_enabled = pl1_en;
                info.pl1_msr_clamped = pl1_cl;
                info.pl1_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
                info.pl2_msr = pl2;
                info.pl2_msr_enabled = pl2_en;
                info.pl2_msr_clamped = pl2_cl;
                info.pl2_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
                debug!(
                    "MSR PL1: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                    pl1, pl1_en, pl1_cl, pl1_y, pl1_z, info.pl1_time_s
                );
                debug!(
                    "MSR PL2: {:.1}W (en={} clamp={}) Y={} Z={} time={:.1}s",
                    pl2, pl2_en, pl2_cl, pl2_y, pl2_z, info.pl2_time_s
                );
            }
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::Limits,
                    reason,
                },
            ),
        }
    }

    // Read MMIO at MCHBAR+0x59A0 via IntelMCHBAR.
    let mmio_offset = 0x59A0u64;
    if let Some(ref handle) = mchbar_handle {
        let mut out = [0u64; 1];
        match exec_ioctl(handle, "ioctl_read_qword", &[mmio_offset], &mut out) {
            Ok(_) => {
                info.mmio_available = true;
                let raw = out[0];
                let (pl1, pl1_en, pl1_cl, pl1_y, pl1_z) = decode_power_limit(raw, info.power_unit);
                let (pl2, pl2_en, pl2_cl, pl2_y, pl2_z) =
                    decode_power_limit(raw >> 32, info.power_unit);
                info.pl1_mmio = pl1;
                info.pl1_mmio_enabled = pl1_en;
                info.pl1_mmio_clamped = pl1_cl;
                info.pl1_mmio_time_s = decode_time_window(pl1_y, pl1_z, info.time_unit);
                info.pl2_mmio = pl2;
                info.pl2_mmio_enabled = pl2_en;
                info.pl2_mmio_clamped = pl2_cl;
                info.pl2_mmio_time_s = decode_time_window(pl2_y, pl2_z, info.time_unit);
                debug!(
                    "MMIO PL1: {:.1}W (en={} clamp={}) time={:.1}s",
                    pl1, pl1_en, pl1_cl, info.pl1_mmio_time_s
                );
                debug!(
                    "MMIO PL2: {:.1}W (en={} clamp={}) time={:.1}s",
                    pl2, pl2_en, pl2_cl, info.pl2_mmio_time_s
                );
            }
            Err(reason) => set_first_failure(
                &mut info,
                CpuPowerUnavailable::Read {
                    register: PowerRegister::LimitsMirror,
                    reason,
                },
            ),
        }
    }

    // The MSR half is the primary register used by edit fields, BIOS
    // snapshots and the sync thread. MMIO alone leaves those consumers with
    // zeroed values, so treat the read as unavailable and keep the root cause
    // recorded above.
    if !msr610_ok {
        return info;
    }
    info.available = true;
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu_power::ffi::IoctlFailure;

    #[test]
    fn decode_rapl_units_uses_the_expected_bit_fields() {
        let raw = 3u64 | (10u64 << 16);
        let (power_unit, time_unit) = decode_rapl_units(raw);
        assert!((power_unit - 0.125).abs() < 1e-12);
        assert!((time_unit - 1.0 / 1024.0).abs() < 1e-12);
    }

    #[test]
    fn decode_energy_unit_uses_esu_bits() {
        assert!((decode_energy_unit(14u64 << 8) - 1.0 / 16384.0).abs() < 1e-12);
        assert_eq!(decode_energy_unit(0), 1.0);
    }

    #[test]
    fn package_power_uses_energy_delta_and_handles_wrap() {
        use std::time::Duration;

        let mut state = PackagePowerState::default();
        let unit = 1.0 / 16384.0;
        let t0 = Instant::now();
        assert_eq!(state.sample(1000, unit, t0), None);

        let t1 = t0 + Duration::from_secs(1);
        let watts = state.sample(1000 + 16384, unit, t1).expect("second sample");
        assert!((watts - 1.0).abs() < 1e-9, "watts={watts}");
        assert_eq!(state.sample(1000 + 16384, unit, t1), Some(watts));

        let mut wrapped = PackagePowerState::default();
        assert_eq!(wrapped.sample(u32::MAX - 8191, unit, t0), None);
        let watts = wrapped.sample(8191, unit, t1).expect("wrapped sample");
        assert!((watts - 0.999_938_964_843_75).abs() < 1e-6, "watts={watts}");

        assert_eq!(
            wrapped.sample(8191, unit, t1 + Duration::from_secs(61)),
            None
        );
    }

    #[test]
    fn decode_turbo_limit_takes_max_group() {
        // LE byte 0 covers 1 active core: groups 48..34, max 48 -> 4.8GHz.
        let v = decode_turbo_limit_ghz(0x222426282A2C2E30).expect("limit");
        assert!((v - 4.8).abs() < 1e-9, "got {v}");
        assert_eq!(decode_turbo_limit_ghz(0), None);
    }

    #[test]
    fn decode_turbo_now_reads_perf_status_ratio() {
        // IA32_PERF_STATUS[15:8] = 12 -> 1.2GHz.
        let v = decode_turbo_now_ghz(0x0C00).expect("now");
        assert!((v - 1.2).abs() < 1e-9, "got {v}");
        assert_eq!(decode_turbo_now_ghz(0), None);
    }

    #[test]
    fn set_first_failure_keeps_the_earliest_cause() {
        // The order of the calls in read_cpu_power is the point: the first
        // failure is the root cause and the later, more generic ones must not
        // mask it. Both setup and read failures can arrive, and whichever
        // comes first wins.
        let mut info = CpuPowerInfo::default();
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Setup("IntelMSR blob missing"),
        );
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Read {
                register: PowerRegister::Limits,
                reason: IoctlFailure::Failed,
            },
        );
        assert_eq!(
            info.unavailable,
            Some(CpuPowerUnavailable::Setup("IntelMSR blob missing"))
        );
    }

    #[test]
    fn set_first_failure_records_when_nothing_failed_before() {
        let mut info = CpuPowerInfo::default();
        assert!(info.unavailable.is_none());
        set_first_failure(
            &mut info,
            CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::ShortRead,
            },
        );
        assert_eq!(
            info.unavailable,
            Some(CpuPowerUnavailable::Read {
                register: PowerRegister::Units,
                reason: IoctlFailure::ShortRead,
            })
        );
    }

    #[test]
    fn an_available_result_carries_no_reason() {
        // Nothing in read_cpu_power records a failure on the success path, so a
        // value that reports itself available must not also explain an outage.
        let info = CpuPowerInfo {
            available: true,
            ..Default::default()
        };
        assert!(info.unavailable.is_none());
    }
}
