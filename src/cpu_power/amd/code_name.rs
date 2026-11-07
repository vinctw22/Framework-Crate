//! Ryzen code-name detection and SMU family grouping.

/// Code-name values returned by RyzenSMU's `ioctl_get_code_name` (matches
/// RyzenAdj and G-Helper).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u32)]
pub enum AmdCodeName {
    #[default]
    Undefined = 0xFFFF_FFFF,
    Colfax = 0,
    SummitRidge = 8,
    PinnacleRidge = 9,
    Threadripper = 4,
    CastlePeak = 5,
    RavenRidge = 6,
    RavenRidge2 = 7,
    Picasso = 2,
    Dali = 15,
    Matisse = 3,
    Vermeer = 11,
    Renoir = 1,
    Lucienne = 22,
    Cezanne = 13,
    Vangogh = 12,
    Milan = 14,
    Rome = 20,
    Naples = 18,
    Rembrandt = 10,
    Mendocino = 25,
    Raphael = 16,
    DragonRange = 28,
    Chagall = 21,
    StormPeak = 27,
    Phoenix = 23,
    Phoenix2 = 24,
    HawkPoint = 30,
    GraniteRidge = 17,
    FireFlight = 19,
    Genoa = 26,
    Bergamo = 37,
    Turin = 35,
    TurinD = 36,
    StrixPoint = 31,
    StrixHalo = 32,
    KrackanPoint = 33,
    KrackanPoint2 = 34,
    ShimadaPeak = 38,
    Mero = 29,
}

/// SMU command-routing families used by the write path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmdCpuFamily {
    Unknown,
    Zen1Desktop,
    Raven,
    Renoir,
    Matisse,
    Mobile,
    Raphael,
    StrixPoint,
    StrixHalo,
    ShimadaPeak,
}

impl AmdCodeName {
    pub fn from_raw(value: u32) -> Self {
        match value {
            0 => Self::Colfax,
            8 => Self::SummitRidge,
            9 => Self::PinnacleRidge,
            4 => Self::Threadripper,
            5 => Self::CastlePeak,
            6 => Self::RavenRidge,
            7 => Self::RavenRidge2,
            2 => Self::Picasso,
            15 => Self::Dali,
            3 => Self::Matisse,
            11 => Self::Vermeer,
            1 => Self::Renoir,
            22 => Self::Lucienne,
            13 => Self::Cezanne,
            12 => Self::Vangogh,
            14 => Self::Milan,
            20 => Self::Rome,
            18 => Self::Naples,
            10 => Self::Rembrandt,
            25 => Self::Mendocino,
            16 => Self::Raphael,
            28 => Self::DragonRange,
            21 => Self::Chagall,
            27 => Self::StormPeak,
            23 => Self::Phoenix,
            24 => Self::Phoenix2,
            30 => Self::HawkPoint,
            17 => Self::GraniteRidge,
            19 => Self::FireFlight,
            26 => Self::Genoa,
            37 => Self::Bergamo,
            35 => Self::Turin,
            36 => Self::TurinD,
            31 => Self::StrixPoint,
            32 => Self::StrixHalo,
            33 => Self::KrackanPoint,
            34 => Self::KrackanPoint2,
            38 => Self::ShimadaPeak,
            29 => Self::Mero,
            _ => Self::Undefined,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Undefined => "Unknown",
            Self::Colfax => "Colfax",
            Self::SummitRidge => "Summit Ridge",
            Self::PinnacleRidge => "Pinnacle Ridge",
            Self::Threadripper => "Threadripper",
            Self::CastlePeak => "Castle Peak",
            Self::RavenRidge => "Raven Ridge",
            Self::RavenRidge2 => "Raven Ridge 2",
            Self::Picasso => "Picasso",
            Self::Dali => "Dali",
            Self::Matisse => "Matisse",
            Self::Vermeer => "Vermeer",
            Self::Renoir => "Renoir",
            Self::Lucienne => "Lucienne",
            Self::Cezanne => "Cezanne",
            Self::Vangogh => "Van Gogh",
            Self::Milan => "Milan",
            Self::Rome => "Rome",
            Self::Naples => "Naples",
            Self::Rembrandt => "Rembrandt",
            Self::Mendocino => "Mendocino",
            Self::Raphael => "Raphael",
            Self::DragonRange => "Dragon Range",
            Self::Chagall => "Chagall",
            Self::StormPeak => "Storm Peak",
            Self::Phoenix => "Phoenix",
            Self::Phoenix2 => "Phoenix 2",
            Self::HawkPoint => "Hawk Point",
            Self::GraniteRidge => "Granite Ridge",
            Self::FireFlight => "Fire Flight",
            Self::Genoa => "Genoa",
            Self::Bergamo => "Bergamo",
            Self::Turin => "Turin",
            Self::TurinD => "Turin D",
            Self::StrixPoint => "Strix Point",
            Self::StrixHalo => "Strix Halo",
            Self::KrackanPoint => "Krackan Point",
            Self::KrackanPoint2 => "Krackan Point 2",
            Self::ShimadaPeak => "Shimada Peak",
            Self::Mero => "Mero",
        }
    }

