//! WP98 cold-start authority.
//!
//! These cases are deliberately separate from the frozen 7x1000 hot-path
//! comparison. Every measured sample constructs a cold user-facing pipeline
//! (or an explicitly warm cache lookup), while one unrecorded sample absorbs
//! process-level page faults and allocator initialization.

use std::collections::{BTreeMap, BTreeSet};
use std::hint::black_box;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use nexa::prelude::{
    HostCallOutcome, HostFunctionSlot, HostRegistry, HostTrap, ResolvedHostFunction,
    ResourceContext, RestartReloadOutcome, RestartReloadPolicy, RuntimeHostArgs,
};
use nexa::{
    BuildProfile, HostContractInput, PackageBuildSession, ReplCellOutcome, ReplConsoleEmission,
    ReplConsoleHost, ReplConsoleHostError, ReplResolvedCellInput, ReplSession, ReplSessionLimits,
    SourceIdentity,
};
use nexa_analysis::{
    CandidateIdentity, CompilationLimits, ModulePath, NormalizedPackagePath,
    PackageId as AnalysisPackageId, PackageManifest, ReplCellInput, ResolvedBuildInput,
    ResolvedDependencyGraph, ResolvedPackage, SourceId as AnalysisSourceId, SourceKey, SourceRole,
    SourceSetBuilder,
};
use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, HostContract, MemoryPackage, MemorySource,
    NexaEngine, PackagePolicy, PackageRuntimeLimits, SourceId as EngineSourceId, TrustLevel,
};
use nexa_runtime::{ExecutableModule, OpcodeCostTable};
use serde::Serialize;

use super::{CaseStats, Observation, PeakResources};

const WARMUP: usize = 1;
const CASE_NAMES: [&str; 8] = [
    "standalone_single_file",
    "standalone_package",
    "engine_first_discover_enable",
    "artifact_cache_warm",
    "artifact_cache_cold",
    "repl_first_cell",
    "repl_subsequent_cell",
    "reload",
];

const ENGINE_CONTRACT_SOURCE: &str = "contract BenchmarkColdStartHost {}\n";
const STANDALONE_SCRIPT: &str = "40 + 2;\n";
const STANDALONE_PACKAGE_SOURCE: &str =
    "fn main(args: Array<string>) -> i32 { return args.len(); }\n";

#[derive(Debug, Serialize)]
struct ColdStartReport {
    schema: u32,
    benchmark_version: u32,
    report: &'static str,
    implementation_commit: String,
    benchmark_source_hash: String,
    toolchain: String,
    os: &'static str,
    os_version: String,
    arch: &'static str,
    machine_model: String,
    cpu_model: String,
    logical_cpu_count: usize,
    power_source: String,
    thermal_policy: String,
    build_profile: &'static str,
    samples: usize,
    warmup: usize,
    started_at_unix_ms: u128,
    protocol: &'static str,
    measurement_boundaries: BTreeMap<&'static str, &'static str>,
    cases: Vec<CaseStats>,
    status: &'static str,
}

struct ResolvedFixture {
    input: ResolvedBuildInput,
    identity: CandidateIdentity,
    source_key: SourceKey,
}

#[derive(Default)]
struct NullReplConsole;

impl ReplConsoleHost for NullReplConsole {
    fn prepare_cell(&mut self, _: &[ReplConsoleEmission]) -> Result<(), ReplConsoleHostError> {
        Ok(())
    }

    fn commit_prepared_cell(&mut self) {}

    fn discard_prepared_cell(&mut self) {}
}

struct EmptyRegistry(nexa::StableId);

impl HostRegistry for EmptyRegistry {
    fn contract_runtime_id(&self) -> Option<nexa::StableId> {
        Some(self.0)
    }

    fn resolve_function(&self, _: nexa::StableId) -> Option<ResolvedHostFunction<'_>> {
        None
    }

    fn call_runtime(
        &mut self,
        slot: HostFunctionSlot,
        _: &mut ResourceContext<'_>,
        _: RuntimeHostArgs<'_>,
    ) -> Result<HostCallOutcome, HostTrap> {
        Err(HostTrap::InvalidFunctionSlot(slot))
    }
}

