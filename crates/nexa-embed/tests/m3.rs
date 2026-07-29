use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CandidateCancellation, CandidateTerminal, CapabilitySet,
    CompileJob, DevelopmentCompileRequest, DevelopmentCompiler, DevelopmentConfig,
    DiagnosticRenderer, EngineDiagnostic, EngineDiagnosticStage, EnqueueOutcome, ExportRequirement,
    MemorySource, PackageId, PackagePolicy, PackageRuntimeLimits, PackageSource,
    SourceFileRegistry, SourceId, TrustLevel,
};

const IDL: &str = "interface TestHost { export Value() -> i32; }";
const MANIFEST: &str = "schema = 1
id = \"tests.development\"
name = \"Development\"
version = \"1.0.0\"
entry = \"main.nexa\"
activation = \"programmatic\"
handler_fuel = 20000
capabilities = []
";

fn policy() -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::FirstParty,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::Programmatic]),
        max_packages: 1,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    }
}

fn candidate(source: &str) -> nexa_embed::PackageCandidate {
    let source = format!("module tests.development;\nimport test;\n{source}");
    MemorySource::new(SourceId::new("tests").expect("source id"), policy())
        .package(MANIFEST, source)
        .discover()
        .expect("candidate discovery")
        .remove(0)
}

fn candidate_for(package: &str, source: &str) -> nexa_embed::PackageCandidate {
    let manifest = format!(
        "schema = 1
id = \"{package}\"
name = \"{package}\"
version = \"1.0.0\"
entry = \"main.nexa\"
activation = \"programmatic\"
handler_fuel = 20000
capabilities = []
"
    );
    let source = format!("module {package};\nimport test;\n{source}");
    MemorySource::new(
        SourceId::new(format!("source-{}", package.replace('.', "-"))).expect("source id"),
        policy(),
    )
    .package(manifest, source)
    .discover()
    .expect("candidate discovery")
    .remove(0)
}

fn requirement(idl: &nexa_idl::Idl) -> ExportRequirement {
    let export = &idl.exports[0];
    ExportRequirement {
        name: export.name.clone(),
        stable_id: nexa_idl::export_stable_id(idl, export),
        signature: nexa_idl::export_signature(idl, export),
    }
}

fn await_results(
    compiler: &DevelopmentCompiler,
    expected_generation: u64,
) -> Vec<CandidateTerminal> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut results = Vec::new();
    while Instant::now() < deadline {
        results.extend(compiler.poll());
        if results
            .iter()
            .any(|result| result.data().generation == expected_generation)
        {
            return results;
        }
        std::thread::yield_now();
    }
    panic!("development compiler did not return generation {expected_generation}");
}

fn await_compile_started(compiler: &DevelopmentCompiler, expected_generation: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if compiler.poll_events().iter().any(|event| {
            matches!(
                event,
                nexa_embed::WorkerEvent::CompileStarted { generation, .. }
                    if *generation == expected_generation
            )
        }) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("development compiler did not start generation {expected_generation}");
}

#[test]
fn diagnostic_renderer_preserves_utf16_ranges_and_schema() {
    let source = "fn Value() -> i32 {\n    return \"界\";\n}\n";
    let registry =
        SourceFileRegistry::from_files([("main.nexa", source)]).expect("source registry");
    let file = registry.file_id("main.nexa").expect("file id");
    let error = nexa::compile_file(source, file).expect_err("type error");
    let nexa::NexaError::Diagnostic(leaf) = error else {
        panic!("expected compiler diagnostic");
    };
    let diagnostic = EngineDiagnostic::from_leaf(
        None,
        SourceId::new("editor").ok(),
        EngineDiagnosticStage::TypeCheck,
        *leaf,
        Some(&registry),
    );
    let rendered = DiagnosticRenderer::json(&diagnostic).expect("diagnostic JSON");
    let value: serde_json::Value = serde_json::from_str(&rendered).expect("JSON value");
    assert_eq!(value["schema"], 1);
    assert_eq!(value["file"], "main.nexa");
    assert!(value["range"]["start"]["line"].is_u64());
    assert!(DiagnosticRenderer::human(&diagnostic).contains("main.nexa:2:"));
}

