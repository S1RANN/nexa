use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;

use nexa_analysis::{
    AnalysisEnvironment, AnalysisOutcome, BuildFingerprintInput, CompilationLimits,
    CompilationOptions, FingerprintBuilder, HostContractSurface, LockFile, NormalizedPackagePath,
    PackageCatalog, PackageId, PackageLocation, PackageManifest, PackageSourceSet, QueryDatabase,
    ResolvedBuildInput, SourceId, SourceRole, SourceSetBuilder, analyze_package,
    load_package_directory, load_package_directory_without_lock, source_set_fingerprint,
};
use nexa_core::StableId;
use nexa_diagnostics::DiagnosticRenderer;
use serde::Serialize;

const BASE_MODULES: usize = 5;
const WORK_MODULES: usize = 100;
const SYMBOLS_PER_MODULE: usize = 10;
const PACKAGE_COUNT: usize = 20;
const ROOT_PACKAGE: &str = "scale.application";
const SOURCE_ID: &str = "scale-source";
const ROOT_DIRECTORY: &str = "packages/root";
/// `nexa_idl::canonical(parse("interface ScaleHost {}"))`.
const SCALE_HOST_CONTRACT: &[u8] = b"interface:ScaleHost;";
const SCALE_HOST_SOURCE_PATH: &str = "host-contract.nidl";
const SCALE_HOST_SOURCE: &str = "interface ScaleHost {\n}\n";
const WORKER_COUNT: usize = 4;
static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);

fn scale_host_source_identity() -> Vec<u8> {
    let mut bytes = b"nexa.host-contract-source\0\x01\0\0\0".to_vec();
    bytes.extend_from_slice(
        &u64::try_from(SCALE_HOST_SOURCE_PATH.len())
            .unwrap()
            .to_le_bytes(),
    );
    bytes.extend_from_slice(SCALE_HOST_SOURCE_PATH.as_bytes());
    bytes.extend_from_slice(
        &u64::try_from(SCALE_HOST_SOURCE.len())
            .unwrap()
            .to_le_bytes(),
    );
    bytes.extend_from_slice(SCALE_HOST_SOURCE.as_bytes());
    bytes
}

fn scale_host_required_exports_identity() -> Vec<u8> {
    let mut bytes = b"nexa.host-required-exports\0".to_vec();
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u64.to_le_bytes());
    bytes
}

#[derive(Serialize)]
struct ScaleReport {
    schema: u32,
    status: &'static str,
    scale: ScaleCounts,
    closure_identity: String,
    determinism: DeterminismEvidence,
}

#[derive(Serialize)]
struct ScaleCounts {
    modules: usize,
    symbols: usize,
    import_edges: usize,
    packages: usize,
}

#[derive(Serialize)]
struct DeterminismEvidence {
    fingerprint: Pair,
    lockfile: Pair,
    analysis_graph: Pair,
    analysis_diagnostics: Pair,
    query_cold_hot: Pair,
    hot_cache_hits: u64,
    temporary_root: TemporaryRootEvidence,
    worker_order: WorkerOrderEvidence,
}

#[derive(Serialize)]
struct Pair {
    first: String,
    second: String,
}

#[derive(Serialize)]
struct TemporaryRootEvidence {
    first: String,
    second: String,
    mechanism: &'static str,
    first_root_digest: String,
    second_root_digest: String,
    first_packages: usize,
    second_packages: usize,
    first_modules: usize,
    second_modules: usize,
}

#[derive(Serialize)]
struct WorkerOrderEvidence {
    first: String,
    second: String,
    mechanism: &'static str,
    first_completion_order: Vec<u64>,
    second_completion_order: Vec<u64>,
    builds_per_order: usize,
    first_max_in_flight: u64,
    second_max_in_flight: u64,
}

struct ScaleFixture {
    input: ResolvedBuildInput,
    lock_bytes: Vec<u8>,
}

struct TempScaleRoot {
    path: PathBuf,
}

impl TempScaleRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nexa-m4-analysis-scale-{}-{sequence}-{label}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempScaleRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

type LibraryFixture = (
    Arc<PackageManifest>,
    Arc<PackageSourceSet>,
    NormalizedPackagePath,
);

