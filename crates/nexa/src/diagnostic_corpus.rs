use std::path::{Path, PathBuf};

use nexa_core::FileId;
use serde::{Deserialize, Serialize};

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
