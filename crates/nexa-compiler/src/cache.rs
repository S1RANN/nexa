//! M5 stage-I source compilation cache.
//!
//! In-process, in-memory only: nothing is ever persisted, so the cache can
//! never serve a module produced by a different build of the toolchain.
//! Keys are domain-separated fingerprints over the language version, the
//! bytecode version, the exact source text, and the pinned Host Contract
//! runtime identity (when one is supplied), so any input that could change
//! the compiled artifact changes the key. Compilation is deterministic:
//! a hit returns a shared `Arc` to a module byte-identical to what a fresh
//! compile would produce, which the differential gate pins.
//!
//! Eviction is bounded FIFO: product realms hold a small, stable script
//! population, and FIFO keeps the implementation obviously correct. A
//! capacity of zero disables retention (every call compiles fresh).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use nexa_core::{FingerprintBuilder, StableId};
use nexa_verifier::VerifiedModule;

use crate::CompileError;

/// Observability snapshot for gates and telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub evictions: u64,
}

struct CacheState {
    entries: BTreeMap<[u8; 32], Arc<VerifiedModule>>,
    order: VecDeque<[u8; 32]>,
    hits: u64,
    misses: u64,
    evictions: u64,
}

/// Bounded, thread-safe cache over the verified compilation pipeline.
pub struct SourceCache {
    capacity: usize,
    state: Mutex<CacheState>,
}

impl SourceCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(CacheState {
                entries: BTreeMap::new(),
                order: VecDeque::with_capacity(capacity),
                hits: 0,
                misses: 0,
                evictions: 0,
            }),
        }
    }

    /// Compiles `source` through the full verified pipeline, serving a
    /// shared module on a key hit.
    pub fn compile(&self, source: &str) -> Result<Arc<VerifiedModule>, CompileError> {
        self.lookup_or_compile(source, None)
    }

    /// Contract-pinned variant of [`Self::compile`]; the pinned identity
    /// participates in the key because it changes the compiled artifact.
    pub fn compile_with_contract_id(
        &self,
        source: &str,
        host_contract_id: StableId,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        self.lookup_or_compile(source, Some(host_contract_id))
    }

    #[must_use]
    pub fn stats(&self) -> SourceCacheStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SourceCacheStats {
            hits: state.hits,
            misses: state.misses,
            entries: state.entries.len(),
            evictions: state.evictions,
        }
    }

    fn lookup_or_compile(
        &self,
        source: &str,
        host_contract_id: Option<StableId>,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        let key = Self::key(source, host_contract_id);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(module) = state.entries.get(&key).cloned() {
                state.hits += 1;
                return Ok(module);
            }
            state.misses += 1;
        }
        // The lock is dropped during compilation: a heavyweight miss must
        // not serialize concurrent hits. A racing miss on the same key
        // compiles twice and the second insert wins; both artifacts are
        // byte-identical because compilation is deterministic.
        let module = Arc::new(match host_contract_id {
            Some(id) => crate::compile_with_contract_id(source, id)?,
            None => crate::compile(source)?,
        });
        if self.capacity == 0 {
            return Ok(module);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.entries.contains_key(&key) {
            while state.entries.len() >= self.capacity {
                let Some(oldest) = state.order.pop_front() else {
                    break;
                };
                state.entries.remove(&oldest);
                state.evictions += 1;
            }
            state.entries.insert(key, Arc::clone(&module));
            state.order.push_back(key);
        }
        Ok(module)
    }

    fn key(source: &str, host_contract_id: Option<StableId>) -> [u8; 32] {
        let mut builder = FingerprintBuilder::new("compiler.source-cache", 1);
        builder.field_u32("language", u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION));
        builder.field_u32("bytecode", u32::from(nexa_core::BYTECODE_VERSION));
        builder.field_str("source", source);
        match host_contract_id {
            Some(id) => builder.field_u64("contract", id.0),
            None => builder.field_u8("contract", 0),
        }
        builder.finish_bytes()
    }
}
