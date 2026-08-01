#[path = "../examples/generate_diagnostic_fixtures.rs"]
mod generator;

use std::collections::BTreeSet;
use std::path::Path;

use nexa_bytecode::{BYTECODE_VERSION, DecodeLimits, Module, SectionKind};

const ALL_BINARY_FIXTURES: [&str; 5] = [
    "NX3001.bin",
    "NX3002.bin",
    "NX3003.bin",
    "NX3004.bin",
    "NX6005.bin",
];

const GENERATED_BYTECODE_V6_FIXTURES: [&str; 5] =
    ["NX3001", "NX3002", "NX3003", "NX3004", "NX6005"];

fn fixture_directory() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/diagnostics/binaries")
}

fn stored_fixture_names(directory: &Path) -> BTreeSet<String> {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to scan {}: {error}", directory.display()));
    entries
        .map(|entry| {
            let entry = entry.unwrap_or_else(|error| {
                panic!(
                    "failed to read an entry in {}: {error}",
                    directory.display()
                )
            });
            let file_type = entry.file_type().unwrap_or_else(|error| {
                panic!("failed to inspect {}: {error}", entry.path().display())
            });
            assert!(
                file_type.is_file() && !file_type.is_symlink(),
                "{} must contain only regular fixture files",
                entry.path().display()
            );
            entry
                .file_name()
                .into_string()
                .unwrap_or_else(|name| panic!("non-UTF-8 fixture name: {}", name.display()))
        })
        .collect()
}

fn read_fixture_hex(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .trim()
        .to_owned()
}

#[test]
fn binary_diagnostic_fixture_inventory_is_exact_and_nonempty() {
    let directory = fixture_directory();
    let expected = ALL_BINARY_FIXTURES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    assert!(
        !expected.is_empty(),
        "the frozen fixture inventory is empty"
    );
    assert_eq!(
        stored_fixture_names(&directory),
        expected,
        "the diagnostic binary directory has a missing, unversioned, or stale fixture"
    );
}

#[test]
fn generated_diagnostic_fixtures_are_the_exact_bytecode_v6_set() {
    let directory = fixture_directory();
    let fixtures = generator::encoded_fixtures();
    let generated_names = fixtures
        .iter()
        .map(|fixture| fixture.name)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        generated_names.len(),
        fixtures.len(),
        "the fixture generator emitted duplicate names"
    );
    assert_eq!(
        generated_names,
        GENERATED_BYTECODE_V6_FIXTURES.into_iter().collect(),
        "the generator omitted a required fixture or retained an obsolete fixture"
    );

    for fixture in fixtures {
        if fixture.name == "NX3001" {
            assert_ne!(
                fixture.bytes.get(..4),
                Some(b"NXBC".as_slice()),
                "NX3001 must remain the invalid-magic fixture"
            );
        } else {
            assert_eq!(
                fixture.bytes.get(..4),
                Some(b"NXBC".as_slice()),
                "{} must retain the canonical bytecode magic",
                fixture.name
            );
        }
        let encoded_version = fixture
            .bytes
            .get(4..6)
            .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
            .map(u16::from_le_bytes);
        assert_eq!(
            encoded_version,
            Some(BYTECODE_VERSION),
            "{} must be regenerated for bytecode v{BYTECODE_VERSION}",
            fixture.name
        );
        let path = directory.join(format!("{}.bin", fixture.name));
        assert_eq!(
            read_fixture_hex(&path),
            fixture.hex(),
            "{} must be regenerated with the verifier example",
            path.display()
        );
    }
}

#[test]
fn decodable_bytecode_v6_fixtures_round_trip_canonically() {
    let mut round_tripped = BTreeSet::new();
    for fixture in generator::encoded_fixtures() {
        let Ok(module) = Module::decode(&fixture.bytes) else {
            continue;
        };
        assert_eq!(
            module.encode(),
            fixture.bytes,
            "{} must round-trip through the canonical v6 section encoding",
            fixture.name
        );
        let constants = Module::inspect_section_directory(&fixture.bytes, DecodeLimits::default())
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == SectionKind::Constants as u16)
            .unwrap();
        let start = constants.offset as usize;
        let end = start + constants.length as usize;
        assert_eq!(
            &fixture.bytes[start..end],
            0_u32.to_le_bytes(),
            "{} must retain the canonical empty constants section",
            fixture.name
        );
        round_tripped.insert(fixture.name);
    }
    assert_eq!(
        round_tripped,
        ["NX3002", "NX3003", "NX6005"].into_iter().collect()
    );
}
