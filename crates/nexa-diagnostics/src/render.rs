use std::fmt::{self, Write as _};

use serde::Serialize;

use crate::{
    ByteRange, Diagnostic, DiagnosticBatch, DroppedCounts, Label, LabelStyle, RelatedLocation,
    SourceIdentity, SourceSnapshotRegistry, TextEditSuggestion, Utf16Range,
};

pub const RENDER_SCHEMA_VERSION: u32 = 1;
pub const HUMAN_POSITION_ENCODING: &str = "unicode-scalar-1-based";
pub const MACHINE_POSITION_ENCODING: &str = "utf-16-0-based";

pub struct DiagnosticRenderer;

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_BOLD_RED: &str = "\x1b[1;31m";
const COLOR_BOLD_YELLOW: &str = "\x1b[1;33m";
const COLOR_BLUE: &str = "\x1b[34m";
const COLOR_BOLD_BLUE: &str = "\x1b[1;34m";
const COLOR_BOLD_RED_CARET: &str = "\x1b[1;31m";

impl DiagnosticRenderer {
    /// Renders one batch in the rustc-style human layout without ANSI colors.
    #[must_use]
    pub fn human(batch: &DiagnosticBatch) -> String {
        Self::render_human(batch, false)
    }

    /// Renders one batch in the rustc-style human layout with ANSI colors.
    #[must_use]
    pub fn human_colored(batch: &DiagnosticBatch) -> String {
        Self::render_human(batch, true)
    }

    fn render_human(batch: &DiagnosticBatch, color: bool) -> String {
        let mut output = String::new();
        for (index, diagnostic) in batch.diagnostics().iter().enumerate() {
            if index > 0 {
                output.push('\n');
                output.push('\n');
            }
            render_human_diagnostic(&mut output, batch.sources(), diagnostic, color);
        }
        if !batch.diagnostics().is_empty() {
            let emitted = batch.diagnostics().len();
            write!(
                output,
                "\nerror: {emitted} {} emitted",
                if emitted == 1 { "error" } else { "errors" }
            )
            .expect("writing to String cannot fail");
            let suppressed = batch.suppressed();
            if suppressed.diagnostics > 0 {
                write!(
                    output,
                    "; {} downstream {} suppressed ({})",
                    suppressed.diagnostics,
                    if suppressed.diagnostics == 1 {
                        "error"
                    } else {
                        "errors"
                    },
                    suppressed
                        .first_cause
                        .as_deref()
                        .unwrap_or("caused by a previous error"),
                )
                .expect("writing to String cannot fail");
            }
            output.push('\n');
        }
        let dropped = batch.dropped();
        if dropped != DroppedCounts::default() {
            write!(
                output,
                "\nsummary: {} diagnostics dropped, {} text bytes dropped, {} fields truncated",
                dropped.diagnostics, dropped.text_bytes, dropped.truncated_fields
            )
            .expect("writing to String cannot fail");
        }
        output
    }

    pub fn json(batch: &DiagnosticBatch) -> Result<String, RenderError> {
        let output = BatchOutput::new(batch);
        serde_json::to_string_pretty(&output).map_err(RenderError::Json)
    }

    pub fn ndjson(batch: &DiagnosticBatch) -> Result<String, RenderError> {
        let mut output = String::new();
        let header = NdjsonHeader {
            schema: RENDER_SCHEMA_VERSION,
            position_encoding: MACHINE_POSITION_ENCODING,
            kind: "batch",
            retained_diagnostics: batch.len(),
            dropped: DroppedOutput::from(batch.dropped()),
        };
        output.push_str(&serde_json::to_string(&header).map_err(RenderError::Json)?);
        output.push('\n');
        for diagnostic in batch.diagnostics() {
            let line = NdjsonDiagnostic {
                schema: RENDER_SCHEMA_VERSION,
                position_encoding: MACHINE_POSITION_ENCODING,
                kind: "diagnostic",
                diagnostic: DiagnosticOutput::new(batch.sources(), diagnostic),
            };
            output.push_str(&serde_json::to_string(&line).map_err(RenderError::Json)?);
            output.push('\n');
        }
        Ok(output)
    }
}