#[test]
fn source_registry_is_deterministic_bounded_and_unicode_safe() {
    let left = SourceFileRegistry::from_files([("z.nexa", "fn z() {}"), ("a.nexa", "界x")])
        .expect("left registry");
    let right = SourceFileRegistry::from_files([("a.nexa", "界x"), ("z.nexa", "fn z() {}")])
        .expect("right registry");
    assert_eq!(left.file_id("a.nexa"), right.file_id("a.nexa"));
    assert_eq!(left.file_id("z.nexa"), right.file_id("z.nexa"));
    let file = left.file_by_path("a.nexa").expect("unicode file");
    assert_eq!(file.lsp_position(2).character, 0);
    assert_eq!(file.lsp_position(3).character, 1);
    assert!(SourceFileRegistry::from_files([("../outside.nexa", "")]).is_err());
    assert!(SourceFileRegistry::from_files([("/outside.nexa", "")]).is_err());
}

#[test]
fn dev_loop_only_latest_generation_becomes_ready() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig::default()).expect("worker");
    let package_id = candidate("fn Value() -> i32 { return 1; }").manifest.id;
    let mut terminals = Vec::new();
    for generation in 1..=20 {
        let candidate = candidate(&format!("fn Value() -> i32 {{ return {generation}; }}"));
        match compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("tests").expect("source id"),
            generation,
            candidate,
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        }) {
            EnqueueOutcome::Accepted => {}
            EnqueueOutcome::ReplacedPending { terminal, .. } => terminals.push(terminal),
            EnqueueOutcome::Backpressured { .. } => {
                panic!("same-Package replacement backpressured")
            }
            EnqueueOutcome::Stopping { .. } => panic!("active Worker stopped"),
        }
    }
    terminals.extend(await_results(&compiler, 20));
    assert!(
        terminals.iter().any(|result| {
            result.data().generation == 20 && matches!(result, CandidateTerminal::Compiled { .. })
        }),
        "latest result: {:?}",
        terminals
            .iter()
            .find(|result| result.data().generation == 20)
            .map(CandidateTerminal::kind)
    );
    let latest = terminals
        .iter()
        .find(|terminal| terminal.data().generation == 20)
        .expect("latest terminal");
    let CandidateTerminal::Compiled { compilation, .. } = latest else {
        panic!("latest generation was not compiled");
    };
    assert!(compilation.compile_duration > Duration::ZERO);
    assert!(compilation.verify_duration > Duration::ZERO);
    assert!(terminals.iter().all(|terminal| {
        terminal.data().generation == 20
            || matches!(
                terminal,
                CandidateTerminal::SupersededBeforeCompile(_)
                    | CandidateTerminal::SupersededAfterCompile(_)
            )
    }));
    let _ = compiler.shutdown();
}

