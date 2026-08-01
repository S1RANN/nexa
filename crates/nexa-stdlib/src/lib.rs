//! Versioned, compiler-provided Nexa standard-library declarations.
//!
//! The standard library is a compile-time static library. Its descriptors do
//! not contain Host uses, capabilities, runtime realms, or ambient logging.
//! Source-backed functions are linked with the application package; compiler
//! intrinsics are resolved from the same canonical symbol records.

mod model;

pub mod collections;
pub mod core;
pub mod debug;
pub mod math;
pub mod string;

pub use model::{
    Allocation, CanonicalSymbol, DescriptorHash, Effect, FieldDescriptor, FunctionBehavior,
    FunctionDescriptor, Intrinsic, Lowering, ModuleDescriptor, ParameterDescriptor,
    StandardLibrary, StandardLibraryVersion, SymbolKind, Termination, TypeDescriptor, TypeKind,
};

/// Schema of the canonical descriptor manifest.
pub const DESCRIPTOR_SCHEMA_VERSION: u16 = 1;

/// Logical package identity used by source resolution.
pub const PACKAGE_ID: &str = "nexa.stdlib";

/// Version of the compiler-provided standard library.
pub const VERSION: StandardLibraryVersion = StandardLibraryVersion::new(1, 0, 0);

/// Versioned identity supplied to the compiler's canonical-symbol machinery.
pub const CANONICAL_PACKAGE_ID: &str = "nexa.stdlib@1.0.0";

/// Domain and schema prefix for the exact standard-library identity embedded in build
/// fingerprints.
const DESCRIPTOR_IDENTITY_PREFIX: &[u8] = b"nexa.stdlib.descriptor.v1\0";

static MODULES: [ModuleDescriptor; 5] = [
    core::MODULE,
    math::MODULE,
    string::MODULE,
    collections::MODULE,
    debug::MODULE,
];

static STANDARD_LIBRARY: StandardLibrary = StandardLibrary::new(
    DESCRIPTOR_SCHEMA_VERSION,
    PACKAGE_ID,
    CANONICAL_PACKAGE_ID,
    VERSION,
    &MODULES,
);

/// Returns the single immutable descriptor set compiled into this crate.
#[must_use]
pub const fn standard_library() -> &'static StandardLibrary {
    &STANDARD_LIBRARY
}

/// Returns the single canonical standard-library identity used by every build-fingerprint
/// producer.
///
/// The length-framed manifest is followed by its independently computed descriptor hash. Keeping
/// this framing here prevents compiler, facade, and analysis-only callers from silently assigning
/// different identities to the same compiler-provided library.
#[must_use]
pub fn canonical_descriptor_identity() -> Vec<u8> {
    let manifest = STANDARD_LIBRARY.canonical_manifest();
    let mut identity = DESCRIPTOR_IDENTITY_PREFIX.to_vec();
    identity.extend_from_slice(
        &u64::try_from(manifest.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    identity.extend_from_slice(manifest.as_bytes());
    identity.extend_from_slice(&STANDARD_LIBRARY.descriptor_hash().0.to_le_bytes());
    identity
}

#[cfg(test)]
mod tests;
