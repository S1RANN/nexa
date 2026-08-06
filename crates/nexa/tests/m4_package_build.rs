use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use nexa::{
    CandidateIdentity, HostContractInput, LeafDiagnosticRenderer, PackageBuildError,
    PackageBuildSession, PackagePipelineStats, SourceIdentity,
    canonical_host_contract_source_identity,
    canonical_package_build_fingerprint_input_with_contract,
};
use nexa_analysis::{
    CompilationLimits, FingerprintBuilder, LockFile, NormalizedPackagePath, PackageCatalog,
    PackageId, PackageLocation, PackageManifest, PackageSourceSet, ResolvedBuildInput, SourceId,
    SourceRole, SourceSetBuilder, load_package_directory, load_package_directory_without_lock,
};
use serde::Serialize;

const BASE_MODULES: usize = 5;
const WORK_MODULES: usize = 100;
const SYMBOLS_PER_MODULE: usize = 10;
const PACKAGE_COUNT: usize = 20;
const ROOT_PACKAGE: &str = "scale.application";
const ROOT_DIRECTORY: &str = "packages/root";
const SCALE_HOST_NIDL: &str = "contract ScaleHost;\n";
const SCALE_HOST_SOURCE_PATH: &str = "nidl://tests/m4-scale/host-contract.nidl";
const WORKER_COUNT: usize = 4;
static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);

fn scale_host_contract<'a>(
    contract: &'a nexa::ValidatedContract,
    source: &str,
) -> HostContractInput<'a> {
    HostContractInput::with_source(
        contract,
        SourceIdentity::standalone(SCALE_HOST_SOURCE_PATH),
        source,
    )
    .expect("exact scale Host NIDL source")
}

const SCENARIOS: [&str; 10] = [
    "forward",
    "reverse",
    "random_seed_1",
    "random_seed_2",
    "cold_cache",
    "hot_cache",
    "temp_root_a",
    "temp_root_b",
    "worker_order_a",
    "worker_order_b",
];

#[derive(Serialize)]
struct FacadeScaleReport {
    schema: u32,
    status: &'static str,
    closure_identity: String,
    scale: ScaleCounters,
    pipeline: PipelineCounters,
    diagnostics: DiagnosticEvidence,
    query_cache: QueryCacheEvidence,
    scenarios: BTreeMap<String, ScenarioEvidence>,
}

#[derive(Clone, Copy, Serialize)]
struct ScaleCounters {
    modules: u64,
    symbols: u64,
    package_modules: u64,
    package_symbols: u64,
    import_edges: u64,
    packages: u64,
}

#[derive(Clone, Copy, Default, Serialize)]
struct PipelineCounters {
    analyzer_runs: u64,
    invalid_analyzer_runs: u64,
    successful_check_analyzer_runs: u64,
    compile_analyzer_runs: u64,
    typed_compiler_runs: u64,
    verifier_runs: u64,
    module_encode_runs: u64,
    module_bytes_length: u64,
}

impl PipelineCounters {
    fn record_facade_stats(&mut self, stats: PackagePipelineStats) {
        self.analyzer_runs = self.analyzer_runs.checked_add(stats.analyzer_runs).unwrap();
        self.invalid_analyzer_runs = self
            .invalid_analyzer_runs
            .checked_add(stats.invalid_check_analyzer_runs)
            .unwrap();
        self.successful_check_analyzer_runs = self
            .successful_check_analyzer_runs
            .checked_add(stats.successful_check_analyzer_runs)
            .unwrap();
        self.compile_analyzer_runs = self
            .compile_analyzer_runs
            .checked_add(stats.compile_analyzer_runs)
            .unwrap();
        self.typed_compiler_runs = self
            .typed_compiler_runs
            .checked_add(stats.typed_compiler_runs)
            .unwrap();
        self.verifier_runs = self.verifier_runs.checked_add(stats.verifier_runs).unwrap();
    }

    fn record_encoded_module(&mut self, bytes_length: usize) {
        self.module_encode_runs = self
            .module_encode_runs
            .checked_add(1)
            .expect("scale module-encode event count fits u64");
        self.module_bytes_length = self
            .module_bytes_length
            .max(u64::try_from(bytes_length).expect("scale module byte length fits u64"));
    }

    fn merge(&mut self, other: Self) {
        self.analyzer_runs = self.analyzer_runs.checked_add(other.analyzer_runs).unwrap();
        self.invalid_analyzer_runs = self
            .invalid_analyzer_runs
            .checked_add(other.invalid_analyzer_runs)
            .unwrap();
        self.successful_check_analyzer_runs = self
            .successful_check_analyzer_runs
            .checked_add(other.successful_check_analyzer_runs)
            .unwrap();
        self.compile_analyzer_runs = self
            .compile_analyzer_runs
            .checked_add(other.compile_analyzer_runs)
            .unwrap();
        self.typed_compiler_runs = self
            .typed_compiler_runs
            .checked_add(other.typed_compiler_runs)
            .unwrap();
        self.verifier_runs = self.verifier_runs.checked_add(other.verifier_runs).unwrap();
        self.module_encode_runs = self
            .module_encode_runs
            .checked_add(other.module_encode_runs)
            .unwrap();
        self.module_bytes_length = self.module_bytes_length.max(other.module_bytes_length);
    }
}

