use std::time::{Duration, Instant};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, DevelopmentCompileRequest, DevelopmentCompiler,
    DevelopmentConfig, DiagnosticRenderer, EngineDiagnostic, EngineDiagnosticStage,
    ExportRequirement, MemorySource, PackagePolicy, PackageRuntimeLimits, PackageSource,
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
) -> Vec<nexa_embed::DevelopmentCompileResult> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut results = Vec::new();
    while Instant::now() < deadline {
        results.extend(compiler.poll());
        if results
            .iter()
            .any(|result| result.generation == expected_generation)
        {
            return results;
        }
        std::thread::yield_now();
    }
    panic!("development compiler did not return generation {expected_generation}");
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
    let mut superseded = Vec::new();
    for generation in 1..=20 {
        let candidate = candidate(&format!("fn Value() -> i32 {{ return {generation}; }}"));
        superseded.extend(compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("tests").expect("source id"),
            generation,
            candidate,
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        }));
    }
    let results = await_results(&compiler, 20);
    assert!(
        results
            .iter()
            .any(|result| result.generation == 20 && result.result.is_ok()),
        "latest result: {:?}",
        results
            .iter()
            .find(|result| result.generation == 20)
            .map(|result| &result.result)
    );
    assert!(superseded.iter().all(|(_, generation, _)| *generation < 20));
    compiler.shutdown();
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
        let _ = compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("tests").expect("source id"),
            generation,
            candidate: candidate(&source),
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        });
        let results = await_results(&compiler, generation);
        assert!(
            results
                .iter()
                .any(|result| result.generation == generation && result.result.is_ok()),
            "generation {generation}: {:?}",
            results
                .iter()
                .find(|result| result.generation == generation)
                .map(|result| &result.result)
        );
    }
    for generation in 101..=200 {
        let _ = compiler.submit(DevelopmentCompileRequest {
            package_id: package_id.clone(),
            source_id: SourceId::new("tests").expect("source id"),
            generation,
            candidate: candidate("fn Value() -> i32 { return missing; }"),
            idl: idl.clone(),
            required_exports: vec![requirement(&idl)],
        });
        let results = await_results(&compiler, generation);
        assert!(
            results
                .iter()
                .any(|result| result.generation == generation && result.result.is_err())
        );
    }
    compiler.shutdown();
}