struct AnalysisRun {
    semantic_fingerprint: String,
    closure_identity: String,
    typed_ir_digest: String,
    hot_typed_ir_digest: String,
    diagnostic_digest: String,
    lock_digest: String,
    module_count: usize,
    symbol_count: usize,
    edge_count: usize,
    package_count: usize,
    cache_hits: u64,
}

fn root_module_names() -> Vec<String> {
    (0..BASE_MODULES)
        .map(|index| format!("scale.base{index}"))
        .chain((0..WORK_MODULES).map(|index| format!("scale.m{index:03}")))
        .collect()
}

fn root_module_source(module: &str) -> String {
    let mut source = format!("module {module};\n");
    if module.contains(".m") {
        for base in 0..BASE_MODULES {
            writeln!(source, "import scale.base{base} as base{base};").unwrap();
        }
    }
    for symbol in 0..SYMBOLS_PER_MODULE {
        writeln!(source, "pub const symbol_{symbol}: i32 = {symbol};").unwrap();
    }
    if module == "scale.base0" {
        writeln!(source, "pub fn boot() -> i32 {{ return symbol_0; }}").unwrap();
    }
    source
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

fn source_set(
    package: &PackageId,
    units: impl IntoIterator<Item = (NormalizedPackagePath, String)>,
) -> Arc<PackageSourceSet> {
    let mut builder = SourceSetBuilder::new(package.clone(), CompilationLimits::default());
    for (path, source) in units {
        builder.add(path, source, SourceRole::Production).unwrap();
    }
    Arc::new(builder.build().unwrap())
}

fn library_fixtures(reverse_packages: bool) -> Vec<LibraryFixture> {
    let mut libraries = (0..(PACKAGE_COUNT - 1))
        .map(|index| {
            let manifest = library_manifest(index);
            let module = format!("scale.lib{index:02}");
            let sources = source_set(
                &manifest.id,
                [(
                    NormalizedPackagePath::new(format!("src/{}.nexa", module.replace('.', "/")))
                        .unwrap(),
                    root_module_source(&module),
                )],
            );
            let directory = NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap();
            (manifest, sources, directory)
        })
        .collect::<Vec<_>>();
    if reverse_packages {
        libraries.reverse();
    }
    libraries
}

fn scale_fixture(module_order: &[usize], reverse_packages: bool) -> ScaleFixture {
    let source_id = SourceId::new(SOURCE_ID).unwrap();
    let root_manifest = root_manifest();
    let module_names = root_module_names();
    let root_sources = source_set(
        &root_manifest.id,
        module_order.iter().map(|index| {
            let module = &module_names[*index];
            (
                NormalizedPackagePath::new(format!("src/{}.nexa", module.replace('.', "/")))
                    .unwrap(),
                root_module_source(module),
            )
        }),
    );

    let libraries = library_fixtures(reverse_packages);

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
    let compilation_options = CompilationOptions::default();
    let fingerprint_input = BuildFingerprintInput {
        root_package: root_manifest.id.clone(),
        root_manifest: root_manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&root_sources),
        dependency_manifests: dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect(),
        dependency_source_sets: dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect(),
        host_contract: SCALE_HOST_CONTRACT.to_vec(),
        host_contract_source: scale_host_source_identity(),
        host_required_exports: scale_host_required_exports_identity(),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION.into(),
        standard_library_version: nexa_stdlib::standard_library().version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: nexa_analysis::canonical_compilation_options(&compilation_options),
        canonical_lock_graph: lock_bytes.clone(),
    };
    let input = ResolvedBuildInput::new(
        root_manifest,
        root_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        Some(lock),
        SCALE_HOST_CONTRACT,
        scale_host_source_identity(),
        fingerprint_input.host_required_exports.clone(),
        compilation_options,
        fingerprint_input,
    )
    .unwrap();
    ScaleFixture { input, lock_bytes }
}

fn analyze(module_order: &[usize], reverse_packages: bool) -> AnalysisRun {
    let fixture = scale_fixture(module_order, reverse_packages);
    analyze_fixture(&fixture)
}