#[test]
fn supersession_is_rechecked_while_a_compiled_result_waits_for_capacity() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig {
        result_queue_capacity: 1,
        ..DevelopmentConfig::default()
    })
    .expect("worker");
    let package_id = candidate("fn Value() -> i32 { return 1; }").manifest.id;
    let request = |generation| DevelopmentCompileRequest {
        package_id: package_id.clone(),
        source_id: SourceId::new("tests").expect("source id"),
        generation,
        candidate: candidate(&format!("fn Value() -> i32 {{ return {generation}; }}")),
        idl: idl.clone(),
        required_exports: vec![requirement(&idl)],
    };

    assert!(matches!(
        compiler.submit(request(1)),
        EnqueueOutcome::Accepted
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while compiler.inspection().completed_results != 1 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(compiler.inspection().completed_results, 1);

    assert!(matches!(
        compiler.submit(request(2)),
        EnqueueOutcome::Accepted
    ));
    await_compile_started(&compiler, 2);
    assert!(matches!(
        compiler.submit(request(3)),
        EnqueueOutcome::Accepted
    ));

    let terminals = await_results(&compiler, 3);
    assert!(terminals.iter().any(|terminal| {
        terminal.data().generation == 2
            && matches!(terminal, CandidateTerminal::SupersededAfterCompile(_))
    }));
    assert!(terminals.iter().any(|terminal| {
        terminal.data().generation == 3 && matches!(terminal, CandidateTerminal::Compiled { .. })
    }));
    assert!(compiler.shutdown().is_empty());
}

#[test]
fn stress_100_success_and_failure_candidates_shutdown_cleanly() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig {
        compile_queue_capacity: 4,
        result_queue_capacity: 4,
        ..DevelopmentConfig::default()
    })
    .expect("worker");
    let package_id = candidate("fn Value() -> i32 { return 1; }").manifest.id;
    for generation in 1..=100 {
        let source = format!("fn Value() -> i32 {{ return {generation}; }}");
        assert!(matches!(
            compiler.submit(DevelopmentCompileRequest {
                package_id: package_id.clone(),
                source_id: SourceId::new("tests").expect("source id"),
                generation,
                candidate: candidate(&source),
                idl: idl.clone(),
                required_exports: vec![requirement(&idl)],
            }),
            EnqueueOutcome::Accepted
        ));
        let results = await_results(&compiler, generation);
        assert!(
            results.iter().any(|result| {
                result.data().generation == generation
                    && matches!(result, CandidateTerminal::Compiled { .. })
            }),
            "generation {generation}: {:?}",
            results
                .iter()
                .find(|result| result.data().generation == generation)
                .map(CandidateTerminal::kind)
        );
    }
    for generation in 101..=200 {
        assert!(matches!(
            compiler.submit(DevelopmentCompileRequest {
                package_id: package_id.clone(),
                source_id: SourceId::new("tests").expect("source id"),
                generation,
                candidate: candidate("fn Value() -> i32 { return missing; }"),
                idl: idl.clone(),
                required_exports: vec![requirement(&idl)],
            }),
            EnqueueOutcome::Accepted
        ));
        let results = await_results(&compiler, generation);
        let failed = results
            .iter()
            .find(|result| result.data().generation == generation)
            .expect("failed terminal");
        match failed {
            CandidateTerminal::CompileFailed {
                compile_duration,
                verify_duration,
                ..
            } => {
                assert!(*compile_duration > Duration::ZERO);
                assert_eq!(*verify_duration, Duration::ZERO);
            }
            CandidateTerminal::VerifyFailed {
                compile_duration,
                verify_duration,
                ..
            } => {
                assert!(*compile_duration > Duration::ZERO);
                assert!(*verify_duration > Duration::ZERO);
            }
            other => panic!("generation {generation} had terminal {:?}", other.kind()),
        }
    }
    let _ = compiler.shutdown();
}

fn submit_distinct_packages(
    compiler: &DevelopmentCompiler,
    idl: &nexa_idl::Idl,
    count: usize,
) -> (Vec<CompileJob>, Vec<CandidateTerminal>) {
    let mut backpressured = Vec::new();
    let mut terminals = Vec::new();
    for index in 0..count {
        let package = format!("tests.dev{index}");
        match compiler.submit(DevelopmentCompileRequest {
            package_id: PackageId::new(&package).expect("package id"),
            source_id: SourceId::new(format!("source-{index}")).expect("source id"),
            generation: 1,
            candidate: candidate_for(&package, "fn Value() -> i32 { return 1; }"),
            idl: idl.clone(),
            required_exports: vec![requirement(idl)],
        }) {
            EnqueueOutcome::Accepted => {}
            EnqueueOutcome::Backpressured { job } => backpressured.push(job),
            EnqueueOutcome::ReplacedPending { terminal, .. } => terminals.push(terminal),
            EnqueueOutcome::Stopping { .. } => panic!("active Worker stopped"),
        }
    }
    (backpressured, terminals)
}

fn drain_all_distinct(
    compiler: &DevelopmentCompiler,
    mut backpressured: Vec<CompileJob>,
    mut terminals: Vec<CandidateTerminal>,
    expected: usize,
) -> Vec<CandidateTerminal> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        terminals.extend(compiler.poll());
        let mut still_waiting = Vec::new();
        for job in backpressured {
            match compiler.retry(job) {
                EnqueueOutcome::Accepted => {}
                EnqueueOutcome::Backpressured { job } => still_waiting.push(job),
                EnqueueOutcome::ReplacedPending { terminal, .. } => terminals.push(terminal),
                EnqueueOutcome::Stopping { .. } => panic!("active Worker stopped"),
            }
        }
        backpressured = still_waiting;
        let unique = terminals
            .iter()
            .map(|terminal| terminal.data().package_id.clone())
            .collect::<BTreeSet<_>>();
        if unique.len() == expected && backpressured.is_empty() {
            return terminals;
        }
        std::thread::yield_now();
    }
    panic!(
        "only {} terminals observed with {} Jobs still backpressured",
        terminals.len(),
        backpressured.len()
    );
}

