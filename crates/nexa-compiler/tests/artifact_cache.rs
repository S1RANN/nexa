//! M5 stage-I gate (WP93-95): the on-disk artifact cache addresses every
//! canonical build authority, serves versioned/key-bound, hash-verified,
//! re-verified, byte-identical portable artifacts across process boundaries;
//! corruption in any layer is discarded and repaired; stores are atomic; the
//! byte budget is a strict upper bound and evicts the oldest artifacts first.

use std::path::{Path, PathBuf};

use nexa_compiler::cache::{ArtifactCache, ArtifactCacheIdentity};
use nexa_core::BuildFingerprint;

const SOURCE_A: &str = "fn a(x: i32) -> i32 { return x + 1; }\n";
const SOURCE_B: &str = "fn b(x: i32) -> i32 { return x * 2; }\n";

fn scratch_directory(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "nexa-artifact-cache-{name}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

fn artifact_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths = std::fs::read_dir(directory)
        .expect("cache directory exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("nxac"))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn identity(
    build: u8,
    contract: Option<[u8; 32]>,
    dependencies: &[u8],
    compiler_options: &[u8],
) -> ArtifactCacheIdentity {
    ArtifactCacheIdentity::new(
        BuildFingerprint::from_bytes([build; 32]),
        contract,
        dependencies.to_vec(),
        compiler_options.to_vec(),
    )
}

#[test]
fn disk_hits_cross_instances_and_match_a_fresh_compile() {
    let directory = scratch_directory("roundtrip");
    let writer = ArtifactCache::new(&directory, u64::MAX).expect("cache opens");
    let stored = writer.compile(SOURCE_A).expect("miss compiles and stores");
    assert_eq!(writer.stats().misses, 1);
    assert_eq!(writer.stats().stores, 1);

    // A separate instance over the same directory proves the hit comes
    // from disk, not from process memory.
    let reader = ArtifactCache::new(&directory, u64::MAX).expect("cache reopens");
    let served = reader.compile(SOURCE_A).expect("hit serves");
    let stats = reader.stats();
    assert_eq!((stats.hits, stats.misses, stats.discarded), (1, 0, 0));

    let fresh = nexa_compiler::compile(SOURCE_A).expect("fresh compile");
    assert_eq!(
        served.module().encode(),
        fresh.module().encode(),
        "the disk artifact is byte-identical to a fresh compile"
    );
    assert_eq!(stored.module().encode(), served.module().encode());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn corruption_in_any_layer_is_discarded_and_repaired() {
    // Payload flip, hash flip, truncation, and trailing garbage must all
    // discard the entry, recompile, and leave a healthy file behind.
    for (label, corrupt) in [
        (
            "payload",
            Box::new(|bytes: &mut Vec<u8>| {
                let last = bytes.len() - 1;
                bytes[last] ^= 0x40;
            }) as Box<dyn Fn(&mut Vec<u8>)>,
        ),
        (
            "hash",
            Box::new(|bytes: &mut Vec<u8>| {
                // The bound content hash starts after magic, three
                // versions, the cache key, and the payload length.
                bytes[60] ^= 0x01;
            }),
        ),
        (
            "format-version",
            Box::new(|bytes: &mut Vec<u8>| {
                bytes[8] ^= 0x01;
            }),
        ),
        (
            "language-version",
            Box::new(|bytes: &mut Vec<u8>| {
                bytes[12] ^= 0x01;
            }),
        ),
        (
            "bytecode-version",
            Box::new(|bytes: &mut Vec<u8>| {
                bytes[16] ^= 0x01;
            }),
        ),
        (
            "truncation",
            Box::new(|bytes: &mut Vec<u8>| {
                bytes.truncate(bytes.len() - 3);
            }),
        ),
        (
            "trailing-garbage",
            Box::new(|bytes: &mut Vec<u8>| {
                bytes.extend_from_slice(b"junk");
            }),
        ),
    ] {
        let directory = scratch_directory(label);
        let cache = ArtifactCache::new(&directory, u64::MAX).expect("cache opens");
        cache.compile(SOURCE_A).expect("initial store");
        let paths = artifact_paths(&directory);
        assert_eq!(paths.len(), 1, "{label}: one artifact stored");
        let mut bytes = std::fs::read(&paths[0]).expect("artifact readable");
        corrupt(&mut bytes);
        std::fs::write(&paths[0], &bytes).expect("corruption written");

        let repaired = ArtifactCache::new(&directory, u64::MAX).expect("cache reopens");
        let module = repaired
            .compile(SOURCE_A)
            .expect("corruption falls back to a fresh compile");
        let stats = repaired.stats();
        assert_eq!(
            (stats.hits, stats.misses, stats.discarded, stats.stores),
            (0, 1, 1, 1),
            "{label}: the entry is discarded, recompiled, and re-stored"
        );
        // The repaired file serves again.
        let reader = ArtifactCache::new(&directory, u64::MAX).expect("cache reopens clean");
        let served = reader.compile(SOURCE_A).expect("repaired entry serves");
        assert_eq!(reader.stats().hits, 1, "{label}: repaired entry hits");
        assert_eq!(served.module().encode(), module.module().encode());
        let _ = std::fs::remove_dir_all(&directory);
    }
}

#[test]
fn the_byte_budget_evicts_the_oldest_artifact_first() {
    let directory = scratch_directory("budget");
    // Budget below two artifacts: storing the second evicts the first.
    let probe = ArtifactCache::new(&directory, u64::MAX).expect("probe opens");
    probe.compile(SOURCE_A).expect("probe store");
    let single = std::fs::metadata(&artifact_paths(&directory)[0])
        .expect("probe metadata")
        .len();
    let _ = std::fs::remove_dir_all(&directory);

    let cache = ArtifactCache::new(&directory, single + single / 2).expect("cache opens");
    cache.compile(SOURCE_A).expect("first store");
    // Filesystem mtime granularity orders the eviction scan.
    std::thread::sleep(std::time::Duration::from_millis(20));
    cache.compile(SOURCE_B).expect("second store evicts");
    assert_eq!(cache.stats().evictions, 1, "oldest artifact evicted");
    assert_eq!(artifact_paths(&directory).len(), 1, "one artifact survives");
    // The survivor is the newest one: SOURCE_B hits, SOURCE_A misses.
    let reader = ArtifactCache::new(&directory, u64::MAX).expect("cache reopens");
    reader.compile(SOURCE_B).expect("survivor serves");
    assert_eq!(reader.stats().hits, 1);
    reader.compile(SOURCE_A).expect("evicted entry recompiles");
    assert_eq!(reader.stats().misses, 1);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn every_build_authority_dimension_addresses_a_distinct_entry() {
    let directory = scratch_directory("complete-identity");
    let cache = ArtifactCache::new(&directory, u64::MAX).expect("cache opens");
    let module = nexa_compiler::compile(SOURCE_A).expect("fixture compiles");
    let mut contract = [0x41; 32];
    let base = identity(1, Some(contract), b"dependencies-a", b"options-a");
    assert!(cache.store_verified(&base, &module).expect("base stores"));
    assert!(
        cache.lookup_verified(&base).is_some(),
        "the exact complete authority hits"
    );

    let mut variants = Vec::new();
    variants.push(identity(2, Some(contract), b"dependencies-a", b"options-a"));
    contract[31] ^= 1;
    variants.push(identity(1, Some(contract), b"dependencies-a", b"options-a"));
    variants.push(identity(
        1,
        Some([0x41; 32]),
        b"dependencies-b",
        b"options-a",
    ));
    variants.push(identity(
        1,
        Some([0x41; 32]),
        b"dependencies-a",
        b"options-b",
    ));
    variants.push(identity(1, None, b"dependencies-a", b"options-a"));

    for variant in &variants {
        assert!(
            cache.lookup_verified(variant).is_none(),
            "changing any authority must miss"
        );
        assert!(
            cache
                .store_verified(variant, &module)
                .expect("variant stores"),
            "a distinct authority must receive its own entry"
        );
    }
    assert_eq!(
        artifact_paths(&directory).len(),
        variants.len() + 1,
        "Build Fingerprint, all 32 Contract bytes, dependency closure, effective options, \
         and Contract presence all participate in the key"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_portable_value_cannot_be_moved_under_another_cache_key() {
    let directory = scratch_directory("key-binding");
    let cache = ArtifactCache::new(&directory, u64::MAX).expect("cache opens");
    let module = nexa_compiler::compile(SOURCE_A).expect("fixture compiles");
    let first = identity(1, Some([1; 32]), b"dependencies", b"options");
    let second = identity(2, Some([1; 32]), b"dependencies", b"options");

    cache.store_verified(&first, &module).expect("first stores");
    let first_path = artifact_paths(&directory)
        .into_iter()
        .next()
        .expect("first path exists");
    cache
        .store_verified(&second, &module)
        .expect("second stores");
    let second_path = artifact_paths(&directory)
        .into_iter()
        .find(|path| path != &first_path)
        .expect("second path exists");
    let first_bytes = std::fs::read(&first_path).expect("first artifact reads");
    std::fs::write(&second_path, first_bytes).expect("copied artifact writes");

    let reader = ArtifactCache::new(&directory, u64::MAX).expect("cache reopens");
    assert!(
        reader.lookup_verified(&second).is_none(),
        "the header key and bound hash reject a value copied under another key"
    );
    assert!(
        reader.lookup_verified(&first).is_some(),
        "the original key remains healthy"
    );
    assert_eq!(reader.stats().discarded, 1);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_entry_larger_than_the_total_budget_is_never_persisted() {
    let directory = scratch_directory("strict-budget");
    let probe = ArtifactCache::new(&directory, u64::MAX).expect("probe opens");
    probe.compile(SOURCE_A).expect("probe store");
    let single = std::fs::metadata(&artifact_paths(&directory)[0])
        .expect("probe metadata")
        .len();
    let _ = std::fs::remove_dir_all(&directory);

    let cache = ArtifactCache::new(&directory, single - 1).expect("cache opens");
    cache
        .compile(SOURCE_A)
        .expect("compilation succeeds without persistence");
    assert!(
        artifact_paths(&directory).is_empty(),
        "the configured total size is a strict upper bound"
    );
    assert_eq!(cache.stats().stores, 0);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn stores_are_atomic_and_leave_no_temp_files() {
    let directory = scratch_directory("atomic");
    let cache = ArtifactCache::new(&directory, u64::MAX).expect("cache opens");
    cache.compile(SOURCE_A).expect("store");
    cache.compile(SOURCE_B).expect("store");
    let leftovers = std::fs::read_dir(&directory)
        .expect("cache directory exists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "temporary files must never survive a store: {leftovers:?}"
    );
    assert_eq!(artifact_paths(&directory).len(), 2);
    let _ = std::fs::remove_dir_all(&directory);
}
