use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexa_analysis::{
    AnalysisEnvironment, BuildFingerprintInput, CompilationOptions, NormalizedPackagePath,
    PackageManifest, PackageSourceSet, QueryDatabase, ResolvedBuildInput, ResolvedDependencyGraph,
    ResolvedPackage, ResolvedTestInput, SourceId, SourceRole, SourceSetBuilder, analyze_package,
    analyze_package_tests, source_set_fingerprint,
};
use nexa_core::FileId;
use nexa_diagnostics::{
    ByteRange, Diagnostic as LeafDiagnostic, DiagnosticBatch, DiagnosticRenderer,
    Label as LeafLabel, LabelStyle, Severity as LeafSeverity, SourceIdentity,
    SourceSnapshotRegistry,
};
use serde::{Deserialize, Serialize};

const ALLOWED_PIPELINES: &[&str] = &[
    "compiler",
    "analysis",
    "bytecode_decode",
    "verifier",
    "runtime",
    "host",
    "reload",
    "migration",
    "engine",
];

use crate::{Diagnostic, ErrorCategory, NexaError};

#[derive(Clone, Debug, Deserialize)]
struct DiagnosticCase {
    version: u32,
    code: String,
    category: String,
    pipeline: String,
    input: PathBuf,
    expected: ExpectedDiagnostic,
}

#[derive(Clone, Debug, Deserialize)]
struct ExpectedDiagnostic {
    primary_text: String,
    #[serde(default = "first_occurrence")]
    primary_occurrence: usize,
    secondary_count: usize,
    #[serde(default)]
    secondary_texts: Vec<String>,
    #[serde(default)]
    related_messages: Vec<String>,
    #[serde(default)]
    related_texts: Vec<String>,
    message_contains: String,
    #[serde(default)]
    notes: Vec<String>,
}