fn analyze_fixture(fixture: &ScaleFixture) -> AnalysisRun {
    let mut database = QueryDatabase::new();
    let environment = scale_environment();
    let cold = analyze_package(&fixture.input, &environment, &mut database);
    assert!(
        cold.ir.is_some(),
        "real scale analysis failed: {:#?}",
        cold.diagnostics.diagnostics()
    );
    let cold_digest = typed_analysis_digest(&cold, &database);
    let cold_diagnostics = digest_bytes(
        "nexa.m4-scale.diagnostics",
        DiagnosticRenderer::ndjson(&cold.diagnostics)
            .unwrap()
            .as_bytes(),
    );
    let hits_before_hot = database.stats().hits;
    let hot = analyze_package(&fixture.input, &environment, &mut database);
    assert!(
        hot.ir.is_some(),
        "hot scale analysis failed: {:#?}",
        hot.diagnostics.diagnostics()
    );
    let hot_digest = typed_analysis_digest(&hot, &database);
    let cache_hits = database.stats().hits.saturating_sub(hits_before_hot);
    assert!(cache_hits > 0, "hot analysis did not reuse any real query");

    let ir = cold.ir.as_ref().unwrap();
    let package_count = ir
        .modules()
        .iter()
        // The canonical standard library is injected into every analysis but is
        // not one of the fixture's twenty independently resolved Packages.
        .filter(|module| module.package_id.as_str() != nexa_stdlib::PACKAGE_ID)
        .map(|module| module.package_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let semantic_fingerprint = digest_semantic_identity(&cold, &fixture.input);
    let closure_identity = digest_closure_identity(&cold, &fixture.input, &fixture.lock_bytes);
    AnalysisRun {
        semantic_fingerprint,
        closure_identity,
        typed_ir_digest: cold_digest,
        hot_typed_ir_digest: hot_digest,
        diagnostic_digest: cold_diagnostics,
        lock_digest: digest_bytes("nexa.m4-scale.lock", &fixture.lock_bytes),
        module_count: ir.modules().len(),
        symbol_count: ir.definitions().len(),
        edge_count: database.resolved_module_imports().len()
            + database.resolved_dependency_imports().len(),
        package_count,
        cache_hits,
    }
}

fn scale_environment() -> AnalysisEnvironment {
    AnalysisEnvironment {
        host: Some(HostContractSurface {
            interface_name: "ScaleHost".into(),
            interface_stable_id: StableId::from_name("ScaleHost"),
            types: Vec::new(),
            functions: Vec::new(),
            required_exports: Vec::new(),
            source: None,
        }),
        ..AnalysisEnvironment::default()
    }
}

fn digest_semantic_identity(outcome: &AnalysisOutcome, input: &ResolvedBuildInput) -> String {
    let mut builder = FingerprintBuilder::new("nexa.m4-scale.semantic-identity", 1);
    builder.field_bytes("source-set", outcome.source_set_fingerprint.as_bytes());
    builder.field_bytes("public-api", outcome.public_api_fingerprint.as_bytes());
    builder.field_bytes("state-schema", outcome.state_schema_fingerprint.as_bytes());
    builder.field_bytes("build", input.build_fingerprint.as_bytes());
    hex(builder.finish_bytes())
}

fn digest_closure_identity(
    outcome: &AnalysisOutcome,
    input: &ResolvedBuildInput,
    lock_bytes: &[u8],
) -> String {
    let mut builder = FingerprintBuilder::new("nexa.m4-scale.closure-identity", 1);
    builder.field_bytes("source-set", outcome.source_set_fingerprint.as_bytes());
    builder.field_bytes("public-api", outcome.public_api_fingerprint.as_bytes());
    builder.field_bytes("state-schema", outcome.state_schema_fingerprint.as_bytes());
    builder.field_bytes("build", input.build_fingerprint.as_bytes());
    builder.field_bytes("lock", lock_bytes);
    hex(builder.finish_bytes())
}

fn typed_analysis_digest(outcome: &AnalysisOutcome, database: &QueryDatabase) -> String {
    let ir = outcome.ir.as_ref().unwrap();
    let mut builder = FingerprintBuilder::new("nexa.m4-scale.typed-analysis", 1);
    builder.field_str("package", ir.package_id().as_str());
    builder.field_u64("module-count", u64::try_from(ir.modules().len()).unwrap());
    for module in ir.modules() {
        builder.field_str("module-package", module.package_id.as_str());
        builder.field_str("module", module.module.as_str());
        builder.field_str("source", module.source.path.as_str());
        builder.field_u64("file-id", u64::from(module.file_id.0));
        builder.field_bytes(
            "declarations",
            format!("{:?}", module.declarations).as_bytes(),
        );
    }
    builder.field_u64(
        "definition-count",
        u64::try_from(ir.definitions().len()).unwrap(),
    );
    for definition in ir.definitions() {
        builder.field_str("identity", &definition.canonical_identity);
        builder.field_str("kind", &format!("{:?}", definition.kind));
        builder.field_str("type", &format!("{:?}", definition.ty));
        builder.field_str("effect", &format!("{:?}", definition.effect));
    }
    for (importer, target) in database.resolved_module_imports() {
        builder.field_str(
            "module-import",
            &format!(
                "{}:{}->{}:{}",
                importer.package_id, importer.module, target.package_id, target.module
            ),
        );
    }
    for (importer, dependency) in database.resolved_dependency_imports() {
        builder.field_str(
            "dependency-import",
            &format!(
                "{}:{}->{}:{}",
                importer.package_id, importer.module, dependency.package_id, dependency.module
            ),
        );
    }
    builder.field_bytes(
        "diagnostics",
        DiagnosticRenderer::ndjson(&outcome.diagnostics)
            .unwrap()
            .as_bytes(),
    );
    hex(builder.finish_bytes())
}

fn digest_bytes(domain: &str, bytes: &[u8]) -> String {
    let mut builder = FingerprintBuilder::new(domain, 1);
    builder.field_bytes("value", bytes);
    hex(builder.finish_bytes())
}

fn write_module_file(package_root: &Path, module: &str) {
    let path = package_root
        .join("src")
        .join(format!("{}.nexa", module.replace('.', "/")));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, root_module_source(module)).unwrap();
}

