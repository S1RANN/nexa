use crate::CompilationLimits;

/// Canonical language revision embedded in every M4 build fingerprint.
pub const NEXA_LANGUAGE_VERSION: &str = "m4";
pub const COMPILATION_OPTIONS_SCHEMA_VERSION: u32 = 3;
pub const DEFAULT_MAX_WHILE_ITERATIONS: u32 = 1_000_000;

/// Complete set of caller-selectable inputs that can change analysis or emitted bytecode.
///
/// A [`crate::ResolvedBuildInput`] owns this value and the analyzer reads it from there. This
/// prevents a caller from hashing one option set and compiling with another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompilationOptions {
    pub limits: CompilationLimits,
    pub max_while_iterations: u32,
}

impl Default for CompilationOptions {
    fn default() -> Self {
        Self {
            limits: CompilationLimits::default(),
            max_while_iterations: DEFAULT_MAX_WHILE_ITERATIONS,
        }
    }
}

/// Canonical bytes for the compiler configuration which is actually applied to analysis.
///
/// Every effective compilation limit and the dynamic-while bound participates. Fixed language
/// semantics belong to `NEXA_LANGUAGE_VERSION`/the compiler version rather than masquerading as
/// caller-selectable options.
#[must_use]
pub fn canonical_compilation_options(options: &CompilationOptions) -> Vec<u8> {
    let limits = &options.limits;
    let mut bytes = b"nexa.compilation-options\0".to_vec();
    bytes.extend_from_slice(&COMPILATION_OPTIONS_SCHEMA_VERSION.to_le_bytes());
    for value in [
        limits.modules_per_package,
        limits.source_file_bytes,
        limits.source_bytes_per_package,
        limits.dependency_closure_bytes,
        limits.imports_per_module,
        limits.module_edges,
        limits.dependency_packages,
        limits.diagnostics_per_revision,
    ] {
        bytes.extend_from_slice(&u64::try_from(value).unwrap_or(u64::MAX).to_le_bytes());
    }
    bytes.extend_from_slice(&limits.max_loop_iterations.to_le_bytes());
    // Analysis normalizes zero to one iteration. Hash that effective value so callers cannot
    // create two BuildFingerprints for identical analysis/codegen behavior.
    bytes.extend_from_slice(&options.max_while_iterations.max(1).to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::{
        COMPILATION_OPTIONS_SCHEMA_VERSION, CompilationOptions, canonical_compilation_options,
    };
    use crate::CompilationLimits;

    #[test]
    fn canonical_options_include_only_the_schema_and_effective_values() {
        const PREFIX: &[u8] = b"nexa.compilation-options\0";
        let compilation_options = CompilationOptions::default();
        let options = canonical_compilation_options(&compilation_options);
        assert!(options.starts_with(PREFIX));
        assert_eq!(
            &options[PREFIX.len()..PREFIX.len() + std::mem::size_of::<u32>()],
            &COMPILATION_OPTIONS_SCHEMA_VERSION.to_le_bytes()
        );
        assert_eq!(
            options.len(),
            PREFIX.len()
                + std::mem::size_of::<u32>()
                + 8 * std::mem::size_of::<u64>()
                + 2 * std::mem::size_of::<u32>()
        );
        assert_eq!(
            options
                .get(options.len() - std::mem::size_of::<u32>()..)
                .expect("canonical options contain the dynamic-while bound"),
            compilation_options.max_while_iterations.to_le_bytes()
        );
    }

    #[test]
    fn every_effective_limit_changes_canonical_options() {
        let options = CompilationOptions::default();
        let limits = options.limits;
        let canonical = canonical_compilation_options(&options);
        let variants = [
            CompilationOptions {
                limits: CompilationLimits {
                    modules_per_package: limits.modules_per_package + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    source_file_bytes: limits.source_file_bytes + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    source_bytes_per_package: limits.source_bytes_per_package + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    dependency_closure_bytes: limits.dependency_closure_bytes + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    imports_per_module: limits.imports_per_module + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    module_edges: limits.module_edges + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    dependency_packages: limits.dependency_packages + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    diagnostics_per_revision: limits.diagnostics_per_revision + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                limits: CompilationLimits {
                    max_loop_iterations: limits.max_loop_iterations + 1,
                    ..limits
                },
                ..options
            },
            CompilationOptions {
                max_while_iterations: options.max_while_iterations + 1,
                ..options
            },
        ];

        for variant in variants {
            assert_ne!(canonical_compilation_options(&variant), canonical);
        }
    }

    #[test]
    fn zero_and_one_while_bounds_share_one_effective_identity() {
        let zero = CompilationOptions {
            max_while_iterations: 0,
            ..CompilationOptions::default()
        };
        let one = CompilationOptions {
            max_while_iterations: 1,
            ..CompilationOptions::default()
        };
        assert_eq!(
            canonical_compilation_options(&zero),
            canonical_compilation_options(&one)
        );
    }
}
