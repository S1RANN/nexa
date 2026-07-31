use std::fmt;

/// A stable, machine-readable public Nexa error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(&'static str);

impl ErrorCode {
    pub const NX1001: Self = Self::new("NX1001");
    pub const NX1002: Self = Self::new("NX1002");
    pub const NX2001: Self = Self::new("NX2001");
    pub const NX2002: Self = Self::new("NX2002");
    pub const NX2101: Self = Self::new("NX2101");
    pub const NX2201: Self = Self::new("NX2201");
    pub const NX2202: Self = Self::new("NX2202");
    pub const NX2210: Self = Self::new("NX2210");
    pub const NX2220: Self = Self::new("NX2220");
    pub const NX2221: Self = Self::new("NX2221");
    pub const NX2301: Self = Self::new("NX2301");
    pub const NX2302: Self = Self::new("NX2302");
    pub const NX2401: Self = Self::new("NX2401");
    pub const NX2501: Self = Self::new("NX2501");
    pub const NX2601: Self = Self::new("NX2601");
    pub const NX2602: Self = Self::new("NX2602");
    pub const NX2603: Self = Self::new("NX2603");
    pub const NX2604: Self = Self::new("NX2604");
    pub const NX2701: Self = Self::new("NX2701");
    pub const NX2702: Self = Self::new("NX2702");
    pub const NX2703: Self = Self::new("NX2703");
    pub const NX2704: Self = Self::new("NX2704");
    pub const NX2705: Self = Self::new("NX2705");
    pub const NX2706: Self = Self::new("NX2706");
    pub const NX2710: Self = Self::new("NX2710");
    pub const NX2711: Self = Self::new("NX2711");
    pub const NX2720: Self = Self::new("NX2720");
    pub const NX2730: Self = Self::new("NX2730");
    pub const NX2740: Self = Self::new("NX2740");
    pub const NX3001: Self = Self::new("NX3001");
    pub const NX3002: Self = Self::new("NX3002");
    pub const NX3003: Self = Self::new("NX3003");
    pub const NX3004: Self = Self::new("NX3004");
    pub const NX4001: Self = Self::new("NX4001");
    pub const NX4002: Self = Self::new("NX4002");
    pub const NX4003: Self = Self::new("NX4003");
    pub const NX5001: Self = Self::new("NX5001");
    pub const NX5002: Self = Self::new("NX5002");
    pub const NX5003: Self = Self::new("NX5003");
    pub const NX5004: Self = Self::new("NX5004");
    pub const NX6001: Self = Self::new("NX6001");
    pub const NX6002: Self = Self::new("NX6002");
    pub const NX6003: Self = Self::new("NX6003");
    pub const NX6005: Self = Self::new("NX6005");
    pub const NX7001: Self = Self::new("NX7001");
    pub const NX7002: Self = Self::new("NX7002");
    pub const NX7003: Self = Self::new("NX7003");
    pub const NX7004: Self = Self::new("NX7004");
    pub const NX7010: Self = Self::new("NX7010");
    pub const NX7011: Self = Self::new("NX7011");
    pub const NX7101: Self = Self::new("NX7101");
    pub const NX7102: Self = Self::new("NX7102");
    pub const NX7103: Self = Self::new("NX7103");
    pub const NX7201: Self = Self::new("NX7201");
    pub const NX7202: Self = Self::new("NX7202");
    pub const NX7302: Self = Self::new("NX7302");
    pub const NX7303: Self = Self::new("NX7303");

    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    #[must_use]
    pub fn definition(self) -> Option<&'static ErrorCodeDefinition> {
        ERROR_CODE_TABLE
            .iter()
            .find(|definition| definition.code == self)
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

/// One immutable entry in Nexa's public diagnostic-code registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCodeDefinition {
    pub code: ErrorCode,
    pub summary: &'static str,
}

impl ErrorCodeDefinition {
    const fn new(code: ErrorCode, summary: &'static str) -> Self {
        Self { code, summary }
    }
}