#[derive(Serialize)]
struct DiagnosticEvidence {
    format: &'static str,
    scenario_runs: u64,
    records: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct QueryRunEvidence {
    revision: u64,
    parsed_sources: u64,
    analyzed_modules: u64,
    reused_queries: u64,
    invalidated_queries: u64,
    cumulative_hits: u64,
    cumulative_misses: u64,
    cumulative_writes: u64,
    cumulative_invalidations: u64,
}

impl QueryRunEvidence {
    fn from_check(check: &nexa::PackageCheckReport) -> Self {
        Self {
            revision: check.query_report.revision,
            parsed_sources: u64::try_from(check.query_report.parsed_sources.len()).unwrap(),
            analyzed_modules: u64::try_from(check.query_report.analyzed_modules.len()).unwrap(),
            reused_queries: u64::try_from(check.query_report.reused_queries.len()).unwrap(),
            invalidated_queries: u64::try_from(check.query_report.invalidated_queries.len())
                .unwrap(),
            cumulative_hits: check.query_stats.hits,
            cumulative_misses: check.query_stats.misses,
            cumulative_writes: check.query_stats.writes,
            cumulative_invalidations: check.query_stats.invalidations,
        }
    }
}

#[derive(Serialize)]
struct QueryCacheEvidence {
    cold: QueryRunEvidence,
    hot: QueryRunEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ScenarioEvidence {
    artifact_bytes_digest: String,
    diagnostic_ndjson_digest: String,
    diagnostic_records: u64,
    source_fingerprint: String,
    public_api_fingerprint: String,
    state_schema_fingerprint: String,
    build_fingerprint: String,
    linked_state_fingerprint: String,
    closure_identity: String,
    lock_digest: String,
    compiled_package_ids: Vec<String>,
    compiled_module_ids: Vec<String>,
    mechanism: &'static str,
    filesystem_root_digest: Option<String>,
    loaded_package_directories: u64,
    loaded_package_ids: Vec<String>,
    worker_completion_order: Vec<u64>,
    max_in_flight: u64,
}

impl ScenarioEvidence {
    fn deterministic_payload_eq(&self, other: &Self) -> bool {
        self.artifact_bytes_digest == other.artifact_bytes_digest
            && self.diagnostic_ndjson_digest == other.diagnostic_ndjson_digest
            && self.diagnostic_records == other.diagnostic_records
            && self.source_fingerprint == other.source_fingerprint
            && self.public_api_fingerprint == other.public_api_fingerprint
            && self.state_schema_fingerprint == other.state_schema_fingerprint
            && self.build_fingerprint == other.build_fingerprint
            && self.linked_state_fingerprint == other.linked_state_fingerprint
            && self.closure_identity == other.closure_identity
            && self.lock_digest == other.lock_digest
            && self.compiled_package_ids == other.compiled_package_ids
            && self.compiled_module_ids == other.compiled_module_ids
    }
}

struct ScaleFixture {
    input: ResolvedBuildInput,
    lock_bytes: Vec<u8>,
    loaded_directory_packages: Vec<String>,
}

struct TempScaleRoot {
    path: PathBuf,
}

impl TempScaleRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexa-m4-facade-scale-{}-{sequence}-{label}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempScaleRoot {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            panic!(
                "failed to remove M4 facade scale temp root {}: {error}",
                self.path.display()
            );
        }
    }
}

fn module_names() -> Vec<String> {
    (0..BASE_MODULES)
        .map(|index| format!("scale.base{index}"))
        .chain((0..WORK_MODULES).map(|index| format!("scale.m{index:03}")))
        .collect()
}

fn module_source(module: &str, invalid: bool) -> String {
    if invalid && module == "scale.base0" {
        return "pub fn broken( -> i32 { return 1; }\n".into();
    }
    let mut source = String::new();
    if module.contains(".m") {
        for base in 0..BASE_MODULES {
            writeln!(source, "use package::scale::base{base};").unwrap();
        }
    }
    for symbol in 0..SYMBOLS_PER_MODULE {
        writeln!(source, "pub const SYMBOL_{symbol}: i32 = {symbol};").unwrap();
    }
    if module == "scale.base0" {
        writeln!(source, "pub fn boot() -> i32 {{ return SYMBOL_0; }}").unwrap();
    }
    source
}

fn root_manifest_source() -> String {
    let mut dependencies = String::new();
    for index in 0..(PACKAGE_COUNT - 1) {
        writeln!(
            dependencies,
            "lib_{index:02} = {{ path = \"../lib{index:02}\" }}"
        )
        .unwrap();
    }
    format!(
        r#"
schema = 2
kind = "application"
id = "{ROOT_PACKAGE}"
name = "Scale Application"
version = "1.0.0"
source_root = "src"
entry = "scale.base0"
activation = "programmatic"

[dependencies]
{dependencies}
"#
    )
}

