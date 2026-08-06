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
//! Disk keys are domain-separated fingerprints over the complete canonical
//! build identity: Build Fingerprint, language and bytecode versions, the
//! full 32-byte Host Contract fingerprint, dependency closure, effective
//! compiler options, and toolchain version. Compilation is deterministic: a
//! hit returns a module byte-identical to what a fresh compile would produce,
//! which the differential gate pins.
//!
//! Eviction is bounded FIFO in memory; the disk layer enforces a byte
//! budget by discarding the oldest artifacts first (WP94).

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nexa_analysis::{ResolvedBuildInput, source_set_fingerprint};
use nexa_core::{BuildFingerprint, FileId, FingerprintBuilder, StableId};
use nexa_contract::ValidatedContract;
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

/// Complete caller authority used to address one portable artifact.
///
/// `dependencies` and `compiler_options` are canonical bytes, not display
/// strings. [`Self::from_resolved_build`] derives both from the validated
/// [`ResolvedBuildInput`] used by the package pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactCacheIdentity {
    build_fingerprint: BuildFingerprint,
    contract_fingerprint: Option<[u8; 32]>,
    dependencies: Arc<[u8]>,
    compiler_options: Arc<[u8]>,
}

impl ArtifactCacheIdentity {
    /// Creates an identity from already-canonical build authorities.
    ///
    /// Product callers should normally use [`Self::from_resolved_build`].
    #[must_use]
    pub fn new(
        build_fingerprint: BuildFingerprint,
        contract_fingerprint: Option<[u8; 32]>,
        dependencies: impl Into<Arc<[u8]>>,
        compiler_options: impl Into<Arc<[u8]>>,
    ) -> Self {
        Self {
            build_fingerprint,
            contract_fingerprint,
            dependencies: dependencies.into(),
            compiler_options: compiler_options.into(),
        }
    }

    /// Derives the exact cache authority from a validated product input.
    #[must_use]
    pub fn from_resolved_build(
        input: &ResolvedBuildInput,
        contract_fingerprint: Option<[u8; 32]>,
    ) -> Self {
        let mut dependencies = FingerprintBuilder::new("compiler.artifact-dependencies", 1);
        dependencies.field_bytes(
            "resolved-graph",
            &input.dependency_graph.canonical_identity_bytes(),
        );
        dependencies.field_bytes("lock-graph", &input.canonical_lock_graph);
        dependencies.field_u64(
            "dependency-count",
            u64::try_from(input.dependency_source_sets.len()).unwrap_or(u64::MAX),
        );
        for (package, sources) in input.dependency_source_sets.iter() {
            dependencies.field_str("package", package.as_str());
            dependencies.field_bytes(
                "manifest",
                &input
                    .dependency_manifests
                    .get(package)
                    .expect("ResolvedBuildInput validates the dependency manifest set")
                    .canonical_bytes(),
            );
            dependencies.field_bytes("source-set", source_set_fingerprint(sources).as_bytes());
        }
        Self::new(
            input.build_fingerprint,
            contract_fingerprint,
            dependencies.finish_bytes().to_vec(),
            input.fingerprint_input.compiler_options.clone(),
        )
    }

    #[must_use]
    pub const fn build_fingerprint(&self) -> BuildFingerprint {
        self.build_fingerprint
    }

    #[must_use]
    pub const fn contract_fingerprint(&self) -> Option<[u8; 32]> {
        self.contract_fingerprint
    }

    #[must_use]
    pub fn dependencies(&self) -> &[u8] {
        &self.dependencies
    }

    #[must_use]
    pub fn compiler_options(&self) -> &[u8] {
        &self.compiler_options
    }

    fn key(&self) -> [u8; 32] {
        let mut builder = FingerprintBuilder::new("compiler.artifact-cache", 2);
        builder.field_bytes("build", self.build_fingerprint.as_bytes());
        builder.field_u32("language", u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION));
        builder.field_u32("bytecode", u32::from(nexa_core::BYTECODE_VERSION));
        builder.field_str("toolchain", nexa_core::NEXA_COMPILER_VERSION);
        match self.contract_fingerprint {
            Some(fingerprint) => {
                builder.field_u8("contract-present", 1);
                builder.field_bytes("contract", &fingerprint);
            }
            None => builder.field_u8("contract-present", 0),
        }
        builder.field_bytes("dependencies", &self.dependencies);
        builder.field_bytes("compiler-options", &self.compiler_options);
        builder.finish_bytes()
    }
}

/// Versioned artifact file layout:
/// magic, cache format, language version, bytecode version, cache key,
/// payload length, bound content hash, payload.
const ARTIFACT_MAGIC: [u8; 8] = *b"NXACM5\0\0";
const ARTIFACT_FORMAT_VERSION: u32 = 1;
const ARTIFACT_HEADER_BYTES: usize = 8 + 4 + 4 + 4 + 32 + 8 + 32;
const ARTIFACT_EXTENSION: &str = "nxac";
const ARTIFACT_IDENTITY_CACHE_CAPACITY: usize = 64;
static ARTIFACT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ArtifactIdentityCache {
    entries: BTreeMap<[u8; 32], ArtifactCacheIdentity>,
    order: VecDeque<[u8; 32]>,
}

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
    identities: Mutex<ArtifactIdentityCache>,
    mutation: Mutex<()>,
    stats: Mutex<ArtifactCacheStats>,
}

