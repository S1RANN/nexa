//! M5 stage-I gate: the source compilation cache serves byte-identical
//! artifacts, keys on the pinned contract identity, respects its bound,
//! and never caches failures.

use nexa_compiler::cache::SourceCache;
use nexa_core::StableId;

const SOURCE_A: &str = "fn a(x: i32) -> i32 { return x + 1; }\n";
const SOURCE_B: &str = "fn b(x: i32) -> i32 { return x * 2; }\n";
const SOURCE_C: &str = "fn c(x: i32) -> i32 { return x - 3; }\n";

#[test]
fn hits_serve_the_byte_identical_shared_module() {
    let cache = SourceCache::new(8);
    let fresh = nexa_compiler::compile(SOURCE_A).expect("fresh compile");
    let first = cache.compile(SOURCE_A).expect("miss compiles");
    let second = cache.compile(SOURCE_A).expect("hit serves");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "a hit shares the cached module"
    );
    assert_eq!(
        first.module().encode(),
        fresh.module().encode(),
        "the cached artifact is byte-identical to a fresh compile"
    );
    let stats = cache.stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.entries, 1);
}

#[test]
fn pinned_contract_identity_participates_in_the_key() {
    let cache = SourceCache::new(8);
    let plain = cache.compile(SOURCE_A).expect("plain compile");
    let pinned = cache
        .compile_with_contract_id(SOURCE_A, StableId::from_name("cache-gate-host"))
        .expect("pinned compile");
    assert!(
        !std::sync::Arc::ptr_eq(&plain, &pinned),
        "the pinned identity changes the artifact, so it must change the key"
    );
    assert_ne!(
        plain.module().encode(),
        pinned.module().encode(),
        "the pinned module differs from the plain one"
    );
    let stats = cache.stats();
    assert_eq!(stats.misses, 2);
    assert_eq!(stats.entries, 2);
    // Each variant hits its own entry afterwards.
    cache.compile(SOURCE_A).expect("plain hit");
    cache
        .compile_with_contract_id(SOURCE_A, StableId::from_name("cache-gate-host"))
        .expect("pinned hit");
    assert_eq!(cache.stats().hits, 2);
}

#[test]
fn the_bound_is_enforced_by_fifo_eviction() {
    let cache = SourceCache::new(2);
    cache.compile(SOURCE_A).expect("a");
    cache.compile(SOURCE_B).expect("b");
    cache.compile(SOURCE_C).expect("c evicts a");
    let stats = cache.stats();
    assert_eq!(stats.entries, 2);
    assert_eq!(stats.evictions, 1);
    cache
        .compile(SOURCE_A)
        .expect("a recompiles after eviction");
    assert_eq!(cache.stats().misses, 4, "the evicted entry is a miss again");
}

#[test]
fn zero_capacity_disables_retention() {
    let cache = SourceCache::new(0);
    cache.compile(SOURCE_A).expect("first");
    cache.compile(SOURCE_A).expect("second");
    let stats = cache.stats();
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.misses, 2);
    assert_eq!(stats.entries, 0);
}

#[test]
fn failures_are_never_cached() {
    let cache = SourceCache::new(8);
    assert!(cache.compile("fn broken(").is_err());
    assert!(cache.compile("fn broken(").is_err());
    let stats = cache.stats();
    assert_eq!(stats.misses, 2, "every failing compile is a fresh attempt");
    assert_eq!(stats.entries, 0);
}