fn root_manifest() -> Arc<PackageManifest> {
    Arc::new(PackageManifest::parse(&root_manifest_source()).unwrap())
}

fn library_manifest_source(index: usize) -> String {
    format!(
        r#"
schema = 2
kind = "library"
id = "scale.lib{index:02}"
name = "Scale Library {index}"
version = "1.0.0"
source_root = "src"
"#
    )
}

fn library_manifest(index: usize) -> Arc<PackageManifest> {
    Arc::new(PackageManifest::parse(&library_manifest_source(index)).unwrap())
}

fn source_set(
    package: &PackageId,
    sources: impl IntoIterator<Item = (NormalizedPackagePath, String)>,
) -> Arc<PackageSourceSet> {
    let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
    for (path, source) in sources {
        builder.add(path, source, SourceRole::Production).unwrap();
    }
    Arc::new(builder.build().unwrap())
}

fn scale_fixture(
    module_order: &[usize],
    reverse_packages: bool,
    source_name: &str,
    invalid: bool,
    contract: &HostContractInput<'_>,
) -> ScaleFixture {
    let source_id = SourceId::new(source_name).unwrap();
    let root_manifest = root_manifest();
    let names = module_names();
    let root_sources = source_set(
        &root_manifest.id,
        module_order.iter().map(|index| {
            let module = &names[*index];
            (
                NormalizedPackagePath::new(format!("src/{}.nexa", module.replace('.', "/")))
                    .unwrap(),
                module_source(module, invalid),
            )
        }),
    );
    let mut libraries = (0..(PACKAGE_COUNT - 1))
        .map(|index| {
            let manifest = library_manifest(index);
            let module = format!("scale.lib{index:02}");
            let sources = source_set(
                &manifest.id,
                [(
                    NormalizedPackagePath::new(format!("src/{}.nexa", module.replace('.', "/")))
                        .unwrap(),
                    module_source(&module, false),
                )],
            );
            (
                manifest,
                sources,
                NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    if reverse_packages {
        libraries.reverse();
    }

    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let mut catalog = PackageCatalog::new();
    catalog
        .insert(PackageLocation {
            source_id: source_id.clone(),
            directory: root_directory.clone(),
            manifest: Arc::clone(&root_manifest),
        })
        .unwrap();
    for (manifest, _, directory) in &libraries {
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: directory.clone(),
                manifest: Arc::clone(manifest),
            })
            .unwrap();
    }
    let graph = Arc::new(
        catalog
            .resolve(&source_id, &root_directory, CompilationLimits::default())
            .unwrap(),
    );
    let dependency_manifests = libraries
        .iter()
        .map(|(manifest, _, _)| (manifest.id.clone(), Arc::clone(manifest)))
        .collect::<BTreeMap<_, _>>();
    let dependency_source_sets = libraries
        .iter()
        .map(|(manifest, sources, _)| (manifest.id.clone(), Arc::clone(sources)))
        .collect::<BTreeMap<_, _>>();
    let lock = Arc::new(LockFile::from_graph(&graph));
    let lock_bytes = lock.canonical_bytes();
    let fingerprint_input = canonical_package_build_fingerprint_input_with_contract(
        &root_manifest,
        &root_sources,
        &dependency_manifests,
        &dependency_source_sets,
        contract,
        Some(&lock),
    );
    let input = ResolvedBuildInput::new(
        root_manifest,
        root_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        Some(lock),
        Arc::<[u8]>::from(fingerprint_input.host_contract.clone()),
        canonical_host_contract_source_identity(contract),
        fingerprint_input.host_required_entrypoints.clone(),
        nexa_analysis::CompilationOptions::default(),
        fingerprint_input,
    )
    .unwrap();
    ScaleFixture {
        input,
        lock_bytes,
        loaded_directory_packages: Vec::new(),
    }
}

fn write_module_file(package_root: &Path, module: &str, invalid: bool) {
    let path = package_root
        .join("src")
        .join(format!("{}.nexa", module.replace('.', "/")));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, module_source(module, invalid)).unwrap();
}

fn write_scale_directory_tree(root: &Path) {
    let packages = root.join("packages");
    let application = packages.join("root");
    fs::create_dir_all(&application).unwrap();
    fs::write(application.join("package.toml"), root_manifest_source()).unwrap();
    for module in module_names() {
        write_module_file(&application, &module, false);
    }

    for index in 0..(PACKAGE_COUNT - 1) {
        let library = packages.join(format!("lib{index:02}"));
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join("package.toml"), library_manifest_source(index)).unwrap();
        write_module_file(&library, &format!("scale.lib{index:02}"), false);
    }

    let source_id = SourceId::new("scale-directory-source").unwrap();
    let mut catalog = PackageCatalog::new();
    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let loaded_root =
        load_package_directory_without_lock(&application, CompilationLimits::default()).unwrap();
    catalog
        .insert(PackageLocation {
            source_id: source_id.clone(),
            directory: root_directory.clone(),
            manifest: loaded_root.manifest,
        })
        .unwrap();
    for index in 0..(PACKAGE_COUNT - 1) {
        let directory = NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap();
        let loaded = load_package_directory_without_lock(
            packages.join(format!("lib{index:02}")),
            CompilationLimits::default(),
        )
        .unwrap();
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory,
                manifest: loaded.manifest,
            })
            .unwrap();
    }
    let graph = catalog
        .resolve(&source_id, &root_directory, CompilationLimits::default())
        .unwrap();
    fs::write(
        application.join("nexa.lock"),
        LockFile::from_graph(&graph).canonical_bytes(),
    )
    .unwrap();
}