fn render_human_diagnostic(
    output: &mut String,
    sources: &SourceSnapshotRegistry,
    diagnostic: &Diagnostic,
    color: bool,
) {
    let severity_color = match diagnostic.severity {
        crate::Severity::Error => COLOR_BOLD_RED,
        crate::Severity::Warning => COLOR_BOLD_YELLOW,
        crate::Severity::Note | crate::Severity::Help => COLOR_BOLD_BLUE,
    };
    if color {
        write!(output, "{severity_color}").expect("writing to String cannot fail");
    }
    write!(
        output,
        "{}[{}]: {}",
        diagnostic.severity.as_str(),
        diagnostic.code,
        diagnostic.message
    )
    .expect("writing to String cannot fail");
    if color {
        write!(output, "{COLOR_RESET}").expect("writing to String cannot fail");
    }

    let gutter_width = diagnostic
        .labels
        .iter()
        .filter_map(|label| label_context_gutter_width(sources, label))
        .max()
        .unwrap_or(1);
    for label in &diagnostic.labels {
        render_human_label(output, sources, label, gutter_width, color);
    }
    for related in &diagnostic.related {
        render_human_related(output, sources, related, color);
    }
    for note in &diagnostic.notes {
        write!(
            output,
            "\n   {}= note: {note}{}",
            if color { COLOR_BOLD_BLUE } else { "" },
            if color { COLOR_RESET } else { "" },
        )
        .expect("writing to String cannot fail");
    }
    for fix in &diagnostic.fixes {
        write!(
            output,
            "\n   {}= help: {}{}",
            if color { COLOR_BOLD_BLUE } else { "" },
            fix.message,
            if color { COLOR_RESET } else { "" },
        )
        .expect("writing to String cannot fail");
        if let Some(replacement) = &fix.replacement {
            write!(
                output,
                "\n   {}= help: replace with `{}`{}",
                if color { COLOR_BOLD_BLUE } else { "" },
                escape_replacement(replacement),
                if color { COLOR_RESET } else { "" },
            )
            .expect("writing to String cannot fail");
        }
    }
}

