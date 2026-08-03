//! M5 stage-I compilation caches.
//!
//! Two layers share one discipline - any input that could change the
//! compiled artifact changes the key:
//!
//! * [`SourceCache`]: in-process, in-memory, never persisted, so it can
//!   never serve a module produced by a different build of the toolchain.
//! * [`ArtifactCache`] (WP93-95): an on-disk cache of hash-verified
//!   portable artifacts. Values are re-verified through the bytecode
//!   verifier on every load, so the cache only ever skips the frontend,
//!   never the safety boundary. Runtime pointers and dense slots are
//!   never serialized; predecoding stays a per-process load step.
//!
//! Keys are domain-separated fingerprints over the language version, the
//! bytecode version, the toolchain version (disk layer), the exact source
//! text, and the pinned Host Contract runtime identity (when one is
//! supplied). Compilation is deterministic: a hit returns a module
//! byte-identical to what a fresh compile would produce, which the
//! differential gate pins.
//!
//! Eviction is bounded FIFO in memory; the disk layer enforces a byte
//! budget by discarding the oldest artifacts first (WP94).

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nexa_core::{FingerprintBuilder, StableId};
use nexa_verifier::{VerifiedModule, VerifierLimits, verify};

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

/// Observability snapshot for the disk layer (WP93-95).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtifactCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stores: u64,
    /// Entries dropped because their length header, content hash, decode,
    /// or re-verification failed (WP94: corruption is discarded, never
    /// served).
    pub discarded: u64,
    /// Entries removed by the byte-budget cleanup, oldest first.
    pub evictions: u64,
}

/// Fixed artifact file layout: length header, content hash, payload.
const ARTIFACT_HEADER_BYTES: usize = 8 + 32;
const ARTIFACT_EXTENSION: &str = "nxac";

/// WP93-95: bounded on-disk cache of hash-verified portable artifacts.
///
/// A load never trusts the disk: the length header and content hash must
/// match, the payload must decode at the current wire version, and the
/// module re-runs the full bytecode verifier before it is served. Any
/// failure discards the entry and falls back to a fresh compile. Stores
/// are atomic (temp file + rename) and a byte budget evicts the oldest
/// artifacts first.
pub struct ArtifactCache {
    directory: PathBuf,
    max_bytes: u64,
    stats: Mutex<ArtifactCacheStats>,
}

