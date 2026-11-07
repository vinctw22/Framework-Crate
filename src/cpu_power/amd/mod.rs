//! AMD RyzenSMU support (read-only first; writes land in a later phase).

use std::sync::Arc;

pub mod code_name;
mod smu;

pub use code_name::{AmdCodeName, AmdCpuFamily};
pub use smu::read_amd_power;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_info_is_unavailable_and_keeps_the_message() {
        let info = AmdPowerInfo::from_error("missing module".to_string());
        assert!(!info.available);
        assert_eq!(info.error.as_deref(), Some("missing module"));
        assert_eq!(info.code_name, AmdCodeName::Undefined);
        assert_eq!(info.family(), AmdCpuFamily::Unknown);
    }
}