pub(super) fn run(samples: usize, output: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let started_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let mut cases = Vec::with_capacity(CASE_NAMES.len());

    cases.push(super::bench_with_warmup(
        "standalone_single_file",
        "product",
        samples,
        WARMUP,
        || (),
        |()| {
            compile_standalone(
                "nexa.snippet",
                "Nexa Standalone Snippet",
                "main",
                STANDALONE_SCRIPT,
                BuildProfile::StandaloneScript,
                1,
            );
            Observation::default()
        },
    ));
    cases.push(super::bench_with_warmup(
        "standalone_package",
        "product",
        samples,
        WARMUP,
        || (),
        |()| {
            compile_standalone(
                "bench.standalone",
                "Benchmark Standalone Package",
                "main",
                STANDALONE_PACKAGE_SOURCE,
                BuildProfile::StandalonePackage,
                1,
            );
            Observation::default()
        },
    ));
    cases.push(super::bench_with_warmup(
        "engine_first_discover_enable",
        "subsystem",
        samples,
        WARMUP,
        || (),
        |()| {
            run_engine_first_discover_enable();
            Observation::default()
        },
    ));

    let cache_root =
        std::env::temp_dir().join(format!("nexa-benchmark-v7-wp98-{}", std::process::id()));
    let warm_root = cache_root.join("warm");
    let cold_root = cache_root.join("cold");
    let _ = std::fs::remove_dir_all(&cache_root);
    let warm_cache = nexa_compiler::cache::ArtifactCache::new(&warm_root, u64::MAX)?;
    warm_cache.compile(super::LANGUAGE_SOURCE_BASE)?;
    cases.push(super::bench_with_warmup(
        "artifact_cache_warm",
        "subsystem",
        samples,
        WARMUP,
        || (),
        |()| {
            let artifact = warm_cache
                .compile(super::LANGUAGE_SOURCE_BASE)
                .expect("warm artifact-cache lookup");
            assert!(warm_cache.stats().hits > 0);
            black_box(artifact.module().functions.len());
            Observation::default()
        },
    ));
    drop(warm_cache);
    cases.push(super::bench_with_warmup(
        "artifact_cache_cold",
        "subsystem",
        samples,
        WARMUP,
        || {
            let _ = std::fs::remove_dir_all(&cold_root);
        },
        |()| {
            let cache = nexa_compiler::cache::ArtifactCache::new(&cold_root, u64::MAX)
                .expect("cold artifact cache");
            let artifact = cache
                .compile(super::LANGUAGE_SOURCE_BASE)
                .expect("cold artifact-cache compile");
            assert_eq!(cache.stats().hits, 0);
            assert!(cache.stats().misses > 0);
            black_box(artifact.module().functions.len());
            Observation::default()
        },
    ));
    let _ = std::fs::remove_dir_all(&cache_root);

    cases.push(super::bench_with_warmup(
        "repl_first_cell",
        "product",
        samples,
        WARMUP,
        || (),
        |()| {
            let mut session = new_repl_session();
            let outcome = submit_repl_cell(&mut session, 1, "1 + 2\n");
            assert_eq!(outcome.rendered_value.as_deref(), Some("3"));
            Observation::default()
        },
    ));
    cases.push(super::bench_with_warmup(
        "repl_subsequent_cell",
        "product",
        samples,
        WARMUP,
        || {
            let mut session = new_repl_session();
            let first = submit_repl_cell(&mut session, 1, "let base = 40;\n");
            assert!(first.rendered_value.is_none());
            session
        },
        |mut session| {
            let outcome = submit_repl_cell(&mut session, 2, "base + 2\n");
            assert_eq!(outcome.rendered_value.as_deref(), Some("42"));
            Observation::default()
        },
    ));

    let migration = super::migration_inputs()?;
    let old_module = migration.old_module;
    let new_module = migration.new_module;
    cases.push(super::bench_with_warmup(
        "reload",
        "subsystem",
        samples,
        WARMUP,
        || super::prepared_reload(&old_module, &new_module),
        |mut prepared| {
            let before = PeakResources::from_ledger(prepared.realm.resource_ledger());
            let outcome = prepared
                .realm
                .restart_reload(
                    prepared.old,
                    prepared.candidate,
                    RestartReloadPolicy::default(),
                )
                .expect("WP98 reload");
            assert!(matches!(outcome, RestartReloadOutcome::Committed(_)));
            let after = prepared.realm.resource_ledger();
            let mut resources = before;
            resources.merge(PeakResources::from_ledger(after));
            Observation {
                fuel: 1,
                instructions: 1,
                heap_slots: after.heap_objects,
                live_vm_bytes: Some(prepared.realm.live_vm_bytes()),
                vm: Some(prepared.realm.vm_allocation_counters()),
                resources,
                ..Observation::default()
            }
        },
    ));

    let actual = cases.iter().map(|case| case.case).collect::<Vec<_>>();
    assert_eq!(actual, CASE_NAMES, "WP98 case inventory changed");
    let report = ColdStartReport {
        schema: 1,
        benchmark_version: 7,
        report: "Nexa M5 WP98 Cold Start",
        implementation_commit: super::git_commit(),
        benchmark_source_hash: super::benchmark_source_hash(),
        toolchain: super::rustc_version(),
        os: std::env::consts::OS,
        os_version: super::os_version(),
        arch: std::env::consts::ARCH,
        machine_model: super::machine_model(),
        cpu_model: super::cpu_model(),
        logical_cpu_count: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        power_source: super::power_source(),
        thermal_policy: super::thermal_policy(),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        samples,
        warmup: WARMUP,
        started_at_unix_ms: started_at,
        protocol: "one isolated process; one unrecorded process warmup; every measured cold sample reconstructs the named boundary",
        measurement_boundaries: BTreeMap::from([
            (
                "standalone_single_file",
                "resolved virtual script through typed analysis, compiler, verifier, and executable predecode",
            ),
            (
                "standalone_package",
                "resolved manifest-backed standalone package through typed analysis, compiler, verifier, and executable predecode",
            ),
            (
                "engine_first_discover_enable",
                "new Engine builder, in-memory package discovery, canonical compile, verifier, Realm load, and default enable",
            ),
            (
                "artifact_cache_warm",
                "hash lookup, integrity validation, portable decode, verifier, and artifact return from an existing disk entry",
            ),
            (
                "artifact_cache_cold",
                "new empty disk cache, source compile, verifier, and atomic artifact publication",
            ),
            (
                "repl_first_cell",
                "new REPL seed Realm plus first resolved Cell analysis, compile, verify, execution, and transactional commit",
            ),
            (
                "repl_subsequent_cell",
                "second resolved Cell analysis, compile, verify, execution, and transactional commit in a prepared session",
            ),
            (
                "reload",
                "prepared active Realm through restart reload, migration, activation, and transactional commit",
            ),
        ]),
        cases,
        status: "PASS",
    };
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = output {
        std::fs::write(path, format!("{rendered}\n"))?;
    }
    println!("{rendered}");
    Ok(())
}