    pub fn family(self) -> AmdCpuFamily {
        match self {
            Self::Colfax
            | Self::SummitRidge
            | Self::PinnacleRidge
            | Self::Threadripper
            | Self::CastlePeak => AmdCpuFamily::Zen1Desktop,
            Self::RavenRidge | Self::RavenRidge2 | Self::Picasso | Self::Dali => {
                AmdCpuFamily::Raven
            }
            Self::Renoir | Self::Lucienne | Self::Cezanne => AmdCpuFamily::Renoir,
            Self::Matisse | Self::Vermeer => AmdCpuFamily::Matisse,
            Self::Vangogh
            | Self::Rembrandt
            | Self::Mendocino
            | Self::Phoenix
            | Self::Phoenix2
            | Self::HawkPoint
            | Self::FireFlight => AmdCpuFamily::Mobile,
            Self::Raphael | Self::GraniteRidge | Self::DragonRange => AmdCpuFamily::Raphael,
            Self::StrixPoint | Self::KrackanPoint | Self::KrackanPoint2 => AmdCpuFamily::StrixPoint,
            Self::StrixHalo => AmdCpuFamily::StrixHalo,
            Self::ShimadaPeak => AmdCpuFamily::ShimadaPeak,
            _ => AmdCpuFamily::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_round_trip_for_known_names() {
        assert_eq!(AmdCodeName::from_raw(23), AmdCodeName::Phoenix);
        assert_eq!(AmdCodeName::from_raw(30), AmdCodeName::HawkPoint);
        assert_eq!(AmdCodeName::from_raw(31), AmdCodeName::StrixPoint);
        assert_eq!(AmdCodeName::from_raw(0xFFFF_FFFF), AmdCodeName::Undefined);
        assert_eq!(AmdCodeName::from_raw(9999), AmdCodeName::Undefined);
    }

    #[test]
    fn families_follow_the_gh_helper_grouping() {
        assert_eq!(AmdCodeName::Phoenix.family(), AmdCpuFamily::Mobile);
        assert_eq!(AmdCodeName::HawkPoint.family(), AmdCpuFamily::Mobile);
        assert_eq!(AmdCodeName::StrixPoint.family(), AmdCpuFamily::StrixPoint);
        assert_eq!(AmdCodeName::StrixHalo.family(), AmdCpuFamily::StrixHalo);
        assert_eq!(AmdCodeName::Raphael.family(), AmdCpuFamily::Raphael);
        assert_eq!(AmdCodeName::RavenRidge.family(), AmdCpuFamily::Raven);
        assert_eq!(AmdCodeName::Renoir.family(), AmdCpuFamily::Renoir);
        assert_eq!(AmdCodeName::Undefined.family(), AmdCpuFamily::Unknown);
    }
}
