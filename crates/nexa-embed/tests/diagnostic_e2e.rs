use std::path::Path;

#[test]
fn every_registered_engine_diagnostic_has_real_deterministic_evidence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report = nexa_embed::run_engine_diagnostic_cases(&root).expect("Engine diagnostic report");
    assert!(report.registered > 0);
    assert_eq!(report.observed_through_real_paths, report.registered);
    assert_eq!(report.direct_diagnostic_construction, 0);
    assert_eq!(report.human_output, report.registered);
    assert_eq!(report.json_output, report.registered);
    assert_eq!(report.ndjson_output, report.registered);
    assert_eq!(report.deterministic, report.registered);
    assert!(
        report.cases.iter().all(|case| case.passed),
        "failed Engine diagnostic cases: {:#?}",
        report
            .cases
            .iter()
            .filter(|case| !case.passed)
            .collect::<Vec<_>>()
    );
    assert!(
        report
            .cases
            .iter()
            .any(|case| !case.primary_text.is_empty())
    );
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
    assert!(trap.observation.real_engine_path);
    assert!(trap.observation.deterministic);
    assert!(trap.rendering.human_matches_diagnostic);
    assert!(trap.rendering.json_matches_diagnostic);
    assert!(trap.rendering.ndjson_matches_diagnostic);
    assert!(trap.observation.source_evidence_valid);
    assert_eq!(trap.diagnostic.context.export.as_deref(), Some("Run"));
    assert!(
        trap.diagnostic
            .related
            .iter()
            .any(|related| related.message.contains("at Run"))
    );

    let primary = trap
        .diagnostic
        .diagnostic
        .primary
        .as_ref()
        .expect("trap primary label");
    let identity = trap.diagnostic.file.as_ref().expect("trap source identity");
    let source = trap
        .diagnostic
        .source_by_identity(identity)
        .expect("trap source snapshot");
    let primary_text = source
        .text()
        .get(primary.span.start as usize..primary.span.end as usize)
        .expect("trap primary source slice");
    assert!(!primary_text.is_empty());
    assert!(trap.human.contains(primary_text));

    let json: serde_json::Value = serde_json::from_str(&trap.json).expect("diagnostic JSON");
    let ndjson: serde_json::Value =
        serde_json::from_str(trap.ndjson.trim_end()).expect("diagnostic NDJSON");
    assert_eq!(json, ndjson);
    assert_eq!(json["code"], trap.code.as_str());
    assert_eq!(
        json["message"],
        trap.diagnostic.diagnostic.message.to_string()
    );
    assert_eq!(json["sourceIdentity"], identity.to_string());
    assert!(json["range"].is_object());
    assert_eq!(
        json["related"].as_array().map(Vec::len),
        Some(trap.diagnostic.related.len())
    );
}

#[test]
fn engine_evidence_harness_does_not_construct_target_diagnostics() {
    let source = include_str!("../src/diagnostic_evidence.rs");
    assert!(!source.contains("Diagnostic::without_source"));
    assert!(!source.contains("EngineDiagnostic::without_source"));
}
