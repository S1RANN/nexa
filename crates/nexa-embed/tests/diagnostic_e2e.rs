use std::path::Path;

#[test]
fn every_registered_engine_diagnostic_has_real_deterministic_evidence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report = nexa_embed::run_engine_diagnostic_cases(&root).expect("Engine diagnostic report");
    assert_eq!(report.registered, 13);
    assert_eq!(report.observed_through_real_paths, report.registered);
    assert_eq!(report.direct_diagnostic_construction, 0);
    assert_eq!(report.human_output, report.registered);
    assert_eq!(report.json_output, report.registered);
    assert_eq!(report.ndjson_output, report.registered);
    assert_eq!(report.deterministic, report.registered);
    assert!(report.cases.iter().all(|case| case.passed));
}

#[test]
fn handler_trap_evidence_preserves_source_and_script_stack() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let evidence =
        nexa_embed::run_engine_diagnostic_evidence(&root).expect("Engine diagnostic evidence");
    let trap = evidence
        .iter()
        .find(|item| item.code == nexa::ErrorCode::NX7103)
        .expect("NX7103 evidence");
    assert!(trap.diagnostic.file.is_some());
    assert!(trap.diagnostic.diagnostic.primary.is_some());
    assert_eq!(trap.diagnostic.context.export.as_deref(), Some("Run"));
    assert!(
        trap.diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("at Run"))
    );
}

#[test]
fn engine_evidence_harness_does_not_construct_target_diagnostics() {
    let source = include_str!("../src/diagnostic_evidence.rs");
    assert!(!source.contains("Diagnostic::without_source"));
    assert!(!source.contains("EngineDiagnostic::without_source"));
}
