use std::fmt;
use std::str::FromStr;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TalkerId {
    // ── AIS ──────────────────────────────────────────────────────────────────
    /// Independent AIS base station
    AB,
    /// Dependent AIS base station
    AD,
    /// Autopilot, general
    AG,
    /// Mobile Class A or B AIS station
    AI,
    /// AIS aid to navigation station
    AN,
    /// Autopilot, magnetic
    AP,
    /// AIS receiving station
    AR,
    /// AIS transmitting station
    AT,
    /// AIS simplex repeater
    AX,
    // ── Communications ───────────────────────────────────────────────────────
    /// Communications, digital selective calling (DSC)
    CD,
    /// Communications, data receiver (beacon receiver)
    CR,
    /// Communications, satellite
    CS,
    /// Communications, radio-telephone (MF/HF)
    CT,
    /// Communications, radio-telephone (VHF)
    CV,
    /// Communications, scanning receiver
    CX,
    // ── Direction finding / legacy positioning ───────────────────────────────
    /// DECCA navigation (obsolete)
    DE,
    /// Direction finder
    DF,
    // ── Electronic systems ───────────────────────────────────────────────────
    /// Electronic chart display and information system (ECDIS)
    EC,
    /// Emergency position indicating radio beacon (EPIRB)
    EP,
    /// Engine room monitoring systems
    ER,
    // ── GNSS ─────────────────────────────────────────────────────────────────
    /// Galileo
    GA,
    /// BeiDou (CNSS)
    GB,
    /// BeiDou (alternate, legacy designation)
    BD,
    /// NavIC / IRNSS
    GI,
    /// GLONASS
    GL,
    /// Combined / multi-constellation GNSS
    GN,
    /// GPS
    GP,
    /// QZSS
    GQ,
    // ── Heading ──────────────────────────────────────────────────────────────
    /// Heading, magnetic compass
    HC,
    /// Heading, north-seeking gyro
    HE,
    /// Heading, fluxgate compass
    HF,
    /// Heading, non north-seeking gyro
    HN,
    // ── Integrated systems ───────────────────────────────────────────────────
    /// Integrated instrumentation
    II,
    /// Integrated navigation
    IN,
    // ── Legacy / obsolete positioning ────────────────────────────────────────
    /// Loran A (obsolete)
    LA,
    /// Loran C (obsolete)
    LC,
    /// Microwave positioning system (obsolete)
    MP,
    /// OMEGA navigation system (obsolete)
    OM,
    /// TRANSIT navigation system (obsolete)
    TR,
    // ── Other onboard systems ────────────────────────────────────────────────
    /// Navigation light controller
    NL,
    /// RADAR and/or ARPA
    RA,
    /// Physical shore AIS station
    SA,
    /// Sounder, depth
    SD,
    /// Electronic positioning system, other/general
    SN,
    /// Sounder, scanning
    SS,
    /// Turn rate indicator
    TI,
    // ── User-configured (U0–U9) ──────────────────────────────────────────────
    U0,
    U1,
    U2,
    U3,
    U4,
    U5,
    U6,
    U7,
    U8,
    U9,
    // ── Velocity sensors ─────────────────────────────────────────────────────
    /// Velocity sensor, Doppler
    VD,
    /// Velocity sensor, magnetic
    VM,
    /// Voyage data recorder
    VR,
    /// Velocity sensor, mechanical (water)
    VW,
    // ── Weather ──────────────────────────────────────────────────────────────
    /// Weather instruments
    WI,
    // ── Transducers (obsolete — superseded by XDR sentence) ──────────────────
    /// Transducer, temperature
    YC,
    /// Transducer, displacement (angular or linear)
    YD,
    /// Transducer, frequency
    YF,
    /// Transducer, level
    YL,
    /// Transducer, pressure
    YP,
    /// Transducer, flow rate
    YR,
    /// Transducer, status
    YS,
    /// Transducer, tachometer
    YT,
    /// Transducer, volume
    YV,
    /// Transducer, general
    YX,
    // ── Timekeepers ──────────────────────────────────────────────────────────
    /// Timekeeper, atomic clock
    ZA,
    /// Timekeeper, chronometer
    ZC,
    /// Timekeeper, quartz
    ZQ,
    // ── Catch-all ────────────────────────────────────────────────────────────
    /// Non-standard or future talker IDs.
    Custom(String),
}