fn write_scale_directory_tree(root: &Path) {
    let packages = root.join("packages");
    let application = packages.join("root");
    fs::create_dir_all(&application).unwrap();
    fs::write(application.join("package.toml"), root_manifest_source()).unwrap();
    for module in root_module_names() {
        write_module_file(&application, &module);
    }
    for index in 0..(PACKAGE_COUNT - 1) {
        let library = packages.join(format!("lib{index:02}"));
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join("package.toml"), library_manifest_source(index)).unwrap();
        write_module_file(&library, &format!("scale.lib{index:02}"));
    }

    let source_id = SourceId::new("scale-directory-source").unwrap();
    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let mut catalog = PackageCatalog::new();
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
        let loaded = load_package_directory_without_lock(
            packages.join(format!("lib{index:02}")),
            CompilationLimits::default(),
        )
        .unwrap();
        catalog
            .insert(PackageLocation {
                source_id: source_id.clone(),
                directory: NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap(),
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

fn load_scale_directory_fixture(root: &Path) -> ScaleFixture {
    let packages = root.join("packages");
    let source_id = SourceId::new("scale-directory-source").unwrap();
    let root_directory = NormalizedPackagePath::new(ROOT_DIRECTORY).unwrap();
    let root_loaded =
        load_package_directory(packages.join("root"), CompilationLimits::default()).unwrap();
    let mut libraries = Vec::with_capacity(PACKAGE_COUNT - 1);
    for index in 0..(PACKAGE_COUNT - 1) {
        libraries.push((
            load_package_directory(
                packages.join(format!("lib{index:02}")),
                CompilationLimits::default(),
            )
            .unwrap(),
            NormalizedPackagePath::new(format!("packages/lib{index:02}")).unwrap(),
        ));
    }

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
    let compilation_options = CompilationOptions::default();
    let fingerprint_input = BuildFingerprintInput {
        root_package: root_loaded.manifest.id.clone(),
        root_manifest: root_loaded.manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&root_loaded.production_sources),
        dependency_manifests: dependency_manifests
            .iter()
            .map(|(package, manifest)| (package.clone(), manifest.canonical_bytes()))
            .collect(),
        dependency_source_sets: dependency_source_sets
            .iter()
            .map(|(package, sources)| (package.clone(), source_set_fingerprint(sources)))
            .collect(),
        host_contract: SCALE_HOST_CONTRACT.to_vec(),
        host_contract_source: scale_host_source_identity(),
        host_required_exports: scale_host_required_exports_identity(),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION.into(),
        standard_library_version: nexa_stdlib::standard_library().version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.into(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.into(),
        compiler_options: nexa_analysis::canonical_compilation_options(&compilation_options),
        canonical_lock_graph: lock_bytes.clone(),
    };
    let input = ResolvedBuildInput::new(
        root_loaded.manifest,
        root_loaded.production_sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        Some(lock),
        SCALE_HOST_CONTRACT,
        scale_host_source_identity(),
        fingerprint_input.host_required_exports.clone(),
        compilation_options,
        fingerprint_input,
    )
    .unwrap();
    ScaleFixture { input, lock_bytes }
}

fn analyze_under_real_root(label: &str) -> (AnalysisRun, String) {
    let root = TempScaleRoot::new(label);
    write_scale_directory_tree(&root.path);
    let fixture = load_scale_directory_fixture(&root.path);
    let run = analyze_fixture(&fixture);
    let canonical_root = fs::canonicalize(&root.path).unwrap();
    let root_digest = digest_bytes(
        "nexa.m4-scale.filesystem-root",
        canonical_root.to_string_lossy().as_bytes(),
    );
    (run, root_digest)
}

fn analyze_worker_schedule(schedule: [usize; WORKER_COUNT]) -> (AnalysisRun, Vec<u64>, u64) {
    let count = BASE_MODULES + WORK_MODULES;
    let start = Arc::new(Barrier::new(WORKER_COUNT));
    let in_flight = Arc::new(AtomicU64::new(0));
    let max_in_flight = Arc::new(AtomicU64::new(0));
    let (ready_tx, ready_rx) = mpsc::channel::<usize>();
    let (completed_tx, completed_rx) = mpsc::channel::<(usize, AnalysisRun)>();
    let mut finish_senders = Vec::with_capacity(WORKER_COUNT);
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for worker in 0..WORKER_COUNT {
        let (finish_tx, finish_rx) = mpsc::channel::<()>();
        finish_senders.push(finish_tx);
        let start = Arc::clone(&start);
        let in_flight = Arc::clone(&in_flight);
        let max_in_flight = Arc::clone(&max_in_flight);
        let ready_tx = ready_tx.clone();
        let completed_tx = completed_tx.clone();
        handles.push(thread::spawn(move || {
            let current = in_flight.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            max_in_flight.fetch_max(current, Ordering::SeqCst);
            start.wait();
            let run = analyze(&(0..count).collect::<Vec<_>>(), false);
            ready_tx.send(worker).unwrap();
            finish_rx.recv().unwrap();
            completed_tx.send((worker, run)).unwrap();
            in_flight.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    drop(ready_tx);
    drop(completed_tx);

    let ready = (0..WORKER_COUNT)
        .map(|_| ready_rx.recv().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ready,
        (0..WORKER_COUNT).collect(),
        "every concurrent analysis worker must finish real analysis before ordered publication"
    );
    let mut observed_order = Vec::with_capacity(WORKER_COUNT);
    let mut runs = BTreeMap::new();
    for expected_worker in schedule {
        finish_senders[expected_worker].send(()).unwrap();
        let (worker, run) = completed_rx.recv().unwrap();
        assert_eq!(
            worker, expected_worker,
            "analysis worker completed without its explicit dispatch permit"
        );
        observed_order.push(u64::try_from(worker).unwrap());
        runs.insert(worker, run);
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let representative = runs.remove(&0).unwrap();
    assert!(
        runs.values().all(|run| {
            run.semantic_fingerprint == representative.semantic_fingerprint
                && run.closure_identity == representative.closure_identity
                && run.typed_ir_digest == representative.typed_ir_digest
                && run.hot_typed_ir_digest == representative.hot_typed_ir_digest
                && run.diagnostic_digest == representative.diagnostic_digest
                && run.lock_digest == representative.lock_digest
        }),
        "identical worker analyses produced different deterministic payloads"
    );
    (
        representative,
        observed_order,
        max_in_flight.load(Ordering::SeqCst),
    )
}

fn hex(bytes: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

#[test]
fn scale_fixture_uses_the_product_build_fingerprint_authorities() {
    let count = BASE_MODULES + WORK_MODULES;
    let fixture = scale_fixture(&(0..count).collect::<Vec<_>>(), false);
    assert_build_authorities(&fixture.input);
}

fn assert_build_authorities(input: &ResolvedBuildInput) {
    let fingerprint = input.fingerprint_input.as_ref();
    let options = CompilationOptions::default();
    assert_eq!(input.compilation_options, options);
    assert_eq!(fingerprint.host_contract, SCALE_HOST_CONTRACT);
    assert_eq!(
        fingerprint.host_contract_source,
        scale_host_source_identity()
    );
    assert_eq!(
        fingerprint.host_required_exports,
        scale_host_required_exports_identity()
    );
    assert_eq!(
        fingerprint.language_version,
        nexa_analysis::NEXA_LANGUAGE_VERSION
    );
    assert_eq!(
        fingerprint.standard_library_version,
        nexa_stdlib::standard_library().version.to_string()
    );
    assert_eq!(
        fingerprint.standard_library_descriptor,
        nexa_stdlib::canonical_descriptor_identity()
    );
    assert_eq!(
        fingerprint.bytecode_version,
        u32::from(nexa_core::BYTECODE_VERSION)
    );
    assert_eq!(
        fingerprint.compiler_version,
        nexa_core::NEXA_COMPILER_VERSION
    );
    assert_eq!(
        fingerprint.runtime_semantics_version,
        u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION)
    );
    assert_eq!(
        fingerprint.opcode_cost_table_version,
        nexa_core::OPCODE_COST_TABLE_VERSION
    );
    assert_eq!(
        fingerprint.deterministic_math_backend,
        nexa_core::RUNTIME_MATH_BACKEND_ID
    );
    assert_eq!(
        fingerprint.compiler_options,
        nexa_analysis::canonical_compilation_options(&options)
    );
}

#[test]
#[ignore = "run by cargo xtask m4-scale-stress"]
#[allow(clippy::too_many_lines)]
fn m4_scale_stress() {
    let count = BASE_MODULES + WORK_MODULES;
    let authority_fixture = scale_fixture(&(0..count).collect::<Vec<_>>(), false);
    assert_build_authorities(&authority_fixture.input);
    let forward = (0..count).collect::<Vec<_>>();
    let reverse = (0..count).rev().collect::<Vec<_>>();
    let shuffled = (0..count)
        .map(|index| (index * 37) % count)
        .collect::<Vec<_>>();

    let first = analyze(&forward, false);
    let second = analyze(&reverse, true);
    let third = analyze(&shuffled, false);
    assert_eq!(first.semantic_fingerprint, second.semantic_fingerprint);
    assert_eq!(first.semantic_fingerprint, third.semantic_fingerprint);
    assert_eq!(first.closure_identity, second.closure_identity);
    assert_eq!(first.closure_identity, third.closure_identity);
    assert_eq!(first.typed_ir_digest, second.typed_ir_digest);
    assert_eq!(first.typed_ir_digest, third.typed_ir_digest);
    assert_eq!(first.typed_ir_digest, first.hot_typed_ir_digest);
    assert_eq!(second.typed_ir_digest, second.hot_typed_ir_digest);
    assert_eq!(first.diagnostic_digest, second.diagnostic_digest);
    assert_eq!(first.lock_digest, second.lock_digest);

    assert!(first.module_count >= 100);
    assert!(first.symbol_count >= 1_000);
    assert!(first.edge_count >= 500);
    assert_eq!(first.package_count, PACKAGE_COUNT);
    assert!(first.cache_hits > 0);

    let (first_root_run, first_root_digest) = analyze_under_real_root("first-root");
    let (second_root_run, second_root_digest) = analyze_under_real_root("second-root");
    assert_eq!(
        first_root_run.semantic_fingerprint,
        second_root_run.semantic_fingerprint
    );
    assert_eq!(
        first_root_run.typed_ir_digest,
        second_root_run.typed_ir_digest
    );
    assert_eq!(
        first_root_run.diagnostic_digest,
        second_root_run.diagnostic_digest
    );
    assert_eq!(first_root_run.lock_digest, second_root_run.lock_digest);
    assert_eq!(first_root_run.typed_ir_digest, first.typed_ir_digest);
    assert_eq!(first_root_run.closure_identity, first.closure_identity);
    assert_eq!(second_root_run.closure_identity, first.closure_identity);
    assert_eq!(first_root_run.package_count, PACKAGE_COUNT);
    assert_eq!(second_root_run.package_count, PACKAGE_COUNT);
    assert!(first_root_run.edge_count >= 500);
    assert!(second_root_run.edge_count >= 500);
    assert_ne!(
        first_root_digest, second_root_digest,
        "temporary-root evidence did not use distinct canonical directories"
    );

    let (first_worker_run, first_completion_order, first_max_in_flight) =
        analyze_worker_schedule([0, 1, 2, 3]);
    let (second_worker_run, second_completion_order, second_max_in_flight) =
        analyze_worker_schedule([3, 2, 1, 0]);
    assert_eq!(
        first_worker_run.typed_ir_digest,
        second_worker_run.typed_ir_digest
    );
    assert_eq!(first_worker_run.typed_ir_digest, first.typed_ir_digest);
    assert_eq!(first_worker_run.closure_identity, first.closure_identity);
    assert_eq!(second_worker_run.closure_identity, first.closure_identity);
    assert_ne!(
        first_completion_order, second_completion_order,
        "worker analyses did not exercise different completion orders"
    );
    assert!(first_max_in_flight >= 2);
    assert!(second_max_in_flight >= 2);

    let report = ScaleReport {
        schema: 3,
        status: "PASS",
        scale: ScaleCounts {
            modules: first.module_count,
            symbols: first.symbol_count,
            import_edges: first.edge_count,
            packages: first.package_count,
        },
        closure_identity: first.closure_identity.clone(),
        determinism: DeterminismEvidence {
            fingerprint: Pair {
                first: first.semantic_fingerprint.clone(),
                second: second.semantic_fingerprint,
            },
            lockfile: Pair {
                first: first.lock_digest,
                second: second.lock_digest,
            },
            analysis_graph: Pair {
                first: first.typed_ir_digest.clone(),
                second: second.typed_ir_digest.clone(),
            },
            analysis_diagnostics: Pair {
                first: first.diagnostic_digest.clone(),
                second: second.diagnostic_digest,
            },
            query_cold_hot: Pair {
                first: first.typed_ir_digest.clone(),
                second: first.hot_typed_ir_digest,
            },
            hot_cache_hits: first.cache_hits,
            temporary_root: TemporaryRootEvidence {
                first: first_root_run.typed_ir_digest,
                second: second_root_run.typed_ir_digest,
                mechanism: "filesystem-directory-loader-full-closure-analysis",
                first_root_digest,
                second_root_digest,
                first_packages: first_root_run.package_count,
                second_packages: second_root_run.package_count,
                first_modules: first_root_run.module_count,
                second_modules: second_root_run.module_count,
            },
            worker_order: WorkerOrderEvidence {
                first: first_worker_run.typed_ir_digest,
                second: second_worker_run.typed_ir_digest,
                mechanism: "concurrent-thread-analysis-controlled-completion",
                first_completion_order,
                second_completion_order,
                builds_per_order: WORKER_COUNT,
                first_max_in_flight,
                second_max_in_flight,
            },
        },
    };
    if let Some(path) = std::env::var_os("NEXA_M4_SCALE_REPORT") {
        let path = PathBuf::from(path);
        fs::create_dir_all(path.parent().expect("report has parent")).unwrap();
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