const fn first_occurrence() -> usize {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ObservedDiagnosticCase {
    pub code: String,
    pub observed: String,
    pub pipeline: String,
    pub category: String,
    pub primary_text: String,
    pub primary_start: u32,
    pub primary_end: u32,
    pub secondary_count: usize,
    pub human_output: bool,
    pub json_output: bool,
    pub passed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompilerDiagnosticReport {
    pub schema_version: u32,
    pub case_count: usize,
    pub deterministic_cases: usize,
    pub source_backed_zero_zero_spans: usize,
    pub source_backed_inexact_spans: usize,
    pub codes: Vec<String>,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AnalysisDiagnosticReport {
    pub schema_version: u32,
    pub case_count: usize,
    pub deterministic_cases: usize,
    pub source_backed_zero_zero_spans: usize,
    pub source_backed_inexact_spans: usize,
    pub codes: Vec<String>,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BinaryDiagnosticReport {
    pub schema_version: u32,
    pub case_count: usize,
    pub passed: usize,
    pub deterministic_cases: usize,
    pub codes: Vec<String>,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeDiagnosticReport {
    pub schema_version: u32,
    pub case_count: usize,
    pub passed: usize,
    pub deterministic_cases: usize,
    pub direct_nexa_error_construction: bool,
    pub multi_file_source_evidence: crate::MultiFileRuntimeDiagnosticEvidence,
    pub codes: Vec<String>,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiagnosticCorpusReport {
    pub schema_version: u32,
    pub registered_codes: usize,
    pub emission_definitions: usize,
    pub fixture_codes: usize,
    pub observed_codes: usize,
    pub missing_codes: Vec<String>,
    pub unexpected_codes: Vec<String>,
    pub source_backed_zero_zero_spans: usize,
    pub source_backed_inexact_spans: usize,
    pub deterministic_cases: usize,
    pub engine: EngineDiagnosticReport,
    pub case_format: CaseFormatReport,
    pub pipelines: PipelineReport,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineDiagnosticReport {
    pub registered: usize,
    pub observed_through_real_paths: usize,
    pub direct_diagnostic_construction: usize,
    pub human_output: usize,
    pub json_output: usize,
    pub ndjson_output: usize,
    pub deterministic: usize,
    pub codes: Vec<String>,
    pub cases: Vec<ObservedDiagnosticCase>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaseFormatReport {
    pub version: u32,
    pub invalid_pipelines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PipelineReport {
    pub compiler: CompilerPipelineReport,
    pub analysis: CompilerPipelineReport,
    pub bytecode_verifier: CountPipelineReport,
    pub runtime_family: RuntimePipelineReport,
    pub engine: CountPipelineReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompilerPipelineReport {
    pub cases: usize,
    pub direct_error_construction: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CountPipelineReport {
    pub cases: usize,
    pub passed: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimePipelineReport {
    pub cases: usize,
    pub passed: usize,
    pub direct_nexa_error_construction: bool,
}

#[derive(Debug, Deserialize)]
struct RuntimeFixture {
    version: u32,
    scenario: String,
}

pub fn run_compiler_diagnostic_cases(root: &Path) -> Result<CompilerDiagnosticReport, String> {
    let cases_dir = root.join("fixtures/diagnostics/cases");
    let mut paths = std::fs::read_dir(&cases_dir)
        .map_err(|error| format!("{}: {error}", cases_dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    let mut cases = Vec::new();
    let mut deterministic_cases = 0;
    for path in paths {
        let case: DiagnosticCase = serde_json::from_slice(
            &std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        if case.pipeline != "compiler" {
            continue;
        }
        validate_case_shape(&case, &path)?;
        let first = execute_compiler_case(&case, &path)?;
        let second = execute_compiler_case(&case, &path)?;
        if first != second {
            return Err(format!("{} was not deterministic", path.display()));
        }
        deterministic_cases += 1;
        cases.push(first);
    }
    let source_backed_zero_zero_spans = cases
        .iter()
        .filter(|case| case.primary_start == 0 && case.primary_end == 0)
        .count();
    let source_backed_inexact_spans = cases.iter().filter(|case| !case.passed).count();
    let codes = cases.iter().map(|case| case.code.clone()).collect();
    Ok(CompilerDiagnosticReport {
        schema_version: 1,
        case_count: cases.len(),
        deterministic_cases,
        source_backed_zero_zero_spans,
        source_backed_inexact_spans,
        codes,
        cases,
    })
}

pub fn run_analysis_diagnostic_cases(root: &Path) -> Result<AnalysisDiagnosticReport, String> {
    let cases = load_cases(root)?;
    let mut observed = Vec::new();
    let mut deterministic_cases = 0;
    for (path, case) in cases {
        if case.pipeline != "analysis" {
            continue;
        }
        validate_case_shape(&case, &path)?;
        let first = execute_analysis_case(&case, &path)?;
        let second = execute_analysis_case(&case, &path)?;
        if first != second {
            return Err(format!("{} was not deterministic", path.display()));
        }
        deterministic_cases += 1;
        observed.push(first.observed);
    }
    let source_backed_zero_zero_spans = observed
        .iter()
        .filter(|case| case.primary_start == 0 && case.primary_end == 0)
        .count();
    let source_backed_inexact_spans = observed.iter().filter(|case| !case.passed).count();
    let codes = observed.iter().map(|case| case.code.clone()).collect();
    Ok(AnalysisDiagnosticReport {
        schema_version: 1,
        case_count: observed.len(),
        deterministic_cases,
        source_backed_zero_zero_spans,
        source_backed_inexact_spans,
        codes,
        cases: observed,
    })
}

pub fn run_binary_diagnostic_cases(root: &Path) -> Result<BinaryDiagnosticReport, String> {
    let cases = load_cases(root)?;
    let mut observed = Vec::new();
    let mut deterministic_cases = 0;
    for (path, case) in cases {
        if !matches!(case.pipeline.as_str(), "bytecode_decode" | "verifier") {
            continue;
        }
        if case.version != 1 {
            return Err(format!("{} has unsupported case version", path.display()));
        }
        let first = execute_binary_case(&case, &path)?;
        let second = execute_binary_case(&case, &path)?;
        if first != second {
            return Err(format!("{} was not deterministic", path.display()));
        }
        deterministic_cases += 1;
        observed.push(first.observed);
    }
    let passed = observed.iter().filter(|case| case.passed).count();
    let codes = observed.iter().map(|case| case.code.clone()).collect();
    Ok(BinaryDiagnosticReport {
        schema_version: 1,
        case_count: observed.len(),
        passed,
        deterministic_cases,
        codes,
        cases: observed,
    })
}

fn load_cases(root: &Path) -> Result<Vec<(PathBuf, DiagnosticCase)>, String> {
    let cases_dir = root.join("fixtures/diagnostics/cases");
    let mut paths = std::fs::read_dir(&cases_dir)
        .map_err(|error| format!("{}: {error}", cases_dir.display()))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let case = serde_json::from_slice(
                &std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?,
            )
            .map_err(|error| format!("{}: {error}", path.display()))?;
            Ok((path, case))
        })
        .collect()
}

pub fn run_runtime_diagnostic_cases(root: &Path) -> Result<RuntimeDiagnosticReport, String> {
    let cases = load_cases(root)?;
    let end_to_end = crate::run_runtime_diagnostic_end_to_end()?;
    if !end_to_end.failures.is_empty()
        || !end_to_end.missing_codes.is_empty()
        || !end_to_end.nondeterministic_cases.is_empty()
    {
        return Err(format!(
            "runtime end-to-end diagnostics failed: failures={:?} missing={:?} nondeterministic={:?}",
            end_to_end.failures, end_to_end.missing_codes, end_to_end.nondeterministic_cases
        ));
    }
    let mut observed = Vec::new();
    let mut deterministic_cases = 0;
    for (path, case) in cases {
        if !matches!(
            case.pipeline.as_str(),
            "runtime" | "host" | "reload" | "migration"
        ) {
            continue;
        }
        if case.version != 1 {
            return Err(format!("{} has unsupported case version", path.display()));
        }
        let input = path
            .parent()
            .expect("case path has a parent")
            .join(&case.input);
        let fixture: RuntimeFixture = serde_json::from_slice(
            &std::fs::read(&input).map_err(|error| format!("{}: {error}", input.display()))?,
        )
        .map_err(|error| format!("{}: {error}", input.display()))?;
        if fixture.version != 1 || fixture.scenario.trim().is_empty() {
            return Err(format!(
                "{} has an invalid runtime fixture",
                input.display()
            ));
        }
        let evidence = end_to_end
            .cases
            .get(&case.code)
            .ok_or_else(|| format!("{} has no end-to-end evidence", case.code))?;
        let summary = crate::ERROR_CODE_TABLE
            .iter()
            .find(|definition| definition.code.to_string() == case.code)
            .map(|definition| definition.summary)
            .ok_or_else(|| format!("{} is not registered", case.code))?;
        let passed = evidence.passed
            && evidence.observed == case.code
            && evidence.category == case.category
            && normalized(summary).contains(&normalized(&case.expected.message_contains))
            && evidence.human_output
            && evidence.json_output
            && evidence.real_realm_runtime
            && evidence.direct_classification_helper_calls == 0;
        if !passed {
            return Err(format!(
                "{} observed {} {} with invalid end-to-end evidence",
                input.display(),
                evidence.category,
                evidence.observed
            ));
        }
        deterministic_cases += usize::from(evidence.deterministic);
        observed.push(ObservedDiagnosticCase {
            code: case.code.clone(),
            observed: evidence.observed.clone(),
            pipeline: case.pipeline.clone(),
            category: evidence.category.clone(),
            primary_text: String::new(),
            primary_start: 0,
            primary_end: 0,
            secondary_count: 0,
            human_output: evidence.human_output,
            json_output: evidence.json_output,
            passed,
        });
    }
    let passed = observed.iter().filter(|case| case.passed).count();
    let codes = observed.iter().map(|case| case.code.clone()).collect();
    Ok(RuntimeDiagnosticReport {
        schema_version: 1,
        case_count: observed.len(),
        passed,
        deterministic_cases,
        direct_nexa_error_construction: false,
        multi_file_source_evidence: end_to_end.multi_file_source_evidence,
        codes,
        cases: observed,
    })
}

#[allow(clippy::too_many_lines)]
pub fn run_diagnostic_corpus(
    root: &Path,
    engine: EngineDiagnosticReport,
) -> Result<DiagnosticCorpusReport, String> {
    let compiler = run_compiler_diagnostic_cases(root)?;
    let analysis = run_analysis_diagnostic_cases(root)?;
    let binary = run_binary_diagnostic_cases(root)?;
    let runtime = run_runtime_diagnostic_cases(root)?;
    let loaded = load_cases(root)?;
    let invalid_pipelines = loaded
        .iter()
        .filter(|(_, case)| !ALLOWED_PIPELINES.contains(&case.pipeline.as_str()))
        .map(|(_, case)| case.pipeline.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let fixture_set = loaded
        .iter()
        .map(|(path, _)| {
            path.file_stem()
                .map_or_else(|| "<unknown>".to_owned(), |stem| stem.to_string_lossy().into_owned())
        })
        .collect::<BTreeSet<_>>();
    if fixture_set.len() != loaded.len() {
        return Err("diagnostic case ids are not unique".into());
    }
    let registered_set = crate::ERROR_CODE_TABLE
        .iter()
        .map(|definition| definition.code.to_string())
        .collect::<BTreeSet<_>>();
    let emission_set = crate::ERROR_EMISSION_TABLE
        .iter()
        .map(|definition| definition.code.to_string())
        .collect::<BTreeSet<_>>();
    let mut cases = compiler.cases.clone();
    cases.extend(analysis.cases.clone());
    cases.extend(binary.cases.clone());
    cases.extend(runtime.cases.clone());
    cases.extend(engine.cases.clone());
    cases.sort_by(|left, right| left.code.cmp(&right.code));
    let observed_set = cases
        .iter()
        .map(|case| case.observed.clone())
        .collect::<BTreeSet<_>>();
    let mut missing_codes = registered_set
        .difference(&observed_set)
        .cloned()
        .collect::<Vec<_>>();
    missing_codes.extend(emission_set.difference(&observed_set).cloned());
    missing_codes.extend(fixture_set.difference(&observed_set).cloned());
    missing_codes.sort();
    missing_codes.dedup();
    let mut unexpected_codes = observed_set
        .difference(&registered_set)
        .cloned()
        .collect::<Vec<_>>();
    unexpected_codes.extend(observed_set.difference(&emission_set).cloned());
    unexpected_codes.extend(observed_set.difference(&fixture_set).cloned());
    unexpected_codes.sort();
    unexpected_codes.dedup();
    let engine_cases = engine.cases.len();
    let engine_passed = engine.cases.iter().filter(|case| case.passed).count();
    Ok(DiagnosticCorpusReport {
        schema_version: 1,
        registered_codes: registered_set.len(),
        emission_definitions: emission_set.len(),
        fixture_codes: fixture_set.len(),
        observed_codes: observed_set.len(),
        missing_codes,
        unexpected_codes,
        source_backed_zero_zero_spans: compiler.source_backed_zero_zero_spans
            + analysis.source_backed_zero_zero_spans,
        source_backed_inexact_spans: compiler.source_backed_inexact_spans
            + analysis.source_backed_inexact_spans,
        deterministic_cases: compiler.deterministic_cases
            + analysis.deterministic_cases
            + binary.deterministic_cases
            + runtime.deterministic_cases
            + engine.deterministic,
        case_format: CaseFormatReport {
            version: 1,
            invalid_pipelines,
        },
        pipelines: PipelineReport {
            compiler: CompilerPipelineReport {
                cases: compiler.case_count,
                direct_error_construction: false,
            },
            analysis: CompilerPipelineReport {
                cases: analysis.case_count,
                direct_error_construction: false,
            },
            bytecode_verifier: CountPipelineReport {
                cases: binary.case_count,
                passed: binary.passed,
            },
            runtime_family: RuntimePipelineReport {
                cases: runtime.case_count,
                passed: runtime.passed,
                direct_nexa_error_construction: runtime.direct_nexa_error_construction,
            },
            engine: CountPipelineReport {
                cases: engine_cases,
                passed: engine_passed,
            },
        },
        engine,
        cases,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutedBinaryCase {
    observed: ObservedDiagnosticCase,
    human: String,
    json: String,
    ndjson: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderedBinaryBatch {
    schema: u32,
    position_encoding: String,
    retained_diagnostics: usize,
    diagnostics: Vec<RenderedBinaryDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderedBinaryNdjsonHeader {
    schema: u32,
    position_encoding: String,
    kind: String,
    retained_diagnostics: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderedBinaryNdjsonDiagnostic {
    schema: u32,
    position_encoding: String,
    kind: String,
    diagnostic: RenderedBinaryDiagnostic,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct RenderedBinaryDiagnostic {
    code: String,
    severity: String,
    message: String,
    labels: Vec<RenderedBinaryLabel>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderedBinaryLabel {
    style: String,
    source: RenderedBinarySource,
    byte_range: RenderedBinaryByteRange,
    range: Option<RenderedBinaryRange>,
    message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenderedBinarySource {
    package_id: Option<String>,
    path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct RenderedBinaryByteRange {
    start: u32,
    end: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct RenderedBinaryPosition {
    line: u32,
    character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct RenderedBinaryRange {
    start: RenderedBinaryPosition,
    end: RenderedBinaryPosition,
}

struct BinaryRenderExpectation<'a> {
    code: &'a str,
    category: &'a str,
    message: &'a str,
    source_path: &'a str,
    source_text: &'a str,
    range: ByteRange,
    label_message: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BinaryRenderObservation {
    human_output: bool,
    json_output: bool,
    ndjson_output: bool,
    primary_text: String,
    primary_start: u32,
    primary_end: u32,
    secondary_count: usize,
}

struct BinaryRenderedEvidence {
    human: String,
    json: String,
    ndjson: String,
    observation: BinaryRenderObservation,
}

fn execute_binary_pipeline(
    case: &DiagnosticCase,
    case_path: &Path,
    input: &Path,
    bytes: &[u8],
) -> Result<NexaError, String> {
    match case.pipeline.as_str() {
        "bytecode_decode" => Ok(crate::decode_module(
            bytes,
            nexa_bytecode::DecodeLimits::default(),
        )
        .expect_err("decode fixture must fail")),
        "verifier" => {
            let module = crate::decode_module(bytes, nexa_bytecode::DecodeLimits::default())
                .map_err(|error| {
                    format!("{} failed before verification: {error}", input.display())
                })?;
            Ok(
                crate::verify_module(module, nexa_verifier::VerifierLimits::default())
                    .expect_err("verifier fixture must fail"),
            )
        }
        _ => Err(format!(
            "{} has an invalid binary pipeline",
            case_path.display()
        )),
    }
}

fn execute_binary_case(
    case: &DiagnosticCase,
    case_path: &Path,
) -> Result<ExecutedBinaryCase, String> {
    let input = case_path
        .parent()
        .expect("case path has a parent")
        .join(&case.input);
    let encoded =
        std::fs::read_to_string(&input).map_err(|error| format!("{}: {error}", input.display()))?;
    if encoded.contains(&case.code) {
        return Err(format!(
            "{} embeds its expected diagnostic code",
            input.display()
        ));
    }
    let encoded = encoded.trim();
    let bytes = decode_hex(encoded).map_err(|error| {
        format!(
            "{} is not a versioned hex fixture: {error}",
            input.display()
        )
    })?;
    let error = execute_binary_pipeline(case, case_path, &input, &bytes)?;
    let summary = error
        .code()
        .definition()
        .ok_or_else(|| format!("{} emitted an unregistered code", input.display()))?
        .summary;
    let source_path = case.input.to_string_lossy().replace('\\', "/");
    let source_identity = SourceIdentity::standalone(source_path.clone());
    let primary_end = u32::try_from(encoded.len())
        .map_err(|_| format!("{} is too large for a diagnostic range", input.display()))?;
    if primary_end == 0 {
        return Err(format!("{} is an empty binary fixture", input.display()));
    }
    let primary_range = ByteRange::new(0, primary_end);
    let label_message = format!("{} rejected this binary fixture", case.pipeline);
    let message = format!("{summary}: {error}");
    let rendered = render_binary_diagnostic(
        &error,
        &input,
        &source_identity,
        &BinaryRenderExpectation {
            code: &case.code,
            category: &case.category,
            message: &message,
            source_path: &source_path,
            source_text: encoded,
            range: primary_range,
            label_message: &label_message,
        },
    )?;
    let rendering = rendered.observation;
    let passed = error.code().as_str() == case.code
        && error.category().as_str() == case.category
        && normalized(summary).contains(&normalized(&case.expected.message_contains))
        && rendering.human_output
        && rendering.json_output
        && rendering.ndjson_output;
    if !passed {
        return Err(format!(
            "{} emitted {} {} instead of {} {}",
            input.display(),
            error.category().as_str(),
            error.code(),
            case.category,
            case.code
        ));
    }
    Ok(ExecutedBinaryCase {
        observed: ObservedDiagnosticCase {
            code: case.code.clone(),
            observed: error.code().to_string(),
            pipeline: case.pipeline.clone(),
            category: error.category().as_str().to_owned(),
            primary_text: rendering.primary_text,
            primary_start: rendering.primary_start,
            primary_end: rendering.primary_end,
            secondary_count: rendering.secondary_count,
            human_output: rendering.human_output,
            json_output: rendering.json_output,
            passed,
        },
        human: rendered.human,
        json: rendered.json,
        ndjson: rendered.ndjson,
    })
}

fn render_binary_diagnostic(
    error: &NexaError,
    input: &Path,
    source_identity: &SourceIdentity,
    expected: &BinaryRenderExpectation<'_>,
) -> Result<BinaryRenderedEvidence, String> {
    let mut sources = SourceSnapshotRegistry::builder();
    sources
        .insert(source_identity.clone(), expected.source_text.to_owned())
        .map_err(|error| format!("{} source registration failed: {error}", input.display()))?;
    let mut batch = DiagnosticBatch::with_default_limits(sources.build());
    batch.push(
        LeafDiagnostic::new(error.code(), LeafSeverity::Error, expected.message).with_label(
            LeafLabel::primary(
                source_identity.clone(),
                expected.range,
                expected.label_message,
            ),
        ),
    );
    let human = DiagnosticRenderer::human(&batch);
    let json = DiagnosticRenderer::json(&batch)
        .map_err(|error| format!("{} JSON render failed: {error}", input.display()))?;
    let ndjson = DiagnosticRenderer::ndjson(&batch)
        .map_err(|error| format!("{} NDJSON render failed: {error}", input.display()))?;
    let observation = observe_binary_rendering(&human, &json, &ndjson, expected)?;
    Ok(BinaryRenderedEvidence {
        human,
        json,
        ndjson,
        observation,
    })
}

fn observe_binary_rendering(
    human: &str,
    json: &str,
    ndjson: &str,
    expected: &BinaryRenderExpectation<'_>,
) -> Result<BinaryRenderObservation, String> {
    let rendered_json = serde_json::from_str::<RenderedBinaryBatch>(json)
        .map_err(|error| format!("binary diagnostic JSON is invalid: {error}"))?;
    let mut ndjson_lines = ndjson.lines();
    let ndjson_header = ndjson_lines
        .next()
        .ok_or_else(|| "binary diagnostic NDJSON is missing its header".to_owned())
        .and_then(|line| {
            serde_json::from_str::<RenderedBinaryNdjsonHeader>(line)
                .map_err(|error| format!("binary diagnostic NDJSON header is invalid: {error}"))
        })?;
    let ndjson_diagnostic = ndjson_lines
        .next()
        .ok_or_else(|| "binary diagnostic NDJSON is missing its diagnostic".to_owned())
        .and_then(|line| {
            serde_json::from_str::<RenderedBinaryNdjsonDiagnostic>(line)
                .map_err(|error| format!("binary diagnostic NDJSON record is invalid: {error}"))
        })?;
    if ndjson_lines.next().is_some() {
        return Err("binary diagnostic NDJSON contains unexpected records".into());
    }

    let json_diagnostic = rendered_json
        .diagnostics
        .first()
        .ok_or("binary diagnostic JSON contains no diagnostics")?;
    let primary = json_diagnostic
        .labels
        .iter()
        .find(|label| label.style == "primary")
        .ok_or("binary diagnostic JSON contains no primary label")?;
    let range_start = usize::try_from(primary.byte_range.start)
        .map_err(|_| "binary diagnostic primary start does not fit usize")?;
    let range_end = usize::try_from(primary.byte_range.end)
        .map_err(|_| "binary diagnostic primary end does not fit usize")?;
    let primary_text = expected
        .source_text
        .get(range_start..range_end)
        .ok_or("binary diagnostic primary range is outside its source")?
        .to_owned();
    let expected_utf16_end = u32::try_from(expected.source_text.encode_utf16().count())
        .map_err(|_| "binary diagnostic UTF-16 range is too large")?;
    let expected_range = RenderedBinaryByteRange {
        start: expected.range.start,
        end: expected.range.end,
    };
    let expected_machine_range = RenderedBinaryRange {
        start: RenderedBinaryPosition {
            line: 0,
            character: 0,
        },
        end: RenderedBinaryPosition {
            line: 0,
            character: expected_utf16_end,
        },
    };
    let diagnostic_matches = |diagnostic: &RenderedBinaryDiagnostic| {
        diagnostic.code == expected.code
            && diagnostic.severity == "error"
            && diagnostic.message == expected.message
            && normalized(&diagnostic.message).contains(&normalized(expected.category))
            && diagnostic.labels.len() == 1
            && diagnostic.labels.first().is_some_and(|label| {
                label.style == "primary"
                    && label.source.package_id.is_none()
                    && label.source.path == expected.source_path
                    && label.byte_range == expected_range
                    && label.range == Some(expected_machine_range)
                    && label.message == expected.label_message
            })
    };
    let human_output = human.contains(&format!("error[{}]: {}", expected.code, expected.message))
        && human.contains(&format!("--> {}:1:1", expected.source_path))
        && human.contains(expected.label_message)
        && human.contains(expected.source_text);
    let json_output = rendered_json.schema == nexa_diagnostics::RENDER_SCHEMA_VERSION
        && rendered_json.position_encoding == nexa_diagnostics::MACHINE_POSITION_ENCODING
        && rendered_json.retained_diagnostics == 1
        && rendered_json.diagnostics.len() == 1
        && diagnostic_matches(json_diagnostic);
    let ndjson_output = ndjson_header.schema == nexa_diagnostics::RENDER_SCHEMA_VERSION
        && ndjson_header.position_encoding == nexa_diagnostics::MACHINE_POSITION_ENCODING
        && ndjson_header.kind == "batch"
        && ndjson_header.retained_diagnostics == 1
        && ndjson_diagnostic.schema == nexa_diagnostics::RENDER_SCHEMA_VERSION
        && ndjson_diagnostic.position_encoding == nexa_diagnostics::MACHINE_POSITION_ENCODING
        && ndjson_diagnostic.kind == "diagnostic"
        && ndjson_diagnostic.diagnostic == *json_diagnostic
        && diagnostic_matches(&ndjson_diagnostic.diagnostic);
    Ok(BinaryRenderObservation {
        human_output,
        json_output,
        ndjson_output,
        primary_text,
        primary_start: primary.byte_range.start,
        primary_end: primary.byte_range.end,
        secondary_count: json_diagnostic
            .labels
            .iter()
            .filter(|label| label.style == "secondary")
            .count(),
    })
}

#[cfg(test)]
mod binary_render_tests {
    use super::{
        BinaryRenderExpectation, RenderedBinaryBatch, execute_binary_case, load_cases,
        observe_binary_rendering, run_binary_diagnostic_cases,
    };
    use nexa_diagnostics::ByteRange;

    #[test]
    fn binary_render_flags_reject_empty_or_mismatched_renderer_output() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let (path, case) = load_cases(&root)
            .unwrap()
            .into_iter()
            .find(|(_, case)| case.code == "NX3001")
            .expect("NX3001 fixture case");
        let executed = execute_binary_case(&case, &path).unwrap();
        let rendered = serde_json::from_str::<RenderedBinaryBatch>(&executed.json).unwrap();
        let diagnostic = rendered.diagnostics.first().expect("rendered diagnostic");
        let primary = diagnostic.labels.first().expect("rendered primary label");
        let expectation = BinaryRenderExpectation {
            code: &case.code,
            category: &case.category,
            message: &diagnostic.message,
            source_path: &primary.source.path,
            source_text: &executed.observed.primary_text,
            range: ByteRange::new(
                executed.observed.primary_start,
                executed.observed.primary_end,
            ),
            label_message: &primary.message,
        };

        let observed = observe_binary_rendering(
            &executed.human,
            &executed.json,
            &executed.ndjson,
            &expectation,
        )
        .unwrap();
        assert!(observed.human_output && observed.json_output && observed.ndjson_output);
        assert!(!observed.primary_text.is_empty());
        assert_eq!(observed.primary_start, 0);
        assert_eq!(
            usize::try_from(observed.primary_end).unwrap(),
            observed.primary_text.len()
        );

        let no_human =
            observe_binary_rendering("", &executed.json, &executed.ndjson, &expectation).unwrap();
        assert!(!no_human.human_output);
        assert!(no_human.json_output && no_human.ndjson_output);

        let wrong_json = executed.json.replacen(&case.code, "NX9999", 1);
        let mismatched_json =
            observe_binary_rendering(&executed.human, &wrong_json, &executed.ndjson, &expectation)
                .unwrap();
        assert!(!mismatched_json.json_output);
        assert!(!mismatched_json.ndjson_output);

        let wrong_ndjson = executed.ndjson.replacen(&case.code, "NX9999", 1);
        let mismatched_ndjson =
            observe_binary_rendering(&executed.human, &executed.json, &wrong_ndjson, &expectation)
                .unwrap();
        assert!(mismatched_ndjson.json_output);
        assert!(!mismatched_ndjson.ndjson_output);

        assert!(
            observe_binary_rendering(&executed.human, "", &executed.ndjson, &expectation).is_err()
        );
        assert!(
            observe_binary_rendering(&executed.human, &executed.json, "", &expectation).is_err()
        );
    }

    #[test]
    fn every_binary_case_observes_all_renderer_channels() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = run_binary_diagnostic_cases(&root).expect("binary diagnostic report");
        assert!(report.case_count > 0);
        assert_eq!(report.passed, report.case_count);
        assert_eq!(report.deterministic_cases, report.case_count);
        assert_eq!(report.cases.len(), report.case_count);
        assert!(report.cases.iter().all(|case| {
            case.passed
                && case.human_output
                && case.json_output
                && !case.primary_text.is_empty()
                && case.primary_start < case.primary_end
        }));
    }
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, &'static str> {
    if !encoded.len().is_multiple_of(2) {
        return Err("odd byte count");
    }
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0]).ok_or("invalid high nibble")?;
            let low = hex_nibble(pair[1]).ok_or("invalid low nibble")?;
            Ok((high << 4) | low)
        })
        .collect()
}

const fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn validate_case_shape(case: &DiagnosticCase, path: &Path) -> Result<(), String> {
    if case.version != 1 {
        return Err(format!("{} has unsupported case version", path.display()));
    }
    if case.category != "diagnostic" {
        return Err(format!("{} is not a compiler diagnostic", path.display()));
    }
    if case.expected.primary_occurrence == 0 {
        return Err(format!("{} has a zero primary occurrence", path.display()));
    }
    if case.expected.secondary_count
        != case.expected.secondary_texts.len() + case.expected.related_messages.len()
    {
        return Err(format!(
            "{} secondary_count does not match secondary_texts + related_messages",
            path.display()
        ));
    }
    if !case.expected.related_texts.is_empty()
        && case.expected.related_texts.len() != case.expected.related_messages.len()
    {
        return Err(format!(
            "{} related_texts must be empty or match related_messages",
            path.display()
        ));
    }
    Ok(())
}

fn rendered_source_matches(rendered: &serde_json::Value, identity: &SourceIdentity) -> bool {
    rendered.get("packageId").is_some()
        && rendered.get("path").is_some()
        && rendered["packageId"].as_str() == identity.package_id()
        && rendered["path"].as_str() == Some(identity.path())
}

fn rendered_location_matches(
    rendered: &serde_json::Value,
    identity: &SourceIdentity,
    range: ByteRange,
    sources: &SourceSnapshotRegistry,
) -> bool {
    let Some(snapshot) = sources.get(identity) else {
        return false;
    };
    let utf16 = snapshot.utf16_range(range);
    rendered_source_matches(&rendered["source"], identity)
        && rendered["byteRange"]["start"].as_u64() == Some(u64::from(range.start))
        && rendered["byteRange"]["end"].as_u64() == Some(u64::from(range.end))
        && rendered["range"]["start"]["line"].as_u64() == Some(u64::from(utf16.start.line))
        && rendered["range"]["start"]["character"].as_u64()
            == Some(u64::from(utf16.start.character))
        && rendered["range"]["end"]["line"].as_u64() == Some(u64::from(utf16.end.line))
        && rendered["range"]["end"]["character"].as_u64() == Some(u64::from(utf16.end.character))
}

fn rendered_analysis_diagnostic_matches(
    rendered: &serde_json::Value,
    diagnostic: &LeafDiagnostic,
    sources: &SourceSnapshotRegistry,
) -> bool {
    let Some(labels) = rendered["labels"].as_array() else {
        return false;
    };
    let Some(related) = rendered["related"].as_array() else {
        return false;
    };
    rendered["code"] == diagnostic.code.as_str()
        && rendered["severity"] == diagnostic.severity.as_str()
        && rendered["message"] == diagnostic.message.as_ref()
        && labels.len() == diagnostic.labels.len()
        && labels
            .iter()
            .zip(&diagnostic.labels)
            .all(|(rendered, label)| {
                let expected_style = match label.style {
                    LabelStyle::Primary => "primary",
                    LabelStyle::Secondary => "secondary",
                };
                rendered["style"] == expected_style
                    && rendered["message"] == label.message.as_ref()
                    && rendered_location_matches(rendered, &label.source, label.range, sources)
            })
        && related.len() == diagnostic.related.len()
        && related
            .iter()
            .zip(&diagnostic.related)
            .all(|(rendered, location)| {
                rendered["message"] == location.message.as_ref()
                    && rendered_location_matches(
                        rendered,
                        &location.source,
                        location.range,
                        sources,
                    )
            })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutedAnalysisCase {
    observed: ObservedDiagnosticCase,
    ndjson: String,
}

struct AnalysisFixtureInput {
    product: Arc<ResolvedBuildInput>,
    tests: Option<Arc<PackageSourceSet>>,
}

#[allow(clippy::too_many_lines)]
fn execute_analysis_case(
    case: &DiagnosticCase,
    case_path: &Path,
) -> Result<ExecutedAnalysisCase, String> {
    let fixture = case_path
        .parent()
        .expect("case path has a parent")
        .join(&case.input);
    let input = load_analysis_fixture(&fixture)?;
    let mut database = QueryDatabase::new();
    let environment = AnalysisEnvironment::default();
    let outcome = if let Some(tests) = &input.tests {
        let test_input = ResolvedTestInput::new(Arc::clone(&input.product), Arc::clone(tests))
            .map_err(|error| format!("{}: {error}", fixture.display()))?;
        analyze_package_tests(&test_input, &environment, &mut database)
    } else {
        analyze_package(&input.product, &environment, &mut database)
    };
    let diagnostic = outcome
        .diagnostics
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.code.as_str() == case.code)
        .ok_or_else(|| {
            let observed = outcome
                .diagnostics
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code.as_str())
                .collect::<Vec<_>>();
            format!(
                "{} did not emit {}; observed {observed:?}",
                fixture.display(),
                case.code
            )
        })?;
    let primary = diagnostic
        .primary_label()
        .ok_or_else(|| format!("{} has no primary label", case.code))?;
    let source = outcome
        .diagnostics
        .sources()
        .get(&primary.source)
        .ok_or_else(|| format!("{} primary source is not registered", case.code))?;
    let expected_start = nth_offset(
        source.text(),
        &case.expected.primary_text,
        case.expected.primary_occurrence,
    )
    .ok_or_else(|| {
        format!(
            "{} does not contain primary text {:?}",
            primary.source, case.expected.primary_text
        )
    })?;
    let expected_end = expected_start
        .checked_add(case.expected.primary_text.len())
        .ok_or("primary source offset overflow")?;
    let actual_primary = source
        .text()
        .get(primary.range.start as usize..primary.range.end as usize)
        .ok_or_else(|| format!("{} primary span is out of bounds", case.code))?;
    let secondary_texts = diagnostic
        .labels
        .iter()
        .filter(|label| label.style == LabelStyle::Secondary)
        .map(|label| {
            let source = outcome
                .diagnostics
                .sources()
                .get(&label.source)
                .ok_or_else(|| format!("{} secondary source is not registered", case.code))?;
            source
                .text()
                .get(label.range.start as usize..label.range.end as usize)
                .map(str::to_owned)
                .ok_or_else(|| format!("{} secondary span is out of bounds", case.code))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let related = diagnostic
        .related
        .iter()
        .map(|related| {
            let source = outcome
                .diagnostics
                .sources()
                .get(&related.source)
                .ok_or_else(|| format!("{} related source is not registered", case.code))?;
            let text = source
                .text()
                .get(related.range.start as usize..related.range.end as usize)
                .ok_or_else(|| format!("{} related span is out of bounds", case.code))?;
            Ok::<(String, String), String>((text.to_owned(), related.message.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let related_texts = related
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>();
    let related_messages = related
        .into_iter()
        .map(|(_, message)| message)
        .collect::<Vec<_>>();
    let notes_match = case.expected.notes.iter().all(|expected| {
        diagnostic
            .notes
            .iter()
            .any(|actual| normalized(actual).contains(&normalized(expected)))
    });
    let human = DiagnosticRenderer::human(&outcome.diagnostics);
    let json = DiagnosticRenderer::json(&outcome.diagnostics).map_err(|error| error.to_string())?;
    let ndjson =
        DiagnosticRenderer::ndjson(&outcome.diagnostics).map_err(|error| error.to_string())?;
    let human_output = human.contains(&case.code);
    let json_value = serde_json::from_str::<serde_json::Value>(&json)
        .map_err(|error| format!("{} JSON render failed: {error}", case.code))?;
    let rendered_diagnostic = json_value["diagnostics"]
        .as_array()
        .and_then(|diagnostics| {
            diagnostics
                .iter()
                .find(|rendered| rendered["code"] == case.code)
        });
    let ndjson_values = ndjson
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{} NDJSON render failed: {error}", case.code))?;
    let ndjson_diagnostic = ndjson_values.iter().skip(1).find_map(|record| {
        (record["kind"] == "diagnostic" && record["diagnostic"]["code"] == case.code)
            .then_some(&record["diagnostic"])
    });
    let json_output = json_value["schema"] == nexa_diagnostics::RENDER_SCHEMA_VERSION
        && json_value["positionEncoding"] == nexa_diagnostics::MACHINE_POSITION_ENCODING
        && json_value["retainedDiagnostics"].as_u64()
            == u64::try_from(outcome.diagnostics.len()).ok()
        && rendered_diagnostic.is_some_and(|rendered| {
            rendered_analysis_diagnostic_matches(
                rendered,
                diagnostic,
                outcome.diagnostics.sources(),
            ) && ndjson_diagnostic == Some(rendered)
        })
        && ndjson_values.first().is_some_and(|header| {
            header["schema"] == nexa_diagnostics::RENDER_SCHEMA_VERSION
                && header["positionEncoding"] == nexa_diagnostics::MACHINE_POSITION_ENCODING
                && header["kind"] == "batch"
                && header["retainedDiagnostics"].as_u64()
                    == u64::try_from(outcome.diagnostics.len()).ok()
        })
        && ndjson_values.len() == outcome.diagnostics.len().saturating_add(1)
        && ndjson_values.iter().skip(1).all(|record| {
            record["schema"] == nexa_diagnostics::RENDER_SCHEMA_VERSION
                && record["positionEncoding"] == nexa_diagnostics::MACHINE_POSITION_ENCODING
                && record["kind"] == "diagnostic"
        });
    let passed = actual_primary == case.expected.primary_text
        && primary.range.start as usize == expected_start
        && primary.range.end as usize == expected_end
        && secondary_texts == case.expected.secondary_texts
        && related_messages
            .iter()
            .zip(&case.expected.related_messages)
            .all(|(actual, expected)| normalized(actual).contains(&normalized(expected)))
        && related_messages.len() == case.expected.related_messages.len()
        && (case.expected.related_texts.is_empty() || related_texts == case.expected.related_texts)
        && secondary_texts.len() + related_messages.len() == case.expected.secondary_count
        && normalized(&diagnostic.message).contains(&normalized(&case.expected.message_contains))
        && notes_match
        && human_output
        && json_output;
    Ok(ExecutedAnalysisCase {
        observed: ObservedDiagnosticCase {
            code: case.code.clone(),
            observed: diagnostic.code.to_string(),
            pipeline: case.pipeline.clone(),
            category: case.category.clone(),
            primary_text: actual_primary.to_owned(),
            primary_start: primary.range.start,
            primary_end: primary.range.end,
            secondary_count: secondary_texts.len() + related_messages.len(),
            human_output,
            json_output,
            passed,
        },
        ndjson,
    })
}

#[allow(clippy::too_many_lines)]
fn load_analysis_fixture(directory: &Path) -> Result<AnalysisFixtureInput, String> {
    let manifest_source = std::fs::read_to_string(directory.join("package.toml"))
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let manifest = Arc::new(
        PackageManifest::parse(&manifest_source)
            .map_err(|error| format!("{}: {error}", directory.display()))?,
    );
    if !manifest.dependencies.is_empty() {
        return Err(format!(
            "{} diagnostic fixtures may not resolve dependencies",
            directory.display()
        ));
    }
    let mut production_paths = Vec::new();
    collect_analysis_sources(&directory.join("src"), &mut production_paths)?;
    let tests_directory = directory.join("tests");
    let mut test_paths = Vec::new();
    if tests_directory.exists() {
        collect_analysis_sources(&tests_directory, &mut test_paths)?;
    }
    production_paths.sort();
    test_paths.sort();
    let compilation_options = CompilationOptions::default();
    let mut source_builder = SourceSetBuilder::new(manifest.id.clone(), compilation_options.limits);
    for path in production_paths {
        let relative = path
            .strip_prefix(directory)
            .map_err(|_| format!("{} escapes its fixture root", path.display()))?;
        let normalized = NormalizedPackagePath::from_path(relative)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        source_builder
            .add(normalized, source, SourceRole::Production)
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    let source_set = Arc::new(
        source_builder
            .build()
            .map_err(|error| format!("{}: {error}", directory.display()))?,
    );
    let source_id = SourceId::new("diagnostic-corpus").map_err(|error| error.to_string())?;
    let package_directory = NormalizedPackagePath::new(format!(
        "diagnostics/{}",
        manifest.id.as_str().replace('.', "/")
    ))
    .map_err(|error| error.to_string())?;
    let graph = Arc::new(ResolvedDependencyGraph {
        root: manifest.id.clone(),
        packages: BTreeMap::from([(
            manifest.id.clone(),
            ResolvedPackage {
                id: manifest.id.clone(),
                version: manifest.version.clone(),
                source_id,
                directory: package_directory,
                kind: manifest.kind,
            },
        )]),
        edges: BTreeSet::new(),
    });
    let standard_library = nexa_stdlib::standard_library();
    let fingerprint_input = BuildFingerprintInput {
        root_package: manifest.id.clone(),
        root_manifest: manifest.canonical_bytes(),
        root_source_set: source_set_fingerprint(&source_set),
        dependency_manifests: BTreeMap::new(),
        dependency_source_sets: BTreeMap::new(),
        host_contract: Vec::new(),
        host_contract_source: Vec::new(),
        host_required_entrypoints: Vec::new(),
        language_version: nexa_analysis::NEXA_LANGUAGE_VERSION,
        standard_library_version: standard_library.version.to_string(),
        standard_library_descriptor: nexa_stdlib::canonical_descriptor_identity(),
        compiler_version: nexa_core::NEXA_COMPILER_VERSION.to_owned(),
        bytecode_version: u32::from(nexa_core::BYTECODE_VERSION),
        runtime_semantics_version: u32::from(nexa_core::RUNTIME_SEMANTICS_VERSION),
        opcode_cost_table_version: nexa_core::OPCODE_COST_TABLE_VERSION,
        deterministic_math_backend: nexa_core::RUNTIME_MATH_BACKEND_ID.to_owned(),
        compiler_options: nexa_analysis::canonical_compilation_options(&compilation_options),
        canonical_lock_graph: Vec::new(),
        repl_session_context: Vec::new(),
    };
    let product = ResolvedBuildInput::new(
        manifest,
        source_set,
        BTreeMap::new(),
        BTreeMap::new(),
        graph,
        None,
        Vec::<u8>::new(),
        Vec::<u8>::new(),
        fingerprint_input.host_required_entrypoints.clone(),
        compilation_options,
        fingerprint_input,
    )
    .map_err(|error| format!("{}: {error}", directory.display()))?;
    let tests = if test_paths.is_empty() {
        None
    } else {
        let mut builder =
            SourceSetBuilder::new(product.root_manifest.id.clone(), compilation_options.limits);
        for path in test_paths {
            let relative = path
                .strip_prefix(directory)
                .map_err(|_| format!("{} escapes its fixture root", path.display()))?;
            let normalized = NormalizedPackagePath::from_path(relative)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            let source = std::fs::read_to_string(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            builder
                .add(normalized, source, SourceRole::Test)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        Some(Arc::new(builder.build().map_err(|error| {
            format!("{}: {error}", directory.display())
        })?))
    };
    Ok(AnalysisFixtureInput {
        product: Arc::new(product),
        tests,
    })
}

fn collect_analysis_sources(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} is a symlink", directory.display()));
    }
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if file_type.is_symlink() {
            return Err(format!("{} is a symlink", path.display()));
        }
        if file_type.is_dir() {
            collect_analysis_sources(&path, output)?;
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "nexa") {
            output.push(path);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn execute_compiler_case(
    case: &DiagnosticCase,
    case_path: &Path,
) -> Result<ObservedDiagnosticCase, String> {
    let input = case_path
        .parent()
        .expect("case path has a parent")
        .join(&case.input);
    let source =
        std::fs::read_to_string(&input).map_err(|error| format!("{}: {error}", input.display()))?;
    if source.contains(&case.code) {
        return Err(format!(
            "{} embeds its expected diagnostic code",
            input.display()
        ));
    }
    let error = nexa_compiler::compile_file(&source, FileId(1))
        .map_err(|error| NexaError::Diagnostic(Box::new(Diagnostic::new(&error, FileId(1)))))
        .expect_err("a compiler diagnostic fixture must fail compilation");
    let NexaError::Diagnostic(diagnostic) = error else {
        return Err(format!("{} did not emit a diagnostic", input.display()));
    };
    let primary = diagnostic
        .primary
        .as_ref()
        .ok_or_else(|| format!("{} has no primary label", input.display()))?;
    let expected_start = nth_offset(
        &source,
        &case.expected.primary_text,
        case.expected.primary_occurrence,
    )
    .ok_or_else(|| {
        format!(
            "{} does not contain primary text {:?}",
            input.display(),
            case.expected.primary_text
        )
    })?;
    let expected_end = expected_start
        .checked_add(case.expected.primary_text.len())
        .ok_or("primary source offset overflow")?;
    let actual_primary = source
        .get(primary.span.start as usize..primary.span.end as usize)
        .ok_or_else(|| format!("{} primary span is out of bounds", input.display()))?;

    let secondary_texts = diagnostic
        .secondary
        .iter()
        .map(|label| {
            source
                .get(label.span.start as usize..label.span.end as usize)
                .ok_or_else(|| format!("{} secondary span is out of bounds", input.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let notes_match = case.expected.notes.iter().all(|expected| {
        diagnostic
            .notes
            .iter()
            .any(|note| note.to_string().contains(expected))
    });
    let human = diagnostic.to_string();
    let json = diagnostic
        .to_json()
        .map_err(|error| format!("{} JSON render failed: {error}", input.display()))?;
    let json_value: serde_json::Value =
        serde_json::from_str(&json).map_err(|error| error.to_string())?;
    let category = ErrorCategory::Diagnostic.as_str();
    let human_output = human.contains(&case.code)
        && human.contains(&case.expected.message_contains)
        && human.contains(&primary.span.start.to_string())
        && human.contains(&primary.span.end.to_string());
    let json_output = json_value["code"] == case.code
        && json_value["primary"]["start"] == primary.span.start
        && json_value["primary"]["end"] == primary.span.end;
    let passed = diagnostic.code.as_str() == case.code
        && category == case.category
        && primary.span.start as usize == expected_start
        && primary.span.end as usize == expected_end
        && actual_primary == case.expected.primary_text
        && diagnostic.secondary.len() == case.expected.secondary_count
        && secondary_texts.iter().copied().eq(case
            .expected
            .secondary_texts
            .iter()
            .map(String::as_str))
        && diagnostic
            .message
            .to_string()
            .contains(&case.expected.message_contains)
        && notes_match
        && human_output
        && json_output;
    if !passed {
        return Err(format!(
            "{} emitted an inexact diagnostic: {diagnostic:?}",
            input.display()
        ));
    }
    Ok(ObservedDiagnosticCase {
        code: case.code.clone(),
        observed: diagnostic.code.to_string(),
        pipeline: case.pipeline.clone(),
        category: category.to_owned(),
        primary_text: actual_primary.to_owned(),
        primary_start: primary.span.start,
        primary_end: primary.span.end,
        secondary_count: diagnostic.secondary.len(),
        human_output,
        json_output,
        passed,
    })
}

fn nth_offset(source: &str, needle: &str, occurrence: usize) -> Option<usize> {
    source
        .match_indices(needle)
        .nth(occurrence.saturating_sub(1))
        .map(|(offset, _)| offset)
}