impl ArtifactCache {
    /// Opens (and creates) the cache directory with a total byte budget.
    pub fn new(directory: impl Into<PathBuf>, max_bytes: u64) -> std::io::Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;
        let cache = Self {
            directory,
            max_bytes,
            identities: Mutex::new(ArtifactIdentityCache {
                entries: BTreeMap::new(),
                order: VecDeque::with_capacity(ARTIFACT_IDENTITY_CACHE_CAPACITY),
            }),
            mutation: Mutex::new(()),
            stats: Mutex::new(ArtifactCacheStats::default()),
        };
        cache.enforce_budget(None);
        Ok(cache)
    }

    /// Serves `source` from the disk cache or compiles and stores it.
    pub fn compile(&self, source: &str) -> Result<Arc<VerifiedModule>, CompileError> {
        let identity = self.snippet_identity(source, None)?;
        self.lookup_or_compile(source, None, &identity)
    }

    /// Full-fingerprint Contract variant of [`Self::compile`].
    pub fn compile_with_contract(
        &self,
        source: &str,
        contract: &ValidatedContract,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        let identity = self.snippet_identity(source, Some(contract))?;
        self.lookup_or_compile(source, Some(contract), &identity)
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
        contract: Option<&ValidatedContract>,
        identity: &ArtifactCacheIdentity,
    ) -> Result<Arc<VerifiedModule>, CompileError> {
        if let Some(module) = self.lookup_verified(identity) {
            return Ok(module);
        }
        let module = Arc::new(match contract {
            Some(contract) => crate::compile_with_contract(source, contract)?,
            None => crate::compile(source)?,
        });
        let _ = self.store_verified(identity, &module);
        Ok(module)
    }

    /// Memoizes virtual-package identity construction separately from disk
    /// values. Warm artifact hits still validate the complete build key but
    /// do not repeatedly rebuild manifests, source sets, and dependency
    /// authorities merely to rediscover the same fingerprint.
    fn snippet_identity(
        &self,
        source: &str,
        contract: Option<&ValidatedContract>,
    ) -> Result<ArtifactCacheIdentity, CompileError> {
        let mut lookup = FingerprintBuilder::new("compiler.artifact-cache-snippet-identity", 1);
        lookup.field_str("source", source);
        let contract_fingerprint = contract.map(|contract| {
            let fingerprint = nexa_contract::contract_fingerprint(contract).into_bytes();
            lookup.field_u8("contract-present", 1);
            lookup.field_bytes("contract", &fingerprint);
            lookup.field_str("contract-source", &contract.source);
            fingerprint
        });
        if contract.is_none() {
            lookup.field_u8("contract-present", 0);
        }
        let lookup = lookup.finish_bytes();
        {
            let identities = self
                .identities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(identity) = identities.entries.get(&lookup) {
                return Ok(identity.clone());
            }
        }

        let input =
            crate::snippet::resolved_snippet_for_cache(source, FileId::default(), contract)?;
        let identity = ArtifactCacheIdentity::from_resolved_build(&input, contract_fingerprint);
        let mut identities = self
            .identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !identities.entries.contains_key(&lookup) {
            while identities.entries.len() >= ARTIFACT_IDENTITY_CACHE_CAPACITY {
                let Some(oldest) = identities.order.pop_front() else {
                    break;
                };
                identities.entries.remove(&oldest);
            }
            identities.entries.insert(lookup, identity.clone());
            identities.order.push_back(lookup);
        }
        Ok(identity)
    }

    /// Looks up one exact build authority, then decodes and re-verifies its
    /// portable module. A miss is observable and never compiles implicitly.
    #[must_use]
    pub fn lookup_verified(&self, identity: &ArtifactCacheIdentity) -> Option<Arc<VerifiedModule>> {
        let key = identity.key();
        let path = self.entry_path(&key);
        if let Some(module) = self.load(&path, &key) {
            self.bump(|stats| stats.hits += 1);
            Some(Arc::new(module))
        } else {
            self.bump(|stats| stats.misses += 1);
            None
        }
    }

    /// Atomically stores one already-verified portable module.
    ///
    /// Returns `Ok(false)` when the entry cannot fit the configured total
    /// byte budget. Cache persistence remains best effort for compile callers.
    pub fn store_verified(
        &self,
        identity: &ArtifactCacheIdentity,
        module: &VerifiedModule,
    ) -> std::io::Result<bool> {
        let key = identity.key();
        let path = self.entry_path(&key);
        let _mutation = self
            .mutation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = self.store(&path, &key, &module.module().encode())?;
        if stored {
            self.bump(|stats| stats.stores += 1);
            self.enforce_budget(Some(&path));
        }
        Ok(stored)
    }

    /// Reads, integrity-checks, decodes, and re-verifies one entry.
    /// Every failure discards the file so a corrupt entry is repaired by
    /// the following store instead of being served or re-tried forever.
    fn load(&self, path: &Path, expected_key: &[u8; 32]) -> Option<VerifiedModule> {
        let mut file = std::fs::File::open(path).ok()?;
        let metadata = file.metadata().ok()?;
        let decode_limits = nexa_bytecode::DecodeLimits::default();
        let maximum_entry_bytes = u64::try_from(
            ARTIFACT_HEADER_BYTES
                .checked_add(decode_limits.max_bytes)
                .expect("default decode limit and cache header fit usize"),
        )
        .unwrap_or(u64::MAX)
        .min(self.max_bytes);
        let verified = (metadata.len() >= ARTIFACT_HEADER_BYTES as u64
            && metadata.len() <= maximum_entry_bytes)
            .then(|| {
                let mut bytes =
                    Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
                std::io::Read::take(&mut file, maximum_entry_bytes.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .ok()?;
                (bytes.len() as u64 == metadata.len()).then_some(bytes)
            })
            .flatten()
            .and_then(|bytes| {
                let payload = Self::decode_entry(&bytes, expected_key)?;
                let module =
                    nexa_bytecode::Module::decode_with_limits(payload, decode_limits).ok()?;
                verify(module, VerifierLimits::default()).ok()
            });
        if verified.is_none() {
            let _mutation = self
                .mutation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = std::fs::remove_file(path);
            self.bump(|stats| stats.discarded += 1);
        }
        verified
    }

    fn decode_entry<'a>(bytes: &'a [u8], expected_key: &[u8; 32]) -> Option<&'a [u8]> {
        if bytes.get(..8)? != ARTIFACT_MAGIC {
            return None;
        }
        let format = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
        let language = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?);
        let bytecode = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?);
        let key: [u8; 32] = bytes.get(20..52)?.try_into().ok()?;
        let length = u64::from_le_bytes(bytes.get(52..60)?.try_into().ok()?);
        let hash: [u8; 32] = bytes.get(60..92)?.try_into().ok()?;
        if format != ARTIFACT_FORMAT_VERSION
            || language != u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION)
            || bytecode != u32::from(nexa_core::BYTECODE_VERSION)
            || key != *expected_key
        {
            return None;
        }
        let expected_total = (ARTIFACT_HEADER_BYTES as u64).checked_add(length)?;
        if bytes.len() as u64 != expected_total {
            return None;
        }
        let payload = bytes.get(ARTIFACT_HEADER_BYTES..)?;
        if Self::entry_content_hash(&key, payload) != hash {
            return None;
        }
        Some(payload)
    }

    fn entry_content_hash(key: &[u8; 32], payload: &[u8]) -> [u8; 32] {
        let mut builder = FingerprintBuilder::new("compiler.artifact-cache-entry", 1);
        builder.field_u32("format", ARTIFACT_FORMAT_VERSION);
        builder.field_u32("language", u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION));
        builder.field_u32("bytecode", u32::from(nexa_core::BYTECODE_VERSION));
        builder.field_bytes("key", key);
        builder.field_bytes("payload", payload);
        builder.finish_bytes()
    }

    /// Atomic store: temp file in the same directory, then rename (WP94).
    fn store(&self, path: &Path, key: &[u8; 32], payload: &[u8]) -> std::io::Result<bool> {
        let decode_limits = nexa_bytecode::DecodeLimits::default();
        let Some(entry_bytes) = ARTIFACT_HEADER_BYTES.checked_add(payload.len()) else {
            return Ok(false);
        };
        if payload.len() > decode_limits.max_bytes
            || u64::try_from(entry_bytes).unwrap_or(u64::MAX) > self.max_bytes
        {
            return Ok(false);
        }
        let mut bytes = Vec::with_capacity(entry_bytes);
        bytes.extend_from_slice(&ARTIFACT_MAGIC);
        bytes.extend_from_slice(&ARTIFACT_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&u32::from(nexa_analysis::NEXA_LANGUAGE_VERSION).to_le_bytes());
        bytes.extend_from_slice(&u32::from(nexa_core::BYTECODE_VERSION).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&Self::entry_content_hash(key, payload));
        bytes.extend_from_slice(payload);
        let temp = self.directory.join(format!(
            ".tmp-{}-{:x}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
            ARTIFACT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)?;
            // The rename is already atomic. Syncing the directory makes it
            // durable on filesystems which support directory fsync.
            let _ = std::fs::File::open(&self.directory).and_then(|directory| directory.sync_all());
            Ok(true)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// WP94 bounded cleanup: keeps total artifact bytes at or under the
    /// configured budget by removing the oldest entries first. `keep` is
    /// only used after a store which was already proven to fit by itself.
    fn enforce_budget(&self, keep: Option<&Path>) {
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
            if keep.is_some_and(|keep| path == keep) {
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
}