fn compile_standalone(
    package_id: &str,
    package_name: &str,
    entry: &str,
    source: &str,
    profile: BuildProfile,
    generation: u64,
) {
    let contract = console_contract_input();
    let fixture = resolved_fixture(
        package_id,
        package_name,
        entry,
        source,
        profile,
        generation,
        None,
        &contract,
    )
    .expect("standalone cold-start fixture");
    let mut session = PackageBuildSession::new();
    let artifact = session
        .compile_standalone_with_contract(&fixture.input, &contract, fixture.identity)
        .expect("standalone cold-start compile");
    let executable =
        ExecutableModule::build(&artifact.package().verified, OpcodeCostTable::canonical())
            .expect("standalone executable predecode");
    black_box(artifact.main_stable_id());
    black_box(executable);
}

fn new_repl_session() -> ReplSession {
    ReplSession::new(
        ReplSessionLimits::default(),
        Box::<NullReplConsole>::default(),
    )
    .expect("WP98 REPL session")
}

fn submit_repl_cell(session: &mut ReplSession, ordinal: u64, source: &str) -> ReplCellOutcome {
    let contract = console_contract_input();
    let display = SourceIdentity::package("nexa.repl", format!("repl::cell_{ordinal}"));
    let source_path = format!("src/__repl/cell_{ordinal:020}.nexa");
    let fixture = resolved_fixture(
        "nexa.repl",
        "Nexa REPL",
        "repl.session",
        source,
        BuildProfile::ReplCell,
        ordinal,
        Some((&source_path, "repl.session")),
        &contract,
    )
    .expect("REPL cold-start fixture");
    let cancelled = AtomicBool::new(false);
    session
        .submit_cell(
            ReplResolvedCellInput {
                build_input: &fixture.input,
                contract: &contract,
                identity: fixture.identity,
                source_key: &fixture.source_key,
                cell: ReplCellInput::new(ordinal, display, Arc::<str>::from(source)),
            },
            &cancelled,
        )
        .expect("WP98 REPL Cell")
}