#[test]
fn worker_queue_backpressure_preserves_32_distinct_packages() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig {
        compile_queue_capacity: 4,
        result_queue_capacity: 4,
        ..DevelopmentConfig::default()
    })
    .expect("worker");
    let (backpressured, terminals) = submit_distinct_packages(&compiler, &idl, 32);
    assert!(
        !backpressured.is_empty(),
        "the queue must apply backpressure"
    );
    let terminals = drain_all_distinct(&compiler, backpressured, terminals, 32);
    assert_eq!(
        terminals
            .iter()
            .map(|terminal| terminal.data().package_id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        32
    );
    assert!(
        terminals
            .iter()
            .all(|terminal| matches!(terminal, CandidateTerminal::Compiled { .. }))
    );
    assert!(compiler.inspection().backpressure_count > 0);
    assert!(compiler.shutdown().is_empty());
}

#[test]
fn worker_result_backpressure_never_discards_completed_results() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig {
        compile_queue_capacity: 4,
        result_queue_capacity: 4,
        ..DevelopmentConfig::default()
    })
    .expect("worker");
    let (backpressured, terminals) = submit_distinct_packages(&compiler, &idl, 32);
    let deadline = Instant::now() + Duration::from_secs(10);
    while compiler.inspection().completed_results < 4 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let blocked = compiler.inspection();
    assert_eq!(blocked.completed_results, 4);
    let terminals = drain_all_distinct(&compiler, backpressured, terminals, 32);
    assert_eq!(
        terminals
            .iter()
            .map(|terminal| terminal.data().package_id.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        32
    );
    assert!(compiler.shutdown().is_empty());
}

fn slow_candidate(package: &str) -> nexa_embed::PackageCandidate {
    let mut body = String::from("fn Value() -> i32 {\n");
    for index in 0..8_000 {
        writeln!(&mut body, "let value_{index}: i32 = {index};").expect("write slow source");
    }
    body.push_str("return 1;\n}\n");
    candidate_for(package, &body)
}

fn wait_until_in_flight(compiler: &DevelopmentCompiler, package_id: &PackageId) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if compiler.inspection().in_flight_package.as_ref() == Some(package_id) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("Package never entered the in-flight state");
}

#[test]
fn disabling_an_in_flight_generation_has_one_cancelled_terminal() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig::default()).expect("worker");
    let package_id = PackageId::new("tests.slow-disable").expect("package id");
    assert!(matches!(
        compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("slow-disable").expect("source id"),
            generation: 1,
            candidate: slow_candidate(package_id.as_str()),
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        }),
        EnqueueOutcome::Accepted
    ));
    wait_until_in_flight(&compiler, &package_id);
    assert!(
        compiler
            .cancel(&package_id, CandidateCancellation::Disable)
            .is_empty()
    );
    let terminals = await_results(&compiler, 1);
    assert_eq!(terminals.len(), 1);
    assert!(matches!(
        terminals[0],
        CandidateTerminal::CancelledByDisable(_)
    ));
    assert!(compiler.shutdown().is_empty());
}

#[test]
fn shutdown_accounts_for_an_in_flight_generation_without_deadlock() {
    let idl = nexa_idl::parse(IDL).expect("IDL");
    let mut compiler = DevelopmentCompiler::start(&DevelopmentConfig::default()).expect("worker");
    let package_id = PackageId::new("tests.slow-shutdown").expect("package id");
    assert!(matches!(
        compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("slow-shutdown").expect("source id"),
            generation: 1,
            candidate: slow_candidate(package_id.as_str()),
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        }),
        EnqueueOutcome::Accepted
    ));
    wait_until_in_flight(&compiler, &package_id);
    let started = Instant::now();
    let terminals = compiler.shutdown();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(terminals.len(), 1);
    assert!(matches!(
        terminals[0],
        CandidateTerminal::CancelledByShutdown(_)
    ));
    assert!(compiler.inspection().in_flight_package.is_none());
    assert_eq!(compiler.inspection().queued_packages, 0);
    assert_eq!(compiler.inspection().completed_results, 0);
}
