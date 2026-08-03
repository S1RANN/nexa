//! M5 stage-I gate (WP93-95): the on-disk artifact cache serves
//! hash-verified, re-verified, byte-identical portable artifacts across
//! process boundaries; corruption in any layer is discarded and repaired;
//! stores are atomic; the byte budget evicts the oldest artifacts first.

use std::path::{Path, PathBuf};

use nexa_compiler::cache::ArtifactCache;

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
                bytes[9] ^= 0x01;
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
