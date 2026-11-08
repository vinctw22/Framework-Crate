//! SMU command table for the four AMD power controls.
//!
//! IDs mirror G-Helper / RyzenAdj. Watt commands take milliwatts; Tctl takes
//! degrees Celsius.

use super::code_name::AmdCpuFamily;

pub(super) const MIN_WATTS: f64 = 5.0;
pub(super) const MAX_WATTS: f64 = 150.0;
pub(super) const MIN_TCTL_C: f64 = 40.0;
pub(super) const MAX_TCTL_C: f64 = 105.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Channel {
    Mp1,
    Psmu,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Command {
    pub channel: Channel,
    pub id: u8,
    /// MP1 is the primary route; PSMU mirrors are best-effort (G-Helper
    /// returns the MP1 status for Renoir).
    pub required: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FamilyCommands {
    pub stapm: &'static [Command],
    pub fast: &'static [Command],
    pub slow: &'static [Command],
    pub tctl: &'static [Command],
}

const fn cmd(channel: Channel, id: u8, required: bool) -> Command {
    Command {
        channel,
        id,
        required,
    }
}

const RAVEN_STAPM: &[Command] = &[cmd(Channel::Mp1, 0x1A, true)];
const RAVEN_FAST: &[Command] = &[cmd(Channel::Mp1, 0x1B, true)];
const RAVEN_SLOW: &[Command] = &[cmd(Channel::Mp1, 0x1C, true)];
const RAVEN_TCTL: &[Command] = &[cmd(Channel::Mp1, 0x1F, true)];

const RENOIR_STAPM: &[Command] = &[
    cmd(Channel::Mp1, 0x14, true),
    cmd(Channel::Psmu, 0x31, false),
];
const RENOIR_FAST: &[Command] = &[
    cmd(Channel::Mp1, 0x15, true),
    cmd(Channel::Psmu, 0x32, false),
];
const RENOIR_SLOW: &[Command] = &[
    cmd(Channel::Mp1, 0x16, true),
    cmd(Channel::Psmu, 0x33, false),
    cmd(Channel::Psmu, 0x34, false),
];
const RENOIR_TCTL: &[Command] = &[cmd(Channel::Mp1, 0x19, true)];

const MOBILE_STAPM: &[Command] = &[cmd(Channel::Mp1, 0x14, true)];
const MOBILE_FAST: &[Command] = &[cmd(Channel::Mp1, 0x15, true)];
const MOBILE_SLOW: &[Command] = &[cmd(Channel::Mp1, 0x16, true)];
const MOBILE_TCTL: &[Command] = &[cmd(Channel::Mp1, 0x19, true)];

const RAPHAEL_STAPM: &[Command] = &[cmd(Channel::Mp1, 0x4F, true)];
const RAPHAEL_FAST: &[Command] = &[cmd(Channel::Mp1, 0x3E, true)];
const RAPHAEL_SLOW: &[Command] = &[cmd(Channel::Mp1, 0x5F, true)];
const RAPHAEL_TCTL: &[Command] = &[cmd(Channel::Mp1, 0x3F, true)];

/// Returns the full four-control command set for a supported family.
pub(super) fn commands_for(family: AmdCpuFamily) -> Option<FamilyCommands> {
    match family {
        AmdCpuFamily::Raven => Some(FamilyCommands {
            stapm: RAVEN_STAPM,
            fast: RAVEN_FAST,
            slow: RAVEN_SLOW,
            tctl: RAVEN_TCTL,
        }),
        AmdCpuFamily::Renoir => Some(FamilyCommands {
            stapm: RENOIR_STAPM,
            fast: RENOIR_FAST,
            slow: RENOIR_SLOW,
            tctl: RENOIR_TCTL,
        }),
        AmdCpuFamily::Mobile | AmdCpuFamily::StrixPoint | AmdCpuFamily::StrixHalo => {
            Some(FamilyCommands {
                stapm: MOBILE_STAPM,
                fast: MOBILE_FAST,
                slow: MOBILE_SLOW,
                tctl: MOBILE_TCTL,
            })
        }
        AmdCpuFamily::Raphael => Some(FamilyCommands {
            stapm: RAPHAEL_STAPM,
            fast: RAPHAEL_FAST,
            slow: RAPHAEL_SLOW,
            tctl: RAPHAEL_TCTL,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LimitRequest {
    pub stapm_w: f64,
    pub fast_w: f64,
    pub slow_w: f64,
    pub tctl_c: f64,
}

pub(super) fn validate_request(req: &LimitRequest) -> Result<(), String> {
    for (name, value) in [
        ("STAPM", req.stapm_w),
        ("PPT Fast", req.fast_w),
        ("PPT Slow", req.slow_w),
    ] {
        if !value.is_finite() {
            return Err(format!("{name} must be a finite number"));
        }
        if !(MIN_WATTS..=MAX_WATTS).contains(&value) {
            return Err(format!(
                "{name} must be between {MIN_WATTS:.0} and {MAX_WATTS:.0} W"
            ));
        }
    }
    if !req.tctl_c.is_finite() {
        return Err("Tctl must be a finite number".to_string());
    }
    if !(MIN_TCTL_C..=MAX_TCTL_C).contains(&req.tctl_c) {
        return Err(format!(
            "Tctl must be between {MIN_TCTL_C:.0} and {MAX_TCTL_C:.0} °C"
        ));
    }
    if req.slow_w < req.stapm_w {
        return Err("PPT Slow must be at least STAPM".to_string());
    }
    if req.fast_w < req.slow_w {
        return Err("PPT Fast must be at least PPT Slow".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_families_have_all_four_commands() {
        for family in [
            AmdCpuFamily::Raven,
            AmdCpuFamily::Renoir,
            AmdCpuFamily::Mobile,
            AmdCpuFamily::StrixPoint,
            AmdCpuFamily::StrixHalo,
            AmdCpuFamily::Raphael,
        ] {
            let commands = commands_for(family).expect("supported family");
            assert!(!commands.stapm.is_empty());
            assert!(!commands.fast.is_empty());
            assert!(!commands.slow.is_empty());
            assert!(!commands.tctl.is_empty());
        }
        assert!(commands_for(AmdCpuFamily::Unknown).is_none());
        assert!(commands_for(AmdCpuFamily::Matisse).is_none());
    }

    #[test]
    fn command_ids_match_the_gh_helper_table() {
        let mobile = commands_for(AmdCpuFamily::Mobile).unwrap();
        assert_eq!(
            (mobile.stapm[0].channel, mobile.stapm[0].id),
            (Channel::Mp1, 0x14)
        );
        assert_eq!(
            (mobile.fast[0].channel, mobile.fast[0].id),
            (Channel::Mp1, 0x15)
        );
        assert_eq!(
            (mobile.slow[0].channel, mobile.slow[0].id),
            (Channel::Mp1, 0x16)
        );
        assert_eq!(
            (mobile.tctl[0].channel, mobile.tctl[0].id),
            (Channel::Mp1, 0x19)
        );

        let renoir = commands_for(AmdCpuFamily::Renoir).unwrap();
        assert_eq!(renoir.stapm.len(), 2);
        assert_eq!(renoir.slow.len(), 3);
        assert!(renoir.stapm[1].channel == Channel::Psmu);
        assert!(!renoir.stapm[1].required);

        let raphael = commands_for(AmdCpuFamily::Raphael).unwrap();
        assert_eq!((raphael.stapm[0].id, raphael.fast[0].id), (0x4F, 0x3E));
        assert_eq!((raphael.slow[0].id, raphael.tctl[0].id), (0x5F, 0x3F));
    }

    #[test]
    fn validation_covers_ranges_and_non_finite_values() {
        let ok = LimitRequest {
            stapm_w: 25.0,
            fast_w: 45.0,
            slow_w: 35.0,
            tctl_c: 90.0,
        };
        assert!(validate_request(&ok).is_ok());
        for bad in [4.9, 150.1, f64::NAN, f64::INFINITY] {
            let mut req = ok;
            req.stapm_w = bad;
            assert!(validate_request(&req).is_err(), "stapm={bad}");
        }
        let mut req = ok;
        req.tctl_c = 39.9;
        assert!(validate_request(&req).is_err());
        req.tctl_c = 105.1;
        assert!(validate_request(&req).is_err());
        let mut req = ok;
        req.slow_w = 20.0;
        assert!(validate_request(&req).is_err());
        let mut req = ok;
        req.fast_w = 30.0;
        assert!(validate_request(&req).is_err());
    }
}