#[allow(clippy::too_many_arguments)]
fn resolved_fixture(
    package_id: &str,
    package_name: &str,
    entry: &str,
    source: &str,
    profile: BuildProfile,
    generation: u64,
    virtual_source: Option<(&str, &str)>,
    contract: &HostContractInput<'_>,
) -> Result<ResolvedFixture, Box<dyn std::error::Error>> {
    let package_id = AnalysisPackageId::new(package_id)?;
    let manifest_source = format!(
        "schema = 2\n\
         kind = \"application\"\n\
         id = \"{}\"\n\
         name = \"{package_name}\"\n\
         version = \"1.0.0\"\n\
         source_root = \"src\"\n\
         entry = \"{entry}\"\n\
         activation = \"programmatic\"\n",
        package_id.as_str()
    );
    let manifest = Arc::new(PackageManifest::parse(&manifest_source)?);
    let mut sources = SourceSetBuilder::new(package_id.clone(), CompilationLimits::default());
    if let Some((path, module)) = virtual_source {
        sources.add_virtual_snippet(
            NormalizedPackagePath::new(path)?,
            Arc::<str>::from(source),
            ModulePath::new(module)?,
        )?;
    } else if profile == BuildProfile::StandaloneScript {
        sources.add_virtual_snippet(
            NormalizedPackagePath::new("src/main.nexa")?,
            Arc::<str>::from(source),
            ModulePath::new(entry)?,
        )?;
    } else {
        sources.add(
            NormalizedPackagePath::new(format!("src/{}.nexa", entry.replace('.', "/")))?,
            Arc::<str>::from(source),
            SourceRole::Production,
        )?;
    }
    let sources = Arc::new(sources.build()?);
    let source_key = sources
        .production_units()
        .next()
        .expect("cold-start fixture has one source")
        .key
        .clone();
    let graph = Arc::new(ResolvedDependencyGraph {
        root: package_id.clone(),
        packages: BTreeMap::from([(
            package_id.clone(),
            ResolvedPackage {
                id: package_id.clone(),
                version: manifest.version.clone(),
                source_id: AnalysisSourceId::new("benchmark-v7-wp98")?,
                directory: NormalizedPackagePath::new(format!(
                    "packages/{}",
                    package_id.as_str().replace('.', "-")
                ))?,
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let dependency_manifests = BTreeMap::new();
    let dependency_source_sets = BTreeMap::new();
    let fingerprint = nexa::canonical_package_build_fingerprint_input_with_contract_for_profile(
        &manifest,
        &sources,
        &dependency_manifests,
        &dependency_source_sets,
        contract,
        None,
        profile,
    );
    let input = ResolvedBuildInput::new(
        Arc::clone(&manifest),
        sources,
        dependency_manifests,
        dependency_source_sets,
        graph,
        None,
        Arc::<[u8]>::from(fingerprint.host_contract.clone()),
        Arc::<[u8]>::from(fingerprint.host_contract_source.clone()),
        Arc::<[u8]>::from(fingerprint.host_required_entrypoints.clone()),
        profile.compilation_options(),
        fingerprint,
    )?;
    let identity = CandidateIdentity::new(package_id, generation, input.build_fingerprint)?;
    Ok(ResolvedFixture {
        input,
        identity,
        source_key,
    })
}

fn console_contract_input() -> HostContractInput<'static> {
    static MODEL: OnceLock<nexa::ValidatedContract> = OnceLock::new();
    let model = MODEL.get_or_init(|| {
        nexa::parse_nidl(nexa::CONSOLE_HOST_NIDL).expect("built-in Console contract")
    });
    HostContractInput::with_source(
        model,
        SourceIdentity::standalone(nexa::CONSOLE_HOST_SOURCE_IDENTITY),
        Arc::<str>::from(nexa::CONSOLE_HOST_NIDL),
    )
    .expect("exact built-in Console source")
}

fn engine_contract() -> HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    *CONTRACT.get_or_init(|| {
        let model = nexa::parse_nidl(ENGINE_CONTRACT_SOURCE).expect("WP98 Engine Host contract");
        let descriptor = nexa::abi_descriptor(&model);
        let fingerprint = descriptor.fingerprint.into_bytes();
        let descriptor = Box::leak(descriptor.bytes.into_boxed_slice());
        HostContract::new(
            "BenchmarkColdStartHost",
            ENGINE_CONTRACT_SOURCE,
            descriptor,
            fingerprint,
            nexa::contract_runtime_id(&model),
            nexa::prelude::HOST_CONTRACT_SCHEMA_VERSION,
        )
    })
}

fn run_engine_first_discover_enable() {
    let contract = engine_contract();
    let contract_runtime_id = contract.contract_runtime_id();
    let policy = PackagePolicy {
        trust: TrustLevel::Trusted,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::DefaultEnabled]),
        max_packages: 1,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    };
    let manifest = "schema = 2\n\
                    kind = \"application\"\n\
                    id = \"bench.engine\"\n\
                    name = \"Benchmark Engine\"\n\
                    version = \"1.0.0\"\n\
                    source_root = \"src\"\n\
                    entry = \"bench.engine\"\n\
                    activation = \"default-enabled\"\n\
                    handler_fuel = 20000\n\
                    capabilities = []\n";
    let source = MemorySource::new(
        EngineSourceId::new("benchmark-v7-wp98").expect("Engine Source ID"),
        policy,
    )
    .package(
        MemoryPackage::new("bench-engine", manifest)
            .source("src/bench/engine.nexa", "fn ready() -> i32 { return 1; }\n"),
    );
    let mut engine = NexaEngine::builder(contract)
        .host_factory(move |_: &nexa_embed::PackageContext| {
            Box::new(EmptyRegistry(contract_runtime_id)) as Box<dyn HostRegistry>
        })
        .package_source(source)
        .build()
        .expect("WP98 Engine build");
    let discovered = engine.discover().expect("WP98 Engine discover");
    assert_eq!(discovered.len(), 1);
    engine
        .enable_defaults()
        .expect("WP98 Engine enable defaults");
    assert_eq!(engine.health().enabled_packages, 1);
    engine.shutdown().expect("WP98 Engine shutdown");
}

#[cfg(test)]
mod tests {
    use super::{CASE_NAMES, ColdStartReport};

    #[test]
    fn wp98_inventory_names_all_eight_required_surfaces_once() {
        assert_eq!(CASE_NAMES.len(), 8);
        assert_eq!(
            CASE_NAMES,
            [
                "standalone_single_file",
                "standalone_package",
                "engine_first_discover_enable",
                "artifact_cache_warm",
                "artifact_cache_cold",
                "repl_first_cell",
                "repl_subsequent_cell",
                "reload",
            ]
        );
        let _ = std::mem::size_of::<ColdStartReport>();
    }
}
