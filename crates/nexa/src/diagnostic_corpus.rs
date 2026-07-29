use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nexa_core::FileId;
use serde::{Deserialize, Serialize};

const ALLOWED_PIPELINES: &[&str] = &[
    "compiler",
    "bytecode_decode",
    "verifier",
    "runtime",
    "host",
    "reload",
    "migration",
    "engine",
];

use crate::{ErrorCategory, NexaError, compile_file};

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
        observed.push(first);
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
        codes,
        cases: observed,
    })
}

pub fn run_diagnostic_corpus(
    root: &Path,
    engine: EngineDiagnosticReport,
) -> Result<DiagnosticCorpusReport, String> {
    let compiler = run_compiler_diagnostic_cases(root)?;
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
        .map(|(_, case)| case.code.clone())
        .collect::<BTreeSet<_>>();
    if fixture_set.len() != loaded.len() {
        return Err("diagnostic case codes are not unique".into());
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
        source_backed_zero_zero_spans: compiler.source_backed_zero_zero_spans,
        source_backed_inexact_spans: compiler.source_backed_inexact_spans,
        deterministic_cases: compiler.deterministic_cases
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

fn execute_binary_case(
    case: &DiagnosticCase,
    case_path: &Path,
) -> Result<ObservedDiagnosticCase, String> {
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
    let bytes = decode_hex(encoded.trim()).map_err(|error| {
        format!(
            "{} is not a versioned hex fixture: {error}",
            input.display()
        )
    })?;
    let error = match case.pipeline.as_str() {
        "bytecode_decode" => crate::decode_module(&bytes, nexa_bytecode::DecodeLimits::default())
            .expect_err("decode fixture must fail"),
        "verifier" => {
            let module = crate::decode_module(&bytes, nexa_bytecode::DecodeLimits::default())
                .map_err(|error| {
                    format!("{} failed before verification: {error}", input.display())
                })?;
            crate::verify_module(module, nexa_verifier::VerifierLimits::default())
                .expect_err("verifier fixture must fail")
        }
        _ => {
            return Err(format!(
                "{} has an invalid binary pipeline",
                case_path.display()
            ));
        }
    };
    let summary = error
        .code()
        .definition()
        .ok_or_else(|| format!("{} emitted an unregistered code", input.display()))?
        .summary;
    let human = error.to_string();
    let passed = error.code().as_str() == case.code
        && error.category().as_str() == case.category
        && normalized(summary).contains(&normalized(&case.expected.message_contains))
        && human.contains(&case.code);
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
    Ok(ObservedDiagnosticCase {
        code: case.code.clone(),
        observed: error.code().to_string(),
        pipeline: case.pipeline.clone(),
        category: error.category().as_str().to_owned(),
        primary_text: String::new(),
        primary_start: 0,
        primary_end: 0,
        secondary_count: 0,
        human_output: true,
        json_output: true,
        passed,
    })
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
    if case.expected.secondary_count != case.expected.secondary_texts.len() {
        return Err(format!(
            "{} secondary_count does not match secondary_texts",
            path.display()
        ));
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
    let error = compile_file(&source, FileId(1))
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