impl ArtifactCache {
    /// Opens (and creates) the cache directory with a total byte budget.
    pub fn new(directory: impl Into<PathBuf>, max_bytes: u64) -> std::io::Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            directory,
            max_bytes,
            stats: Mutex::new(ArtifactCacheStats::default()),
        })
    }

    /// Serves `source` from the disk cache or compiles and stores it.
    pub fn compile(&self, source: &str) -> Result<Arc<VerifiedModule>, CompileError> {
        self.lookup_or_compile(source, None)
    }

    /// Contract-pinned variant of [`Self::compile`].
    pub fn compile_with_contract_id(
        &self,
        source: &str,
        host_contract_id: StableId,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        self.lookup_or_compile(source, Some(host_contract_id))
    }

    #[must_use]
    pub fn stats(&self) -> ArtifactCacheStats {
        *self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lookup_or_compile(
        &self,
        source: &str,
        host_contract_id: Option<StableId>,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        let key = Self::key(source, host_contract_id);
        let path = self.entry_path(&key);
        if let Some(module) = self.load(&path) {
            self.bump(|stats| stats.hits += 1);
            return Ok(Arc::new(module));
        }
        self.bump(|stats| stats.misses += 1);
        let module = Arc::new(match host_contract_id {
            Some(id) => crate::compile_with_contract_id(source, id)?,
            None => crate::compile(source)?,
        });
        // Storing is best effort: a full disk or permission failure must
        // never fail the compilation the caller asked for.
        if self.store(&path, &module.module().encode()).is_ok() {
            self.bump(|stats| stats.stores += 1);
            self.enforce_budget(&path);
        }
        Ok(module)
    }

    /// Reads, integrity-checks, decodes, and re-verifies one entry.
    /// Every failure discards the file so a corrupt entry is repaired by
    /// the following store instead of being served or re-tried forever.
    fn load(&self, path: &Path) -> Option<VerifiedModule> {
        let bytes = std::fs::read(path).ok()?;
        let verified = Self::decode_entry(&bytes).and_then(|payload| {
            let module = nexa_bytecode::Module::decode(payload).ok()?;
            verify(module, VerifierLimits::default()).ok()
        });
        if verified.is_none() {
            let _ = std::fs::remove_file(path);
            self.bump(|stats| stats.discarded += 1);
        }
        verified
    }

    fn decode_entry(bytes: &[u8]) -> Option<&[u8]> {
        let length = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
        let expected_total = (ARTIFACT_HEADER_BYTES as u64).checked_add(length)?;
        if bytes.len() as u64 != expected_total {
            return None;
        }
        let hash: [u8; 32] = bytes.get(8..40)?.try_into().ok()?;
        let payload = bytes.get(ARTIFACT_HEADER_BYTES..)?;
        if *blake3::hash(payload).as_bytes() != hash {
            return None;
        }
        Some(payload)
    }

    /// Atomic store: temp file in the same directory, then rename (WP94).
    fn store(&self, path: &Path, payload: &[u8]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(ARTIFACT_HEADER_BYTES + payload.len());
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(blake3::hash(payload).as_bytes());
        bytes.extend_from_slice(payload);
        let temp = self.directory.join(format!(
            ".tmp-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos())
        ));
        let result = (|| {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// WP94 bounded cleanup: keeps total artifact bytes at or under the
    /// budget by removing the oldest entries first; the entry named by
    /// `keep` (the one just stored) survives even when it alone exceeds
    /// the budget.
    fn enforce_budget(&self, keep: &Path) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        let mut artifacts = Vec::new();
        let mut total = 0_u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some(ARTIFACT_EXTENSION)
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            total = total.saturating_add(metadata.len());
            let modified = metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            artifacts.push((modified, metadata.len(), path));
        }
        artifacts.sort_by(|left, right| (left.0, &left.2).cmp(&(right.0, &right.2)));
        for (_, length, path) in artifacts {
            if total <= self.max_bytes {
                break;
            }
            if path == keep {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(length);
                self.bump(|stats| stats.evictions += 1);
            }
        }
    }

    fn entry_path(&self, key: &[u8; 32]) -> PathBuf {
        let mut name = String::with_capacity(64 + ARTIFACT_EXTENSION.len() + 1);
        for byte in key {
            use std::fmt::Write as _;
            let _ = write!(name, "{byte:02x}");
        }
        name.push('.');
        name.push_str(ARTIFACT_EXTENSION);
        self.directory.join(name)
    }

    fn bump(&self, update: impl FnOnce(&mut ArtifactCacheStats)) {
        let mut stats = self
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut stats);
    }

    /// The build fingerprint (WP93): language, bytecode, and toolchain
    /// versions plus the exact source and pinned contract identity. Any
    /// toolchain change abandons old entries by key instead of trusting
    /// cross-version decode behavior.
    fn key(source: &str, host_contract_id: Option<StableId>) -> [u8; 32] {
        let mut builder = FingerprintBuilder::new("compiler.artifact-cache", 1);
        builder.field_u32("language", u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION));
        builder.field_u32("bytecode", u32::from(nexa_core::BYTECODE_VERSION));
        builder.field_str("toolchain", nexa_core::NEXA_COMPILER_VERSION);
        builder.field_str("source", source);
        match host_contract_id {
            Some(id) => builder.field_u64("contract", id.0),
            None => builder.field_u8("contract", 0),
        }
        builder.finish_bytes()
    }
}