/// Renders a replacement on one safe, readable line: backslashes, the common whitespace escapes,
/// and other control characters become visible so embedded newlines cannot corrupt the human
/// layout and embedded escape sequences stay unambiguous.
fn escape_replacement(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\0' => escaped.push_str("\\0"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            character if (character as u32) < 0x20 || (character as u32) == 0x7f => {
                write!(escaped, "\\x{:02x}", character as u32)
                    .expect("writing to String cannot fail");
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn digit_count(mut value: u32) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// Digits needed for the widest line number a label renders, including its one-line context
/// window above and below (clamped to the non-empty lines that actually exist in the file).
fn label_context_gutter_width(sources: &SourceSnapshotRegistry, label: &Label) -> Option<usize> {
    let snapshot = sources.get(&label.source)?;
    let line = snapshot.human_range(label.range).start.line;
    let mut widest = digit_count(line);
    if let Some(before) = line.checked_sub(1).filter(|&line| {
        snapshot
            .line_text(line)
            .is_some_and(|text| !text.is_empty())
    }) {
        widest = widest.max(digit_count(before));
    }
    if snapshot
        .line_text(line.saturating_add(1))
        .is_some_and(|text| !text.is_empty())
    {
        widest = widest.max(digit_count(line.saturating_add(1)));
    }
    Some(widest)
}

/// Renders one source label as a `--> file:line:col` header, the gutter, the source line, and a
/// caret line whose width matches the span (clamped to the line end, at least one column). The
/// label also gets a minimal one-line context window above and below (clamped to the file's
/// lines, skipping empty lines) so the reader can locate the span without leaving the snippet.
fn render_human_label(
    output: &mut String,
    sources: &SourceSnapshotRegistry,
    label: &Label,
    gutter_width: usize,
    color: bool,
) {
    let Some(snapshot) = sources.get(&label.source) else {
        write!(
            output,
            "\n  --> {}:<source unavailable>",
            display_source(&label.source)
        )
        .expect("writing to String cannot fail");
        return;
    };
    let range = snapshot.human_range(label.range);
    write!(
        output,
        "\n  --> {}:{}:{}",
        display_source(&label.source),
        range.start.line,
        range.start.column
    )
    .expect("writing to String cannot fail");
    let Some(line) = snapshot.line_text(range.start.line) else {
        return;
    };
    let (_, columns) = expand_tabs(line);
    let gutter = " ".repeat(gutter_width);
    if color {
        write!(output, "\n{COLOR_BLUE}{gutter} |{COLOR_RESET}")
            .expect("writing to String cannot fail");
    } else {
        write!(output, "\n{gutter} |").expect("writing to String cannot fail");
    }
    if let Some(before) = range
        .start
        .line
        .checked_sub(1)
        .and_then(|line| snapshot.line_text(line))
        .filter(|text| !text.is_empty())
    {
        render_human_source_line(output, range.start.line - 1, before, gutter_width, color);
    }
    render_human_source_line(output, range.start.line, line, gutter_width, color);
    let caret_start = columns
        .get(range.start.column.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(0);
    let caret_width = if range.start.line == range.end.line {
        columns
            .get(range.end.column.saturating_sub(1) as usize)
            .copied()
            .unwrap_or(caret_start + 1)
            .saturating_sub(caret_start)
    } else {
        columns
            .last()
            .copied()
            .unwrap_or(caret_start)
            .saturating_sub(caret_start)
    };
    let caret_width = caret_width.max(1);
    let caret = if label.style == LabelStyle::Primary {
        "^"
    } else {
        "-"
    };
    let gutter_bar = if color {
        format!("{COLOR_BLUE}{gutter} |{COLOR_RESET}")
    } else {
        format!("{gutter} |")
    };
    write!(
        output,
        "\n{gutter_bar} {}{}{} {}",
        " ".repeat(caret_start),
        if color { COLOR_BOLD_RED_CARET } else { "" },
        caret.repeat(caret_width),
        label.message
    )
    .expect("writing to String cannot fail");
    if color {
        write!(output, "{COLOR_RESET}").expect("writing to String cannot fail");
    }
    if let Some(after) = snapshot
        .line_text(range.start.line.saturating_add(1))
        .filter(|text| !text.is_empty())
    {
        render_human_source_line(output, range.start.line + 1, after, gutter_width, color);
    }
}

/// Renders one source line with its right-aligned line-number gutter, expanding tabs to
/// four-column stops.
fn render_human_source_line(
    output: &mut String,
    line_number: u32,
    text: &str,
    gutter_width: usize,
    color: bool,
) {
    let (expanded, _) = expand_tabs(text);
    if color {
        write!(
            output,
            "\n{COLOR_BLUE}{line_number:>gutter_width$} |{COLOR_RESET} {expanded}"
        )
        .expect("writing to String cannot fail");
    } else {
        write!(output, "\n{line_number:>gutter_width$} | {expanded}")
            .expect("writing to String cannot fail");
    }
}

/// Expands tabs to four-column stops. Returns the expanded line and the display column after each
/// original character (plus one trailing entry for the end of line).
fn expand_tabs(line: &str) -> (String, Vec<usize>) {
    let mut expanded = String::new();
    let mut columns = Vec::with_capacity(line.len() + 1);
    for character in line.chars() {
        if character == '\t' {
            let pad = 4 - expanded.chars().count() % 4;
            expanded.push_str(&" ".repeat(pad));
        } else {
            expanded.push(character);
        }
        columns.push(expanded.chars().count());
    }
    columns.push(expanded.chars().count());
    (expanded, columns)
}

/// Human-readable source identity. REPL cells drop their fixed `nexa.repl` package prefix so the
/// surviving `repl::cell_N` path reads naturally.
fn display_source(identity: &SourceIdentity) -> String {
    if identity.package_id() == Some("nexa.repl") {
        identity.path().to_owned()
    } else {
        identity.to_string()
    }
}

fn render_human_related(
    output: &mut String,
    sources: &SourceSnapshotRegistry,
    related: &RelatedLocation,
    color: bool,
) {
    if let Some(snapshot) = sources.get(&related.source) {
        let range = snapshot.human_range(related.range);
        write!(
            output,
            "\n{}related: {} at {}:{}:{}",
            if color { COLOR_BOLD_BLUE } else { "" },
            related.message,
            display_source(&related.source),
            range.start.line,
            range.start.column
        )
        .expect("writing to String cannot fail");
        if color {
            write!(output, "{COLOR_RESET}").expect("writing to String cannot fail");
        }
    } else {
        write!(
            output,
            "\n{}related: {} at {}:<source unavailable>{}",
            if color { COLOR_BOLD_BLUE } else { "" },
            related.message,
            display_source(&related.source),
            if color { COLOR_RESET } else { "" },
        )
        .expect("writing to String cannot fail");
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchOutput<'a> {
    schema: u32,
    position_encoding: &'static str,
    retained_diagnostics: usize,
    dropped: DroppedOutput,
    diagnostics: Vec<DiagnosticOutput<'a>>,
}

impl<'a> BatchOutput<'a> {
    fn new(batch: &'a DiagnosticBatch) -> Self {
        Self {
            schema: RENDER_SCHEMA_VERSION,
            position_encoding: MACHINE_POSITION_ENCODING,
            retained_diagnostics: batch.len(),
            dropped: DroppedOutput::from(batch.dropped()),
            diagnostics: batch
                .diagnostics()
                .iter()
                .map(|diagnostic| DiagnosticOutput::new(batch.sources(), diagnostic))
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NdjsonHeader {
    schema: u32,
    position_encoding: &'static str,
    kind: &'static str,
    retained_diagnostics: usize,
    dropped: DroppedOutput,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NdjsonDiagnostic<'a> {
    schema: u32,
    position_encoding: &'static str,
    kind: &'static str,
    diagnostic: DiagnosticOutput<'a>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct DroppedOutput {
    diagnostics: u64,
    text_bytes: u64,
    truncated_fields: u64,
}

impl From<DroppedCounts> for DroppedOutput {
    fn from(dropped: DroppedCounts) -> Self {
        Self {
            diagnostics: dropped.diagnostics,
            text_bytes: dropped.text_bytes,
            truncated_fields: dropped.truncated_fields,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticOutput<'a> {
    code: &'static str,
    severity: &'static str,
    message: &'a str,
    labels: Vec<LabelOutput<'a>>,
    related: Vec<RelatedOutput<'a>>,
    notes: Vec<&'a str>,
    fixes: Vec<FixOutput<'a>>,
}

impl<'a> DiagnosticOutput<'a> {
    fn new(sources: &SourceSnapshotRegistry, diagnostic: &'a Diagnostic) -> Self {
        Self {
            code: diagnostic.code.as_str(),
            severity: diagnostic.severity.as_str(),
            message: &diagnostic.message,
            labels: diagnostic
                .labels
                .iter()
                .map(|label| LabelOutput::new(sources, label))
                .collect(),
            related: diagnostic
                .related
                .iter()
                .map(|related| RelatedOutput::new(sources, related))
                .collect(),
            notes: diagnostic.notes.iter().map(AsRef::as_ref).collect(),
            fixes: diagnostic
                .fixes
                .iter()
                .map(|fix| FixOutput::new(sources, fix))
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LabelOutput<'a> {
    style: &'static str,
    source: SourceOutput<'a>,
    byte_range: ByteRangeOutput,
    range: Option<Utf16RangeOutput>,
    message: &'a str,
}

impl<'a> LabelOutput<'a> {
    fn new(sources: &SourceSnapshotRegistry, label: &'a Label) -> Self {
        Self {
            style: match label.style {
                LabelStyle::Primary => "primary",
                LabelStyle::Secondary => "secondary",
            },
            source: SourceOutput::new(&label.source),
            byte_range: ByteRangeOutput::from(label.range),
            range: machine_range(sources, &label.source, label.range),
            message: &label.message,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RelatedOutput<'a> {
    source: SourceOutput<'a>,
    byte_range: ByteRangeOutput,
    range: Option<Utf16RangeOutput>,
    message: &'a str,
}

impl<'a> RelatedOutput<'a> {
    fn new(sources: &SourceSnapshotRegistry, related: &'a RelatedLocation) -> Self {
        Self {
            source: SourceOutput::new(&related.source),
            byte_range: ByteRangeOutput::from(related.range),
            range: machine_range(sources, &related.source, related.range),
            message: &related.message,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FixOutput<'a> {
    message: &'a str,
    source: Option<SourceOutput<'a>>,
    byte_range: Option<ByteRangeOutput>,
    range: Option<Utf16RangeOutput>,
    replacement: Option<&'a str>,
}

impl<'a> FixOutput<'a> {
    fn new(sources: &SourceSnapshotRegistry, fix: &'a TextEditSuggestion) -> Self {
        Self {
            message: &fix.message,
            source: fix.source.as_ref().map(SourceOutput::new),
            byte_range: fix.range.map(ByteRangeOutput::from),
            range: fix
                .source
                .as_ref()
                .zip(fix.range)
                .and_then(|(source, range)| machine_range(sources, source, range)),
            replacement: fix.replacement.as_deref(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceOutput<'a> {
    package_id: Option<&'a str>,
    path: &'a str,
}

impl<'a> SourceOutput<'a> {
    fn new(source: &'a SourceIdentity) -> Self {
        Self {
            package_id: source.package_id(),
            path: source.path(),
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct ByteRangeOutput {
    start: u32,
    end: u32,
}

impl From<ByteRange> for ByteRangeOutput {
    fn from(range: ByteRange) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct Utf16PositionOutput {
    line: u32,
    character: u32,
}

#[derive(Clone, Copy, Serialize)]
struct Utf16RangeOutput {
    start: Utf16PositionOutput,
    end: Utf16PositionOutput,
}

impl From<Utf16Range> for Utf16RangeOutput {
    fn from(range: Utf16Range) -> Self {
        Self {
            start: Utf16PositionOutput {
                line: range.start.line,
                character: range.start.character,
            },
            end: Utf16PositionOutput {
                line: range.end.line,
                character: range.end.character,
            },
        }
    }
}

fn machine_range(
    sources: &SourceSnapshotRegistry,
    identity: &SourceIdentity,
    range: ByteRange,
) -> Option<Utf16RangeOutput> {
    sources
        .get(identity)
        .map(|source| Utf16RangeOutput::from(source.utf16_range(range)))
}

#[derive(Debug)]
pub enum RenderError {
    Json(serde_json::Error),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RenderError {}

#[cfg(test)]
mod tests {
    use crate::{
        ByteRange, Diagnostic, DiagnosticBatch, ErrorCode, Label, RelatedLocation, Severity,
        SourceIdentity, SourceSnapshotRegistry, TextEditSuggestion,
    };

    use super::{DiagnosticRenderer, MACHINE_POSITION_ENCODING, RENDER_SCHEMA_VERSION};

    fn batch() -> DiagnosticBatch {
        let primary = SourceIdentity::package("root.app", "src/main.nexa");
        let dependency = SourceIdentity::package("dep.lib", "src/math.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(primary.clone(), "fn main() {\r\n  dep.😀();\r\n}")
            .unwrap();
        sources
            .insert(dependency.clone(), "pub fn 😀() -> i32 { 1 }")
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let start = u32::try_from("fn main() {\r\n  dep.".len()).unwrap();
        let end = start + u32::try_from("😀".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "type mismatch")
                .with_label(Label::primary(
                    primary,
                    ByteRange::new(start, end),
                    "call has the wrong type",
                ))
                .with_related(RelatedLocation::new(
                    dependency,
                    ByteRange::new(7, 11),
                    "callee declaration",
                )),
        );
        batch
    }

    #[test]
    fn human_layout_is_rustc_style_while_machine_protocols_stay_stable() {
        let batch = batch();
        let human = DiagnosticRenderer::human(&batch);
        assert!(human.contains("error[NX2101]: type mismatch"));
        assert!(human.contains("--> root.app:src/main.nexa:2:7"));
        assert!(human.contains("2 |   dep."));
        assert!(human.contains("^ call has the wrong type"));
        assert!(human.contains("related: callee declaration at dep.lib:src/math.nexa:1:8"));
        assert!(human.contains("1 error emitted"));

        let json = DiagnosticRenderer::json(&batch).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["schema"], RENDER_SCHEMA_VERSION);
        assert_eq!(json["positionEncoding"], MACHINE_POSITION_ENCODING);
        assert_eq!(
            json["diagnostics"][0]["labels"][0]["range"]["start"]["line"],
            1
        );
        assert_eq!(
            json["diagnostics"][0]["labels"][0]["range"]["start"]["character"],
            6
        );

        let ndjson = DiagnosticRenderer::ndjson(&batch).unwrap();
        let lines = ndjson.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["schema"], RENDER_SCHEMA_VERSION);
            assert_eq!(value["positionEncoding"], MACHINE_POSITION_ENCODING);
        }
    }

    #[test]
    fn human_layout_renders_multi_label_caret_snapshots_and_summaries() {
        let source = SourceIdentity::package("root.app", "src/main.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(
                source.clone(),
                "fn main() {\n\tlet value = 1 + \"x\";\n\tvalue\n}\n",
            )
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let line_start = u32::try_from("fn main() {\n".len()).unwrap();
        let one_start = line_start + u32::try_from("\tlet value = ".len()).unwrap();
        let string_start = line_start + u32::try_from("\tlet value = 1 + ".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "type mismatch")
                .with_label(Label::primary(
                    source.clone(),
                    ByteRange::new(one_start, one_start + 1),
                    "this expression is not a number",
                ))
                .with_label(Label::secondary(
                    source,
                    ByteRange::new(string_start, string_start + 3),
                    "string literal has type `string`",
                ))
                .with_note("expected `i32`, found `string`"),
        );
        batch.record_suppressed("caused by unknown type `u32`");

        let human = DiagnosticRenderer::human(&batch);
        assert!(human.contains("--> root.app:src/main.nexa:2:14"));
        // The tab expands to four columns, so the caret lands after the expanded gutter.
        assert!(human.contains("2 |     let value = 1 + \"x\";"));
        assert!(human.contains("^ this expression is not a number"));
        assert!(human.contains("--- string literal has type `string`"));
        // Context lines surround the labeled line, with the tabbed line expanded.
        assert!(human.contains("1 | fn main() {"));
        assert!(human.contains("3 |     value"));
        assert!(human.contains("= note: expected `i32`, found `string`"));
        assert!(human.contains(
            "error: 1 error emitted; 1 downstream error suppressed (caused by unknown type `u32`)"
        ));

        let colored = DiagnosticRenderer::human_colored(&batch);
        assert!(colored.contains("\x1b[1;31m"));
        assert!(!colored.contains("Nexa diagnostics schema"));
    }

    #[test]
    fn human_layout_clamps_cross_line_spans_to_the_line_end() {
        let source = SourceIdentity::standalone("multi.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(source.clone(), "first\nsecond line\nthird\n")
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let start = u32::try_from("first\ns".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "cross-line span").with_label(
                Label::primary(
                    source,
                    ByteRange::new(start, start + 20),
                    "spans across lines",
                ),
            ),
        );
        let human = DiagnosticRenderer::human(&batch);
        assert!(human.contains("--> multi.nexa:2:2"));
        assert!(human.contains("2 | second line"));
        assert!(human.contains("spans across lines"));
        assert!(human.contains('^'));
        // The one-line context window is clamped to the file: line 1 before, line 3 after.
        assert!(human.contains("1 | first"));
        assert!(human.contains("3 | third"));
    }

    #[test]
    fn related_locations_keep_semantic_order_in_json() {
        let mut batch = batch();
        let source = SourceIdentity::standalone("extra.nexa");
        batch.push(
            Diagnostic::new(ErrorCode::NX5001, Severity::Error, "trap")
                .with_related(RelatedLocation::new(
                    source.clone(),
                    ByteRange::new(0, 0),
                    "callee",
                ))
                .with_related(RelatedLocation::new(source, ByteRange::new(0, 0), "caller")),
        );
        let json: serde_json::Value =
            serde_json::from_str(&DiagnosticRenderer::json(&batch).unwrap()).unwrap();
        let related = json["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|diagnostic| diagnostic["code"] == "NX5001")
            .unwrap()["related"]
            .as_array()
            .unwrap();
        assert_eq!(related[0]["message"], "callee");
        assert_eq!(related[1]["message"], "caller");
    }

    #[test]
    fn m4_semantic_code_renders_without_losing_cross_file_identity() {
        let usage = SourceIdentity::package("root.app", "src/app/main.nexa");
        let declaration = SourceIdentity::package("dep.lib", "src/internal/types.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(usage.clone(), "pub fn expose(value: internal.Secret) {}")
            .unwrap();
        sources
            .insert(declaration.clone(), "struct Secret {}")
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        batch.push(
            Diagnostic::new(
                ErrorCode::NX2706,
                Severity::Error,
                "public API exposes a private type",
            )
            .with_label(Label::primary(
                usage,
                ByteRange::new(21, 36),
                "private type appears in this public signature",
            ))
            .with_related(RelatedLocation::new(
                declaration,
                ByteRange::new(7, 13),
                "private type is declared here",
            )),
        );

        let json: serde_json::Value =
            serde_json::from_str(&DiagnosticRenderer::json(&batch).unwrap()).unwrap();
        let diagnostic = &json["diagnostics"][0];
        assert_eq!(diagnostic["code"], "NX2706");
        assert_eq!(diagnostic["labels"][0]["source"]["packageId"], "root.app");
        assert_eq!(diagnostic["related"][0]["source"]["packageId"], "dep.lib");
        assert_eq!(
            diagnostic["related"][0]["message"],
            "private type is declared here"
        );
    }

    #[test]
    fn replacement_fix_keeps_standalone_nidl_identity_and_utf16_range_in_json_and_ndjson() {
        let usage = SourceIdentity::package("root.app", "src/main.nexa");
        let contract = SourceIdentity::standalone("/tmp/Host 合同.nidl");
        let usage_text = "fn main() {\r\n    let marker = \"🚀\";\r\n    host::ping();\r\n}\r\n";
        let contract_text = "contract Host {\r\n    host {\r\n        fn ping(message: string) \
                             -> i32;\r\n    }\r\n}\r\n";
        let mut sources = SourceSnapshotRegistry::builder();
        sources.insert(usage.clone(), usage_text).unwrap();
        sources.insert(contract.clone(), contract_text).unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let usage_start = u32::try_from(usage_text.find("host::ping").unwrap()).unwrap();
        let contract_start = u32::try_from(contract_text.find("ping").unwrap()).unwrap();
        let contract_end = contract_start + u32::try_from("ping".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2703, Severity::Error, "unknown Host use path")
                .with_label(Label::primary(
                    usage,
                    ByteRange::new(usage_start, usage_start + 10),
                    "unknown call",
                ))
                .with_related(RelatedLocation::new(
                    contract.clone(),
                    ByteRange::new(contract_start, contract_end),
                    "Host declaration",
                ))
                .with_fix(TextEditSuggestion::replacement(
                    "rename the Host declaration",
                    contract,
                    ByteRange::new(contract_start, contract_end),
                    "pong",
                )),
        );

        let json: serde_json::Value =
            serde_json::from_str(&DiagnosticRenderer::json(&batch).unwrap()).unwrap();
        let fix = &json["diagnostics"][0]["fixes"][0];
        assert_eq!(fix["source"]["packageId"], serde_json::Value::Null);
        assert_eq!(fix["source"]["path"], "/tmp/Host 合同.nidl");
        assert_eq!(fix["byteRange"]["start"], contract_start);
        assert_eq!(fix["byteRange"]["end"], contract_end);
        assert_eq!(fix["range"]["start"]["line"], 2);
        assert_eq!(fix["range"]["start"]["character"], 11);
        assert_eq!(fix["range"]["end"]["line"], 2);
        assert_eq!(fix["range"]["end"]["character"], 15);

        let ndjson = DiagnosticRenderer::ndjson(&batch).unwrap();
        let ndjson_diagnostic: serde_json::Value =
            serde_json::from_str(ndjson.lines().nth(1).unwrap()).unwrap();
        assert_eq!(ndjson_diagnostic["diagnostic"]["fixes"][0], *fix);
    }

    #[test]
    fn full_human_output_matches_the_rustc_style_snapshot() {
        let source = SourceIdentity::package("root.app", "src/main.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(
                source.clone(),
                "fn main() -> i32 {\n    let value = 1 + \"x\";\n    return value;\n}\n",
            )
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let line_start = u32::try_from("fn main() -> i32 {\n".len()).unwrap();
        let one_start = line_start + u32::try_from("    let value = ".len()).unwrap();
        let string_start = line_start + u32::try_from("    let value = 1 + ".len()).unwrap();
        let return_start =
            line_start + u32::try_from("    let value = 1 + \"x\";\n    ".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2101, Severity::Error, "type mismatch")
                .with_label(Label::primary(
                    source.clone(),
                    ByteRange::new(one_start, one_start + 1),
                    "this expression is not a number",
                ))
                .with_label(Label::secondary(
                    source.clone(),
                    ByteRange::new(string_start, string_start + 3),
                    "string literal has type `string`",
                ))
                .with_note("expected `i32`, found `string`")
                .with_fix(TextEditSuggestion::message("write `value` as an integer")),
        );
        batch.push(
            Diagnostic::new(ErrorCode::NX1002, Severity::Error, "expected `;`").with_label(
                Label::primary(
                    source,
                    ByteRange::new(return_start, return_start + 6),
                    "invalid Nexa syntax",
                ),
            ),
        );
        batch.record_suppressed("caused by unknown type `u32`");

        let human = DiagnosticRenderer::human(&batch);
        let expected = concat!(
            "error[NX2101]: type mismatch\n",
            "  --> root.app:src/main.nexa:2:17\n",
            "  |\n",
            "1 | fn main() -> i32 {\n",
            "2 |     let value = 1 + \"x\";\n",
            "  |                  ^ this expression is not a number\n",
            "3 |     return value;\n",
            "  --> root.app:src/main.nexa:2:21\n",
            "  |\n",
            "1 | fn main() -> i32 {\n",
            "2 |     let value = 1 + \"x\";\n",
            "  |                      --- string literal has type `string`\n",
            "3 |     return value;\n",
            "   = note: expected `i32`, found `string`\n",
            "   = help: write `value` as an integer\n",
            "\n",
            "error[NX1002]: expected `;`\n",
            "  --> root.app:src/main.nexa:3:5\n",
            "  |\n",
            "2 |     let value = 1 + \"x\";\n",
            "3 |     return value;\n",
            "  |      ^^^^^^ invalid Nexa syntax\n",
            "4 | }\n",
            "error: 2 errors emitted; 1 downstream error suppressed (caused by unknown type `u32`)\n",
        );
        assert_eq!(human, expected);
    }

    #[test]
    fn human_layout_shows_escaped_replacement_text() {
        let source = SourceIdentity::standalone("fix.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(source.clone(), "let marker = \"🚀\";\n")
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let marker = u32::try_from("let marker = ".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX1001, Severity::Error, "rewrite marker")
                .with_fix(TextEditSuggestion::replacement(
                    "normalize the marker literal",
                    source.clone(),
                    ByteRange::new(marker, marker + u32::try_from("\"🚀\"".len()).unwrap()),
                    "let marker = {\n\t\"a\tb\"\r\n};\n\u{1}",
                ))
                .with_fix(TextEditSuggestion::replacement(
                    "remove the marker",
                    source,
                    ByteRange::new(marker, marker + 1),
                    "",
                )),
        );

        let human = DiagnosticRenderer::human(&batch);
        assert!(human.contains("= help: normalize the marker literal"));
        assert!(
            human.contains("= help: replace with `let marker = {\\n\\t\"a\\tb\"\\r\\n};\\n\\x01`")
        );
        assert!(human.contains("= help: remove the marker"));
        assert!(human.contains("= help: replace with ``"));
    }

    #[test]
    fn human_layout_clamps_context_lines_to_file_bounds() {
        let source = SourceIdentity::standalone("bounds.nexa");
        let mut sources = SourceSnapshotRegistry::builder();
        sources
            .insert(source.clone(), "first\nmiddle\nlast\n")
            .unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let last = u32::try_from("first\nmiddle\n".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX1001, Severity::Error, "bound labels")
                .with_label(Label::primary(
                    source.clone(),
                    ByteRange::new(0, 1),
                    "first line",
                ))
                .with_label(Label::secondary(
                    source,
                    ByteRange::new(last, last + 4),
                    "last line",
                )),
        );

        let human = DiagnosticRenderer::human(&batch);
        // Line 1 has no preceding context line; line 3 has no following one (the trailing
        // newline's empty phantom line is not rendered as context).
        assert!(human.contains("\n  |\n1 | first\n  |  ^ first line\n2 | middle\n"));
        assert!(human.contains("\n  |\n2 | middle\n3 | last\n  |  --- last line\n"));
        assert!(!human.contains("0 |"));
        assert!(!human.contains("4 |"));
    }
}
