//! Public build profiles for the canonical Nexa source pipeline.
//!
//! A profile is semantic input. It controls whether top-level statements are accepted and which
//! generated entrypoint contract the compiler must enforce, so it is also bound into the
//! canonical build fingerprint by `nexa-analysis`.

use std::fmt;

/// Selects the source shape and entrypoint rules applied by the canonical frontend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BuildProfile {
    /// A manifest-backed application or library package.
    #[default]
    Package,
    /// A manifest-backed executable package with an explicit `main`.
    StandalonePackage,
    /// One standalone source file. Top-level statements lower to a synthetic `main`.
    StandaloneScript,
    /// One cell in the synthetic `nexa.repl` package.
    ReplCell,
}

impl BuildProfile {
    /// Stable reader-facing spelling used by diagnostics and inspection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::StandalonePackage => "standalone-package",
            Self::StandaloneScript => "standalone-script",
            Self::ReplCell => "repl-cell",
        }
    }

    /// Whether this profile consumes a fully resolved manifest/package snapshot.
    #[must_use]
    pub const fn is_manifest_backed(self) -> bool {
        matches!(self, Self::Package | Self::StandalonePackage)
    }

    /// Whether the source may contain executable statements at module scope.
    #[must_use]
    pub const fn allows_top_level_statements(self) -> bool {
        matches!(self, Self::StandaloneScript | Self::ReplCell)
    }

    /// Whether successful compilation must identify an executable entrypoint.
    #[must_use]
    pub const fn requires_entrypoint(self) -> bool {
        !matches!(self, Self::Package)
    }

    /// Canonical analysis profile bound into compilation options and build fingerprints.
    #[must_use]
    pub const fn analysis_profile(self) -> nexa_analysis::CompilationProfile {
        match self {
            Self::Package => nexa_analysis::CompilationProfile::Package,
            Self::StandalonePackage => nexa_analysis::CompilationProfile::Standalone,
            Self::StandaloneScript => nexa_analysis::CompilationProfile::Script,
            Self::ReplCell => nexa_analysis::CompilationProfile::ReplCell,
        }
    }

    /// Constructs the effective compiler options for this profile.
    #[must_use]
    pub fn compilation_options(self) -> nexa_analysis::CompilationOptions {
        nexa_analysis::CompilationOptions {
            profile: self.analysis_profile(),
            ..nexa_analysis::CompilationOptions::default()
        }
    }
}

impl From<BuildProfile> for nexa_analysis::CompilationProfile {
    fn from(profile: BuildProfile) -> Self {
        profile.analysis_profile()
    }
}

impl fmt::Display for BuildProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::BuildProfile;

    #[test]
    fn profiles_freeze_source_and_entrypoint_rules() {
        assert!(BuildProfile::Package.is_manifest_backed());
        assert!(!BuildProfile::Package.allows_top_level_statements());
        assert!(!BuildProfile::Package.requires_entrypoint());

        assert!(BuildProfile::StandalonePackage.is_manifest_backed());
        assert!(!BuildProfile::StandalonePackage.allows_top_level_statements());
        assert!(BuildProfile::StandalonePackage.requires_entrypoint());

        for profile in [BuildProfile::StandaloneScript, BuildProfile::ReplCell] {
            assert!(!profile.is_manifest_backed());
            assert!(profile.allows_top_level_statements());
            assert!(profile.requires_entrypoint());
        }
    }

    #[test]
    fn profile_spellings_are_stable_and_unique() {
        let profiles = [
            BuildProfile::Package,
            BuildProfile::StandalonePackage,
            BuildProfile::StandaloneScript,
            BuildProfile::ReplCell,
        ];
        let spellings = profiles.map(BuildProfile::as_str);
        assert_eq!(
            spellings,
            [
                "package",
                "standalone-package",
                "standalone-script",
                "repl-cell",
            ]
        );
    }

    #[test]
    fn profiles_map_one_to_one_into_canonical_analysis_options() {
        let profiles = [
            BuildProfile::Package,
            BuildProfile::StandalonePackage,
            BuildProfile::StandaloneScript,
            BuildProfile::ReplCell,
        ];
        let canonical = profiles.map(|profile| {
            nexa_analysis::canonical_compilation_options(&profile.compilation_options())
        });
        for (index, left) in canonical.iter().enumerate() {
            for right in canonical.iter().skip(index + 1) {
                assert_ne!(left, right);
            }
        }
    }
}
