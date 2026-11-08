//! AMD RyzenSMU support (read-only first; writes land in a later phase).

use std::sync::Arc;

pub mod code_name;
mod commands;
mod smu;

pub use code_name::{AmdCodeName, AmdCpuFamily};
pub use commands::LimitRequest;
pub use smu::{apply_amd_limits, read_amd_power};

/// Current AMD power limits read from the SMU PM table.
#[derive(Debug, Clone, Default)]
pub struct AmdPowerInfo {
    pub available: bool,
    pub error: Option<Arc<str>>,
    pub code_name: AmdCodeName,
    pub smu_version: u32,
    pub pm_table_version: u32,
    pub stapm_w: f32,
    pub fast_w: f32,
    pub slow_w: f32,
    pub tctl_c: f32,
    pub apu_slow_w: Option<f32>,
    /// Raw PM-table floats for diagnostics (Tctl offset verification).
    pub pm_table: Vec<f32>,
}

impl AmdPowerInfo {
    pub fn from_error(error: String) -> Self {
        Self {
            available: false,
            error: Some(Arc::from(error)),
            ..Default::default()
        }
    }

    pub fn code_name_name(&self) -> &'static str {
        self.code_name.name()
    }

    pub fn family(&self) -> AmdCpuFamily {
        self.code_name.family()
    }

    /// Copy/paste-friendly dump for AMD testers. Includes the raw PM table so
    /// the Tctl index and command behavior can be verified off-device.
    pub fn diagnostics_text(&self) -> String {
        let mut out = String::new();
        out.push_str("Framework Crate AMD diagnostics\n");
        out.push_str(&format!(
            "code_name={} ({:?})\nfamily={:?}\n",
            self.code_name_name(),
            self.code_name,
            self.family()
        ));
        out.push_str(&format!(
            "smu_version=0x{:08X}\npm_table_version=0x{:06X}\n",
            self.smu_version, self.pm_table_version
        ));
        out.push_str(&format!(
            "stapm={:.3}W fast={:.3}W slow={:.3}W tctl={:.3}C apu_slow={:?}\n",
            self.stapm_w, self.fast_w, self.slow_w, self.tctl_c, self.apu_slow_w
        ));
        out.push_str("pm_table_floats:\n");
        for (idx, value) in self.pm_table.iter().enumerate() {
            if idx % 8 == 0 {
                out.push_str(&format!("[{idx:03}] "));
            }
            out.push_str(&format!("{value:.6} "));
            if idx % 8 == 7 {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_include_code_name_versions_and_pm_table() {
        let info = AmdPowerInfo {
            available: true,
            code_name: AmdCodeName::Phoenix,
            smu_version: 0x0040_0001,
            pm_table_version: 0x004C_0008,
            stapm_w: 25.0,
            fast_w: 45.0,
            slow_w: 35.0,
            tctl_c: 90.0,
            pm_table: vec![25.0, 0.0, 45.0, 0.0],
            ..Default::default()
        };
        let text = info.diagnostics_text();
        assert!(text.contains("code_name=Phoenix"));
        assert!(text.contains("family=Mobile"));
        assert!(text.contains("pm_table_version=0x4C0008"));
        assert!(text.contains("[000] 25.000000"));
    }

    #[test]
    fn error_info_is_unavailable_and_keeps_the_message() {
        let info = AmdPowerInfo::from_error("missing module".to_string());
        assert!(!info.available);
        assert_eq!(info.error.as_deref(), Some("missing module"));
        assert_eq!(info.code_name, AmdCodeName::Undefined);
        assert_eq!(info.family(), AmdCpuFamily::Unknown);
    }
}