/// All standard two-character talker IDs in this enum, in declaration order.
/// Intended for UIs that want to present every option (e.g. a filterable
/// dropdown). Does not include the `Custom(String)` variant.
pub const ALL: &[&str] = &[
    "AB", "AD", "AG", "AI", "AN", "AP", "AR", "AT", "AX", "CD", "CR", "CS", "CT", "CV", "CX", "DE",
    "DF", "EC", "EP", "ER", "GA", "GB", "BD", "GI", "GL", "GN", "GP", "GQ", "HC", "HE", "HF", "HN",
    "II", "IN", "LA", "LC", "MP", "OM", "TR", "NL", "RA", "SA", "SD", "SN", "SS", "TI", "U0", "U1",
    "U2", "U3", "U4", "U5", "U6", "U7", "U8", "U9", "VD", "VM", "VR", "VW", "WI", "YC", "YD", "YF",
    "YL", "YP", "YR", "YS", "YT", "YV", "YX", "ZA", "ZC", "ZQ",
];

impl TalkerId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::AB => "AB",
            Self::AD => "AD",
            Self::AG => "AG",
            Self::AI => "AI",
            Self::AN => "AN",
            Self::AP => "AP",
            Self::AR => "AR",
            Self::AT => "AT",
            Self::AX => "AX",
            Self::CD => "CD",
            Self::CR => "CR",
            Self::CS => "CS",
            Self::CT => "CT",
            Self::CV => "CV",
            Self::CX => "CX",
            Self::DE => "DE",
            Self::DF => "DF",
            Self::EC => "EC",
            Self::EP => "EP",
            Self::ER => "ER",
            Self::GA => "GA",
            Self::GB => "GB",
            Self::BD => "BD",
            Self::GI => "GI",
            Self::GL => "GL",
            Self::GN => "GN",
            Self::GP => "GP",
            Self::GQ => "GQ",
            Self::HC => "HC",
            Self::HE => "HE",
            Self::HF => "HF",
            Self::HN => "HN",
            Self::II => "II",
            Self::IN => "IN",
            Self::LA => "LA",
            Self::LC => "LC",
            Self::MP => "MP",
            Self::OM => "OM",
            Self::TR => "TR",
            Self::NL => "NL",
            Self::RA => "RA",
            Self::SA => "SA",
            Self::SD => "SD",
            Self::SN => "SN",
            Self::SS => "SS",
            Self::TI => "TI",
            Self::U0 => "U0",
            Self::U1 => "U1",
            Self::U2 => "U2",
            Self::U3 => "U3",
            Self::U4 => "U4",
            Self::U5 => "U5",
            Self::U6 => "U6",
            Self::U7 => "U7",
            Self::U8 => "U8",
            Self::U9 => "U9",
            Self::VD => "VD",
            Self::VM => "VM",
            Self::VR => "VR",
            Self::VW => "VW",
            Self::WI => "WI",
            Self::YC => "YC",
            Self::YD => "YD",
            Self::YF => "YF",
            Self::YL => "YL",
            Self::YP => "YP",
            Self::YR => "YR",
            Self::YS => "YS",
            Self::YT => "YT",
            Self::YV => "YV",
            Self::YX => "YX",
            Self::ZA => "ZA",
            Self::ZC => "ZC",
            Self::ZQ => "ZQ",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl fmt::Display for TalkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TalkerId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "AB" => Self::AB,
            "AD" => Self::AD,
            "AG" => Self::AG,
            "AI" => Self::AI,
            "AN" => Self::AN,
            "AP" => Self::AP,
            "AR" => Self::AR,
            "AT" => Self::AT,
            "AX" => Self::AX,
            "CD" => Self::CD,
            "CR" => Self::CR,
            "CS" => Self::CS,
            "CT" => Self::CT,
            "CV" => Self::CV,
            "CX" => Self::CX,
            "DE" => Self::DE,
            "DF" => Self::DF,
            "EC" => Self::EC,
            "EP" => Self::EP,
            "ER" => Self::ER,
            "GA" => Self::GA,
            "GB" => Self::GB,
            "BD" => Self::BD,
            "GI" => Self::GI,
            "GL" => Self::GL,
            "GN" => Self::GN,
            "GP" => Self::GP,
            "GQ" => Self::GQ,
            "HC" => Self::HC,
            "HE" => Self::HE,
            "HF" => Self::HF,
            "HN" => Self::HN,
            "II" => Self::II,
            "IN" => Self::IN,
            "LA" => Self::LA,
            "LC" => Self::LC,
            "MP" => Self::MP,
            "OM" => Self::OM,
            "TR" => Self::TR,
            "NL" => Self::NL,
            "RA" => Self::RA,
            "SA" => Self::SA,
            "SD" => Self::SD,
            "SN" => Self::SN,
            "SS" => Self::SS,
            "TI" => Self::TI,
            "U0" => Self::U0,
            "U1" => Self::U1,
            "U2" => Self::U2,
            "U3" => Self::U3,
            "U4" => Self::U4,
            "U5" => Self::U5,
            "U6" => Self::U6,
            "U7" => Self::U7,
            "U8" => Self::U8,
            "U9" => Self::U9,
            "VD" => Self::VD,
            "VM" => Self::VM,
            "VR" => Self::VR,
            "VW" => Self::VW,
            "WI" => Self::WI,
            "YC" => Self::YC,
            "YD" => Self::YD,
            "YF" => Self::YF,
            "YL" => Self::YL,
            "YP" => Self::YP,
            "YR" => Self::YR,
            "YS" => Self::YS,
            "YT" => Self::YT,
            "YV" => Self::YV,
            "YX" => Self::YX,
            "ZA" => Self::ZA,
            "ZC" => Self::ZC,
            "ZQ" => Self::ZQ,
            _ => Self::Custom(s.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_constant_matches_enum() {
        // Every entry in `ALL` must parse back to a named variant — not the
        // `Custom` catch-all — so the const stays in sync with the enum.
        for s in ALL {
            let parsed: TalkerId = s.parse().expect("ALL entry parses");
            assert!(
                !matches!(parsed, TalkerId::Custom(_)),
                "ALL contains {s:?} but it parsed as Custom — the enum is missing this variant",
            );
            assert_eq!(parsed.as_str(), *s, "round-trip mismatch for {s:?}");
        }
    }

    #[test]
    fn known_ids_round_trip() {
        let cases = [
            (TalkerId::AB, "AB"),
            (TalkerId::AD, "AD"),
            (TalkerId::AG, "AG"),
            (TalkerId::AI, "AI"),
            (TalkerId::AN, "AN"),
            (TalkerId::AP, "AP"),
            (TalkerId::AR, "AR"),
            (TalkerId::AT, "AT"),
            (TalkerId::AX, "AX"),
            (TalkerId::CD, "CD"),
            (TalkerId::CR, "CR"),
            (TalkerId::CS, "CS"),
            (TalkerId::CT, "CT"),
            (TalkerId::CV, "CV"),
            (TalkerId::CX, "CX"),
            (TalkerId::DE, "DE"),
            (TalkerId::DF, "DF"),
            (TalkerId::EC, "EC"),
            (TalkerId::EP, "EP"),
            (TalkerId::ER, "ER"),
            (TalkerId::GA, "GA"),
            (TalkerId::GB, "GB"),
            (TalkerId::BD, "BD"),
            (TalkerId::GI, "GI"),
            (TalkerId::GL, "GL"),
            (TalkerId::GN, "GN"),
            (TalkerId::GP, "GP"),
            (TalkerId::GQ, "GQ"),
            (TalkerId::HC, "HC"),
            (TalkerId::HE, "HE"),
            (TalkerId::HF, "HF"),
            (TalkerId::HN, "HN"),
            (TalkerId::II, "II"),
            (TalkerId::IN, "IN"),
            (TalkerId::LA, "LA"),
            (TalkerId::LC, "LC"),
            (TalkerId::MP, "MP"),
            (TalkerId::OM, "OM"),
            (TalkerId::TR, "TR"),
            (TalkerId::NL, "NL"),
            (TalkerId::RA, "RA"),
            (TalkerId::SA, "SA"),
            (TalkerId::SD, "SD"),
            (TalkerId::SN, "SN"),
            (TalkerId::SS, "SS"),
            (TalkerId::TI, "TI"),
            (TalkerId::U0, "U0"),
            (TalkerId::U1, "U1"),
            (TalkerId::U2, "U2"),
            (TalkerId::U3, "U3"),
            (TalkerId::U4, "U4"),
            (TalkerId::U5, "U5"),
            (TalkerId::U6, "U6"),
            (TalkerId::U7, "U7"),
            (TalkerId::U8, "U8"),
            (TalkerId::U9, "U9"),
            (TalkerId::VD, "VD"),
            (TalkerId::VM, "VM"),
            (TalkerId::VR, "VR"),
            (TalkerId::VW, "VW"),
            (TalkerId::WI, "WI"),
            (TalkerId::YC, "YC"),
            (TalkerId::YD, "YD"),
            (TalkerId::YF, "YF"),
            (TalkerId::YL, "YL"),
            (TalkerId::YP, "YP"),
            (TalkerId::YR, "YR"),
            (TalkerId::YS, "YS"),
            (TalkerId::YT, "YT"),
            (TalkerId::YV, "YV"),
            (TalkerId::YX, "YX"),
            (TalkerId::ZA, "ZA"),
            (TalkerId::ZC, "ZC"),
            (TalkerId::ZQ, "ZQ"),
        ];
        for (variant, s) in cases {
            assert_eq!(variant.as_str(), s);
            assert_eq!(s.parse::<TalkerId>().unwrap(), variant);
            assert_eq!(variant.to_string(), s);
        }
    }

    #[test]
    fn custom_id_round_trip() {
        let id: TalkerId = "XY".parse().unwrap();
        assert_eq!(id, TalkerId::Custom("XY".to_string()));
        assert_eq!(id.to_string(), "XY");
    }
}