fn load_scale_directory_fixture(root: &Path, contract: &HostContractInput<'_>) -> ScaleFixture {
    let packages = root.join("packages");
    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let source_id = SourceId::new("scale-directory-source").unwrap();
    let root_loaded =
        load_package_directory(packages.join("root"), CompilationLimits::default()).unwrap();
    let mut libraries = Vec::with_capacity(PACKAGE_COUNT - 1);
    for index in 0..(PACKAGE_COUNT - 1) {
        let loaded = load_package_directory(
            packages.join(format!("lib{index:02}")),
            CompilationLimits::default(),
        )
        .unwrap();
        libraries.push((
            loaded,
            NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap(),
        ));
    }
    let mut loaded_directory_packages = std::iter::once(root_loaded.manifest.id.to_string())
        .chain(
            libraries
                .iter()
                .map(|(loaded, _)| loaded.manifest.id.to_string()),
        )
        .collect::<Vec<_>>();
    loaded_directory_packages.sort();

    let mut catalog = PackageCatalog::new();
    catalog
        .insert(PackageLocation {
            source_id: source_id.clone(),
            directory: root_directory.clone(),
            manifest: Arc::clone(&root_loaded.manifest),
        })
        .unwrap();
    for (loaded, directory) in &libraries {
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: directory.clone(),
                manifest: Arc::clone(&loaded.manifest),
            })
            .unwrap();
    }
    let graph = Arc::new(
        catalog
            .resolve(&source_id, &root_directory, CompilationLimits::default())
            .unwrap(),
    );
    let dependency_manifests = libraries
        .iter()
        .map(|(loaded, _)| (loaded.manifest.id.clone(), Arc::clone(&loaded.manifest)))
        .collect::<BTreeMap<_, _>>();
    let dependency_source_sets = libraries
        .iter()
        .map(|(loaded, _)| {
            (
                loaded.manifest.id.clone(),
                Arc::clone(&loaded.production_sources),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let lock = root_loaded
        .lock
        .as_ref()
        .map(Arc::clone)
        .expect("directory scale root has generated nexa.lock");
    lock.verify(&graph).unwrap();
    let lock_bytes = lock.canonical_bytes();
    let fingerprint_input = canonical_package_build_fingerprint_input_with_contract(
        &root_loaded.manifest,
        &root_loaded.production_sources,
        &dependency_manifests,
        &dependency_source_sets,
        contract,
        Some(&lock),
    );
    let input = ResolvedBuildInput::new(
        root_loaded.manifest,
        root_loaded.production_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        Some(lock),
        Arc::<[u8]>::from(fingerprint_input.host_contract.clone()),
        canonical_host_contract_source_identity(contract),
        fingerprint_input.host_required_entrypoints.clone(),
        nexa_analysis::CompilationOptions::default(),
        fingerprint_input,
    )
    .unwrap();
    ScaleFixture {
        input,
        lock_bytes,
        loaded_directory_packages,
    }
}

struct TempScenarioFixtures {
    _root: TempScaleRoot,
    valid: ScaleFixture,
    invalid: ScaleFixture,
    root_digest: String,
    loaded_package_ids: Vec<String>,
}

fn temp_scenario_fixtures(label: &str, contract: &HostContractInput<'_>) -> TempScenarioFixtures {
    let root = TempScaleRoot::new(label);
    write_scale_directory_tree(&root.path);
    let valid = load_scale_directory_fixture(&root.path, contract);
    let invalid_source = root.path.join("packages/root/src/scale/base0.nexa");
    fs::write(&invalid_source, module_source("scale.base0", true)).unwrap();
    let invalid = load_scale_directory_fixture(&root.path, contract);
    fs::write(&invalid_source, module_source("scale.base0", false)).unwrap();
    let canonical_root = fs::canonicalize(&root.path).unwrap();
    let root_digest = digest(
        "nexa.m4.facade.filesystem-root",
        canonical_root.to_string_lossy().as_bytes(),
    );
    let loaded_package_ids = valid.loaded_directory_packages.clone();
    TempScenarioFixtures {
        _root: root,
        valid,
        invalid,
        root_digest,
        loaded_package_ids,
    }
}

fn permutation(multiplier: usize, count: usize) -> Vec<usize> {
    (0..count)
        .map(|index| (index * multiplier) % count)
        .collect()
}

fn digest(domain: &str, bytes: &[u8]) -> String {
    let mut builder = FingerprintBuilder::new(domain, 1);
    builder.field_bytes("bytes", bytes);
    hex(builder.finish_bytes())
}

fn closure_identity(artifact: &nexa::CompiledPackageArtifact, lock: &[u8]) -> String {
    let mut builder = FingerprintBuilder::new("nexa.m4-scale.closure-identity", 1);
    builder.field_bytes("source-set", artifact.source_set_fingerprint.as_bytes());
    builder.field_bytes("public-api", artifact.public_api_fingerprint.as_bytes());
    builder.field_bytes("state-schema", artifact.state_schema_fingerprint.as_bytes());
    builder.field_bytes("build", artifact.build_fingerprint.as_bytes());
    builder.field_bytes("lock", lock);
    hex(builder.finish_bytes())
}

fn hex(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

fn scenario_order(name: &str, count: usize) -> (Vec<usize>, bool, &'static str) {
    match name {
        "reverse" => ((0..count).rev().collect(), true, "scale-source"),
        "random_seed_1" => (permutation(37, count), false, "scale-source"),
        "random_seed_2" => (permutation(53, count), true, "scale-source"),
        "temp_root_a" | "temp_root_b" | "worker_order_a" | "worker_order_b" | "forward"
        | "cold_cache" | "hot_cache" => ((0..count).collect(), false, "scale-source"),
        other => panic!("unknown scenario {other}"),
    }
}

fn compiled_closure_ids(
    artifact: &nexa::CompiledPackageArtifact,
    fixture: &ScaleFixture,
) -> (Vec<String>, Vec<String>) {
    let package_ids = artifact
        .source_files
        .files()
        .iter()
        .filter(|source| !source.compiler_provided())
        .filter_map(|source| source.key())
        .map(|key| key.package_id.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let expected_package_ids = fixture
        .input
        .dependency_graph
        .packages
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(package_ids, expected_package_ids);

    let module_ids = artifact
        .debug_inspection()
        .modules
        .iter()
        .filter(|module| module.package_id != nexa_stdlib::PACKAGE_ID)
        .map(|module| format!("{}::{}", module.package_id, module.module_path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let expected_module_ids = fixture
        .input
        .all_source_sets()
        .flat_map(PackageSourceSet::production_units)
        .map(|unit| {
            format!(
                "{}::{}",
                unit.key.package_id,
                unit.expected_module_path().unwrap()
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    assert_eq!(module_ids, expected_module_ids);
    (package_ids, module_ids)
}

fn scale_counters(
    check: &nexa::PackageCheckReport,
    artifact: &nexa::CompiledPackageArtifact,
) -> ScaleCounters {
    let compilation = artifact.compilation_evidence;
    assert_eq!(
        compilation.import_edges,
        check.resolved_module_imports + check.resolved_dependency_imports
    );
    ScaleCounters {
        modules: u64::try_from(compilation.modules).unwrap(),
        symbols: u64::try_from(compilation.symbols).unwrap(),
        package_modules: u64::try_from(compilation.package_modules).unwrap(),
        package_symbols: u64::try_from(compilation.package_symbols).unwrap(),
        import_edges: u64::try_from(compilation.import_edges).unwrap(),
        packages: u64::try_from(compilation.packages).unwrap(),
    }
}

fn compile_scenario(
    session: &mut PackageBuildSession,
    fixture: &ScaleFixture,
    contract: &HostContractInput<'_>,
    diagnostic_digest: &str,
    diagnostic_records: usize,
    pipeline: &mut PipelineCounters,
) -> (ScenarioEvidence, ScaleCounters, QueryRunEvidence) {
    let check_pipeline_before = session.pipeline_stats();
    let check = session
        .check_package_with_contract(&fixture.input, contract)
        .unwrap();
    assert!(check.diagnostics.is_empty());
    pipeline.record_facade_stats(
        session
            .pipeline_stats()
            .checked_delta(check_pipeline_before)
            .expect("session pipeline counters are monotonic"),
    );
    let query_evidence = QueryRunEvidence::from_check(&check);
    let identity = CandidateIdentity::new(
        fixture.input.root_manifest.id.clone(),
        1,
        fixture.input.build_fingerprint,
    )
    .unwrap();
    let observation =
        session.compile_package_with_contract_observed(&fixture.input, contract, identity);
    pipeline.record_facade_stats(observation.pipeline);
    let artifact = observation.result.unwrap();
    artifact.verify_integrity().unwrap();
    assert_eq!(
        check.compilation_evidence, artifact.compilation_evidence,
        "check and compiled artifact observed different canonical closure cardinalities"
    );
    let bytes = artifact.encode_module();
    assert!(
        !bytes.is_empty(),
        "canonical scale artifact must encode bytes"
    );
    pipeline.record_encoded_module(bytes.len());
    let (compiled_package_ids, compiled_module_ids) = compiled_closure_ids(&artifact, fixture);
    let scale = scale_counters(&check, &artifact);
    let closure_identity = closure_identity(&artifact, &fixture.lock_bytes);
    (
        ScenarioEvidence {
            artifact_bytes_digest: digest("nexa.m4.facade.artifact", &bytes),
            diagnostic_ndjson_digest: diagnostic_digest.to_owned(),
            diagnostic_records: u64::try_from(diagnostic_records).unwrap(),
            source_fingerprint: artifact.source_set_fingerprint.to_string(),
            public_api_fingerprint: artifact.public_api_fingerprint.to_string(),
            state_schema_fingerprint: artifact.state_schema_fingerprint.to_string(),
            build_fingerprint: artifact.build_fingerprint.to_string(),
            linked_state_fingerprint: artifact.linked_state_fingerprint.to_string(),
            closure_identity,
            lock_digest: digest("nexa.m4.facade.lock", &fixture.lock_bytes),
            compiled_package_ids,
            compiled_module_ids,
            mechanism: "direct-session",
            filesystem_root_digest: None,
            loaded_package_directories: 0,
            loaded_package_ids: Vec::new(),
            worker_completion_order: Vec::new(),
            max_in_flight: 1,
        },
        scale,
        query_evidence,
    )
}

fn invalid_diagnostic_evidence(
    fixture: &ScaleFixture,
    contract: &HostContractInput<'_>,
    pipeline: &mut PipelineCounters,
) -> (String, usize) {
    let mut session = PackageBuildSession::new();
    let before = session.pipeline_stats();
    let error = session
        .check_package_with_contract(&fixture.input, contract)
        .unwrap_err();
    pipeline.record_facade_stats(
        session
            .pipeline_stats()
            .checked_delta(before)
            .expect("session pipeline counters are monotonic"),
    );
    let PackageBuildError::AnalysisFailed(batch) = error else {
        panic!("invalid twin did not fail through canonical analysis: {error}");
    };
    let diagnostic_count = batch.len();
    assert!(diagnostic_count > 0);
    let ndjson = LeafDiagnosticRenderer::ndjson(&batch).unwrap();
    let records = ndjson.lines().filter(|line| !line.is_empty()).count();
    // Canonical NDJSON has one batch header followed by one record per diagnostic.
    assert_eq!(records, diagnostic_count + 1);
    (
        digest("nexa.m4.facade.diagnostic-ndjson", ndjson.as_bytes()),
        records,
    )
}

type WorkerBuildResult = (
    usize,
    ScenarioEvidence,
    ScaleCounters,
    QueryRunEvidence,
    PipelineCounters,
);

fn compile_worker_schedule(
    schedule: [usize; WORKER_COUNT],
    contract_source: &str,
    diagnostic_digest: &str,
    diagnostic_records: usize,
    pipeline: &mut PipelineCounters,
) -> (ScenarioEvidence, ScaleCounters, QueryRunEvidence) {
    let count = BASE_MODULES + WORK_MODULES;
    let (completed_tx, completed_rx) = mpsc::channel::<WorkerBuildResult>();
    let (ready_tx, ready_rx) = mpsc::channel::<usize>();
    let rendezvous = Arc::new(Barrier::new(WORKER_COUNT + 1));
    let in_flight = Arc::new(AtomicU64::new(0));
    let max_in_flight = Arc::new(AtomicU64::new(0));
    let mut start_senders = Vec::with_capacity(WORKER_COUNT);
    let mut finish_senders = Vec::with_capacity(WORKER_COUNT);
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for worker in 0..WORKER_COUNT {
        let (start_tx, start_rx) = mpsc::channel::<()>();
        let (finish_tx, finish_rx) = mpsc::channel::<()>();
        start_senders.push(start_tx);
        finish_senders.push(finish_tx);
        let completed_tx = completed_tx.clone();
        let ready_tx = ready_tx.clone();
        let rendezvous = Arc::clone(&rendezvous);
        let in_flight = Arc::clone(&in_flight);
        let max_in_flight = Arc::clone(&max_in_flight);
        let parsed_contract = nexa::parse_contract(contract_source).unwrap();
        let contract = scale_host_contract(&parsed_contract, contract_source);
        let fixture = scale_fixture(
            &(0..count).collect::<Vec<_>>(),
            false,
            "scale-source",
            false,
            &contract,
        );
        let contract_source = contract_source.to_owned();
        let diagnostic_digest = diagnostic_digest.to_owned();
        handles.push(thread::spawn(move || {
            start_rx.recv().unwrap();
            let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            max_in_flight.fetch_max(active, Ordering::SeqCst);
            rendezvous.wait();
            let parsed_contract = nexa::parse_contract(&contract_source).unwrap();
            let contract = scale_host_contract(&parsed_contract, &contract_source);
            let mut local_pipeline = PipelineCounters::default();
            let (evidence, scale, query) = compile_scenario(
                &mut PackageBuildSession::new(),
                &fixture,
                &contract,
                &diagnostic_digest,
                diagnostic_records,
                &mut local_pipeline,
            );
            ready_tx.send(worker).unwrap();
            finish_rx.recv().unwrap();
            in_flight.fetch_sub(1, Ordering::SeqCst);
            completed_tx
                .send((worker, evidence, scale, query, local_pipeline))
                .unwrap();
        }));
    }
    drop(completed_tx);
    drop(ready_tx);

    for sender in &start_senders {
        sender.send(()).unwrap();
    }
    rendezvous.wait();
    let ready = (0..WORKER_COUNT)
        .map(|_| ready_rx.recv().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(ready.len(), WORKER_COUNT);
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        u64::try_from(WORKER_COUNT).unwrap()
    );
    let mut observed_order = Vec::with_capacity(WORKER_COUNT);
    let mut results = BTreeMap::new();
    for expected_worker in schedule {
        finish_senders[expected_worker].send(()).unwrap();
        let (worker, evidence, scale, query, local_pipeline) = completed_rx.recv().unwrap();
        assert_eq!(
            worker, expected_worker,
            "worker completed without its explicit dispatch permit"
        );
        observed_order.push(u64::try_from(worker).unwrap());
        pipeline.merge(local_pipeline);
        results.insert(worker, (evidence, scale, query));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let (mut representative, scale, query) = results.remove(&0).unwrap();
    assert!(
        results
            .values()
            .all(|(evidence, _, _)| evidence.deterministic_payload_eq(&representative)),
        "identical worker builds produced different façade evidence"
    );
    representative.mechanism = "thread-dispatch-and-completion";
    representative.worker_completion_order = observed_order;
    representative.max_in_flight = max_in_flight.load(Ordering::SeqCst);
    assert!(
        representative.max_in_flight >= 2,
        "worker schedule never had two canonical builds in flight"
    );
    (representative, scale, query)
}

#[test]
#[ignore = "run by cargo xtask m4-scale-stress"]
#[allow(clippy::too_many_lines)]
fn facade_scale_determinism_report() {
    let parsed_contract = nexa::parse_contract(SCALE_HOST_NIDL).unwrap();
    let contract = scale_host_contract(&parsed_contract, SCALE_HOST_NIDL);
    let mut pipeline = PipelineCounters::default();
    let count = BASE_MODULES + WORK_MODULES;
    let mut scenarios = BTreeMap::new();
    let mut baseline_scale: Option<ScaleCounters> = None;
    let mut cache_session = PackageBuildSession::new();
    let mut cold_query = None;
    let mut hot_query = None;
    let mut diagnostic_records = 0_u64;

    for name in SCENARIOS {
        let (order, reverse_packages, source_name) = scenario_order(name, count);
        let (evidence, scale, query_evidence, scenario_diagnostic_records) = match name {
            "temp_root_a" | "temp_root_b" => {
                let fixtures = temp_scenario_fixtures(name, &contract);
                let (diagnostic_digest, records) =
                    invalid_diagnostic_evidence(&fixtures.invalid, &contract, &mut pipeline);
                let (mut evidence, scale, query) = compile_scenario(
                    &mut PackageBuildSession::new(),
                    &fixtures.valid,
                    &contract,
                    &diagnostic_digest,
                    records,
                    &mut pipeline,
                );
                evidence.mechanism = "filesystem-directory-loader";
                evidence.filesystem_root_digest = Some(fixtures.root_digest.clone());
                evidence.loaded_package_directories =
                    u64::try_from(fixtures.loaded_package_ids.len()).unwrap();
                evidence.loaded_package_ids = fixtures.loaded_package_ids;
                (evidence, scale, query, records)
            }
            "worker_order_a" | "worker_order_b" => {
                let invalid_fixture =
                    scale_fixture(&order, reverse_packages, source_name, true, &contract);
                let (diagnostic_digest, records) =
                    invalid_diagnostic_evidence(&invalid_fixture, &contract, &mut pipeline);
                let schedule = if name == "worker_order_a" {
                    [0, 1, 2, 3]
                } else {
                    [3, 2, 1, 0]
                };
                let (evidence, scale, query) = compile_worker_schedule(
                    schedule,
                    SCALE_HOST_NIDL,
                    &diagnostic_digest,
                    records,
                    &mut pipeline,
                );
                (evidence, scale, query, records)
            }
            _ => {
                let invalid_fixture =
                    scale_fixture(&order, reverse_packages, source_name, true, &contract);
                let (diagnostic_digest, records) =
                    invalid_diagnostic_evidence(&invalid_fixture, &contract, &mut pipeline);
                let fixture =
                    scale_fixture(&order, reverse_packages, source_name, false, &contract);
                let (mut evidence, scale, query) = if matches!(name, "cold_cache" | "hot_cache") {
                    compile_scenario(
                        &mut cache_session,
                        &fixture,
                        &contract,
                        &diagnostic_digest,
                        records,
                        &mut pipeline,
                    )
                } else {
                    compile_scenario(
                        &mut PackageBuildSession::new(),
                        &fixture,
                        &contract,
                        &diagnostic_digest,
                        records,
                        &mut pipeline,
                    )
                };
                if matches!(name, "cold_cache" | "hot_cache") {
                    evidence.mechanism = "persistent-query-database";
                }
                (evidence, scale, query, records)
            }
        };
        diagnostic_records = diagnostic_records
            .checked_add(u64::try_from(scenario_diagnostic_records).unwrap())
            .expect("scale diagnostic record count fits u64");
        match name {
            "cold_cache" => cold_query = Some(query_evidence),
            "hot_cache" => hot_query = Some(query_evidence),
            _ => {}
        }
        if let Some(baseline) = baseline_scale {
            assert_eq!(scale.modules, baseline.modules);
            assert_eq!(scale.symbols, baseline.symbols);
            assert_eq!(scale.package_modules, baseline.package_modules);
            assert_eq!(scale.package_symbols, baseline.package_symbols);
            assert_eq!(scale.import_edges, baseline.import_edges);
            assert_eq!(scale.packages, baseline.packages);
        } else {
            baseline_scale = Some(scale);
        }
        scenarios.insert(name.to_owned(), evidence);
    }

    let forward = scenarios.get("forward").unwrap().clone();
    assert!(
        scenarios
            .values()
            .all(|scenario| scenario.deterministic_payload_eq(&forward)),
        "compiled façade scenarios are not byte/fingerprint/diagnostic identical"
    );
    let temp_a = scenarios.get("temp_root_a").unwrap();
    let temp_b = scenarios.get("temp_root_b").unwrap();
    assert_eq!(temp_a.mechanism, "filesystem-directory-loader");
    assert_eq!(temp_b.mechanism, "filesystem-directory-loader");
    assert_eq!(
        temp_a.loaded_package_directories,
        u64::try_from(PACKAGE_COUNT).unwrap()
    );
    assert_eq!(
        temp_b.loaded_package_directories,
        u64::try_from(PACKAGE_COUNT).unwrap()
    );
    assert_eq!(temp_a.loaded_package_ids, temp_a.compiled_package_ids);
    assert_eq!(temp_b.loaded_package_ids, temp_b.compiled_package_ids);
    assert_ne!(
        temp_a.filesystem_root_digest, temp_b.filesystem_root_digest,
        "temporary-root scenarios did not use distinct canonical directories"
    );
    let worker_a = scenarios.get("worker_order_a").unwrap();
    let worker_b = scenarios.get("worker_order_b").unwrap();
    assert_eq!(worker_a.mechanism, "thread-dispatch-and-completion");
    assert_eq!(worker_b.mechanism, "thread-dispatch-and-completion");
    assert_eq!(
        worker_a.worker_completion_order.len(),
        WORKER_COUNT,
        "worker-order A omitted actual completion events"
    );
    assert_eq!(
        worker_b.worker_completion_order.len(),
        WORKER_COUNT,
        "worker-order B omitted actual completion events"
    );
    assert_ne!(
        worker_a.worker_completion_order, worker_b.worker_completion_order,
        "worker schedules produced the same completion order"
    );
    assert!(worker_a.max_in_flight >= 2);
    assert!(worker_b.max_in_flight >= 2);
    let scale = baseline_scale.unwrap();
    assert!(scale.modules >= 100);
    assert!(scale.symbols >= 1_000);
    assert!(scale.package_modules >= 100);
    assert!(scale.package_symbols >= 1_000);
    assert!(scale.import_edges >= 500);
    assert!(scale.packages >= 20);
    assert_eq!(pipeline.analyzer_runs, 42);
    assert_eq!(pipeline.invalid_analyzer_runs, 10);
    assert_eq!(pipeline.successful_check_analyzer_runs, 16);
    assert_eq!(pipeline.compile_analyzer_runs, 16);
    assert_eq!(pipeline.typed_compiler_runs, 16);
    assert_eq!(pipeline.verifier_runs, 16);
    assert_eq!(pipeline.module_encode_runs, 16);
    assert!(pipeline.module_bytes_length > 0);
    let cold = cold_query.expect("cold-cache query evidence was recorded");
    let hot = hot_query.expect("hot-cache query evidence was recorded");
    assert!(
        cold.parsed_sources > 0 && cold.analyzed_modules > 0,
        "cold check must parse sources and analyze modules: {cold:?}"
    );
    assert_eq!(
        hot.parsed_sources, 0,
        "hot check reparsed unchanged sources: {hot:?}"
    );
    assert_eq!(
        hot.analyzed_modules, 0,
        "hot check reanalyzed unchanged modules: {hot:?}"
    );
    assert!(
        hot.reused_queries > 0 && hot.cumulative_hits > cold.cumulative_hits,
        "hot check did not prove persistent QueryDatabase reuse: cold={cold:?}, hot={hot:?}"
    );

    let closure_identity = forward.closure_identity;
    let report = FacadeScaleReport {
        schema: 3,
        status: "PASS",
        closure_identity,
        scale,
        pipeline,
        diagnostics: DiagnosticEvidence {
            format: "canonical-ndjson",
            scenario_runs: u64::try_from(SCENARIOS.len()).unwrap(),
            records: diagnostic_records,
        },
        query_cache: QueryCacheEvidence { cold, hot },
        scenarios,
    };
    if let Some(path) = std::env::var_os("NEXA_M4_FACADE_SCALE_REPORT") {
        let path = PathBuf::from(path);
        fs::create_dir_all(path.parent().expect("report has parent")).unwrap();
        let mut bytes = serde_json::to_vec_pretty(&report).unwrap();
        bytes.push(b'\n');
        fs::write(path, bytes).unwrap();
    }
}