/// The stable diagnostic-code registry, ordered by code.
pub static ERROR_CODE_TABLE: &[ErrorCodeDefinition] = &[
    ErrorCodeDefinition::new(ErrorCode::NX1001, "Unexpected character"),
    ErrorCodeDefinition::new(ErrorCode::NX1002, "Unexpected token"),
    ErrorCodeDefinition::new(ErrorCode::NX2001, "Unknown name"),
    ErrorCodeDefinition::new(ErrorCode::NX2002, "Unknown type"),
    ErrorCodeDefinition::new(ErrorCode::NX2101, "Type mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX2201, "Non-exhaustive match"),
    ErrorCodeDefinition::new(ErrorCode::NX2202, "Duplicate match variant"),
    ErrorCodeDefinition::new(ErrorCode::NX2210, "Cannot infer constructor type"),
    ErrorCodeDefinition::new(ErrorCode::NX2220, "? requires Result"),
    ErrorCodeDefinition::new(ErrorCode::NX2221, "? error mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX2301, "Await outside Task"),
    ErrorCodeDefinition::new(ErrorCode::NX2302, "Missing await"),
    ErrorCodeDefinition::new(ErrorCode::NX2401, "Invalid numeric conversion"),
    ErrorCodeDefinition::new(ErrorCode::NX2501, "Invalid field access"),
    ErrorCodeDefinition::new(ErrorCode::NX2601, "Migration intrinsic outside Migration"),
    ErrorCodeDefinition::new(ErrorCode::NX2602, "Missing finish_migration"),
    ErrorCodeDefinition::new(ErrorCode::NX2603, "Missing forwarding"),
    ErrorCodeDefinition::new(ErrorCode::NX2604, "Duplicate forwarding"),
    ErrorCodeDefinition::new(ErrorCode::NX2701, "Module path mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX2702, "Module cycle"),
    ErrorCodeDefinition::new(ErrorCode::NX2703, "Unknown import"),
    ErrorCodeDefinition::new(ErrorCode::NX2704, "Duplicate/ambiguous namespace"),
    ErrorCodeDefinition::new(ErrorCode::NX2705, "Private access"),
    ErrorCodeDefinition::new(ErrorCode::NX2706, "Invalid public API exposure"),
    ErrorCodeDefinition::new(ErrorCode::NX2710, "Invalid @stable"),
    ErrorCodeDefinition::new(ErrorCode::NX2711, "Duplicate/colliding stable identity"),
    ErrorCodeDefinition::new(ErrorCode::NX2720, "Invalid const expression"),
    ErrorCodeDefinition::new(ErrorCode::NX2730, "Invalid package test"),
    ErrorCodeDefinition::new(ErrorCode::NX2740, "Invalid lifecycle/export location"),
    ErrorCodeDefinition::new(ErrorCode::NX3001, "Invalid bytecode section"),
    ErrorCodeDefinition::new(ErrorCode::NX3002, "Invalid register range"),
    ErrorCodeDefinition::new(ErrorCode::NX3003, "Invalid root map"),
    ErrorCodeDefinition::new(ErrorCode::NX3004, "Invalid SourceMap"),
    ErrorCodeDefinition::new(ErrorCode::NX4001, "Host interface mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX4002, "Host capability unavailable"),
    ErrorCodeDefinition::new(ErrorCode::NX4003, "Host argument mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX5001, "Host result mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX5002, "Host abandoned"),
    ErrorCodeDefinition::new(ErrorCode::NX5003, "Unknown host error code"),
    ErrorCodeDefinition::new(ErrorCode::NX5004, "Runtime resource capacity"),
    ErrorCodeDefinition::new(ErrorCode::NX6001, "Migration limit"),
    ErrorCodeDefinition::new(ErrorCode::NX6002, "Migration graph failure"),
    ErrorCodeDefinition::new(ErrorCode::NX6003, "Activation failure"),
    ErrorCodeDefinition::new(ErrorCode::NX6005, "Invalid ReloadMetadata"),
    ErrorCodeDefinition::new(ErrorCode::NX7001, "Package source failure"),
    ErrorCodeDefinition::new(ErrorCode::NX7002, "Invalid package manifest"),
    ErrorCodeDefinition::new(ErrorCode::NX7003, "Package policy rejection"),
    ErrorCodeDefinition::new(ErrorCode::NX7004, "Entitlement unavailable"),
    ErrorCodeDefinition::new(ErrorCode::NX7010, "Missing required export"),
    ErrorCodeDefinition::new(ErrorCode::NX7011, "Export signature mismatch"),
    ErrorCodeDefinition::new(ErrorCode::NX7101, "Handler yielded under MustComplete"),
    ErrorCodeDefinition::new(ErrorCode::NX7102, "Handler waited under MustComplete"),
    ErrorCodeDefinition::new(ErrorCode::NX7103, "Handler trapped"),
    ErrorCodeDefinition::new(ErrorCode::NX7201, "Reload rolled back before commit"),
    ErrorCodeDefinition::new(ErrorCode::NX7202, "Activation faulted after commit"),
    ErrorCodeDefinition::new(ErrorCode::NX7302, "Persistence failed"),
    ErrorCodeDefinition::new(ErrorCode::NX7303, "Engine shutdown incomplete"),
];

#[cfg(test)]
mod tests {
    use super::{ERROR_CODE_TABLE, ErrorCode};

    #[test]
    fn registry_is_strictly_sorted_and_unique() {
        assert!(
            ERROR_CODE_TABLE
                .windows(2)
                .all(|pair| pair[0].code < pair[1].code)
        );
        assert_eq!(
            ErrorCode::NX2101.definition().map(|entry| entry.summary),
            Some("Type mismatch")
        );
    }

    #[test]
    fn m4_semantic_codes_are_stable_and_registered() {
        const FROZEN_M4_CODES: &[(&str, &str)] = &[
            ("NX2701", "Module path mismatch"),
            ("NX2702", "Module cycle"),
            ("NX2703", "Unknown import"),
            ("NX2704", "Duplicate/ambiguous namespace"),
            ("NX2705", "Private access"),
            ("NX2706", "Invalid public API exposure"),
            ("NX2710", "Invalid @stable"),
            ("NX2711", "Duplicate/colliding stable identity"),
            ("NX2720", "Invalid const expression"),
            ("NX2730", "Invalid package test"),
            ("NX2740", "Invalid lifecycle/export location"),
        ];
        let registered = ERROR_CODE_TABLE
            .iter()
            .filter(|definition| definition.code.as_str().starts_with("NX27"))
            .map(|definition| (definition.code.as_str(), definition.summary))
            .collect::<Vec<_>>();
        assert_eq!(registered, FROZEN_M4_CODES);
        assert_eq!(
            [
                ErrorCode::NX2701,
                ErrorCode::NX2702,
                ErrorCode::NX2703,
                ErrorCode::NX2704,
                ErrorCode::NX2705,
                ErrorCode::NX2706,
                ErrorCode::NX2710,
                ErrorCode::NX2711,
                ErrorCode::NX2720,
                ErrorCode::NX2730,
                ErrorCode::NX2740,
            ]
            .map(ErrorCode::as_str),
            [
                "NX2701", "NX2702", "NX2703", "NX2704", "NX2705", "NX2706", "NX2710", "NX2711",
                "NX2720", "NX2730", "NX2740",
            ]
        );
    }
}
