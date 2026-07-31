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

impl DiagnosticRenderer {
    #[must_use]
    pub fn human(batch: &DiagnosticBatch) -> String {
        let mut output = format!(
            "Nexa diagnostics schema {RENDER_SCHEMA_VERSION} \
             (positions: {HUMAN_POSITION_ENCODING})"
        );
        for diagnostic in batch.diagnostics() {
            output.push('\n');
            render_human_diagnostic(&mut output, batch.sources(), diagnostic);
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
) {
    write!(
        output,
        "{}[{}]: {}",
        diagnostic.severity.as_str(),
        diagnostic.code,
        diagnostic.message
    )
    .expect("writing to String cannot fail");
    for label in &diagnostic.labels {
        let prefix = match label.style {
            LabelStyle::Primary => "-->",
            LabelStyle::Secondary => ":::",
        };
        if let Some(snapshot) = sources.get(&label.source) {
            let range = snapshot.human_range(label.range);
            write!(
                output,
                "\n{prefix} {}:{}:{}: {}",
                label.source, range.start.line, range.start.column, label.message
            )
            .expect("writing to String cannot fail");
            if let Some(line) = snapshot.line_text(range.start.line) {
                write!(output, "\n    {line}").expect("writing to String cannot fail");
            }
        } else {
            write!(
                output,
                "\n{prefix} {}:<source unavailable>: {}",
                label.source, label.message
            )
            .expect("writing to String cannot fail");
        }
    }
    for related in &diagnostic.related {
        render_human_related(output, sources, related);
    }
    for note in &diagnostic.notes {
        write!(output, "\nnote: {note}").expect("writing to String cannot fail");
    }
    for fix in &diagnostic.fixes {
        write!(output, "\nhelp: {}", fix.message).expect("writing to String cannot fail");
    }
}

fn render_human_related(
    output: &mut String,
    sources: &SourceSnapshotRegistry,
    related: &RelatedLocation,
) {
    if let Some(snapshot) = sources.get(&related.source) {
        let range = snapshot.human_range(related.range);
        write!(
            output,
            "\nrelated: {} at {}:{}:{}",
            related.message, related.source, range.start.line, range.start.column
        )
        .expect("writing to String cannot fail");
    } else {
        write!(
            output,
            "\nrelated: {} at {}:<source unavailable>",
            related.message, related.source
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

    use super::{
        DiagnosticRenderer, HUMAN_POSITION_ENCODING, MACHINE_POSITION_ENCODING,
        RENDER_SCHEMA_VERSION,
    };

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
    fn renderers_declare_schema_and_position_encoding() {
        let batch = batch();
        let human = DiagnosticRenderer::human(&batch);
        assert!(human.contains(&format!("schema {RENDER_SCHEMA_VERSION}")));
        assert!(human.contains(HUMAN_POSITION_ENCODING));
        assert!(human.contains("src/main.nexa:2:7"));

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
        let usage_text = "fn main() {\r\n    let marker = \"🚀\";\r\n    Host.ping();\r\n}\r\n";
        let contract_text =
            "interface Host {\r\n    sync fn ping(message: string) -> i32;\r\n}\r\n";
        let mut sources = SourceSnapshotRegistry::builder();
        sources.insert(usage.clone(), usage_text).unwrap();
        sources.insert(contract.clone(), contract_text).unwrap();
        let mut batch = DiagnosticBatch::with_default_limits(sources.build());
        let usage_start = u32::try_from(usage_text.find("Host.ping").unwrap()).unwrap();
        let contract_start = u32::try_from(contract_text.find("ping").unwrap()).unwrap();
        let contract_end = contract_start + u32::try_from("ping".len()).unwrap();
        batch.push(
            Diagnostic::new(ErrorCode::NX2703, Severity::Error, "unknown Host import")
                .with_label(Label::primary(
                    usage,
                    ByteRange::new(usage_start, usage_start + 9),
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
        assert_eq!(fix["range"]["start"]["line"], 1);
        assert_eq!(fix["range"]["start"]["character"], 12);
        assert_eq!(fix["range"]["end"]["line"], 1);
        assert_eq!(fix["range"]["end"]["character"], 16);

        let ndjson = DiagnosticRenderer::ndjson(&batch).unwrap();
        let ndjson_diagnostic: serde_json::Value =
            serde_json::from_str(ndjson.lines().nth(1).unwrap()).unwrap();
        assert_eq!(ndjson_diagnostic["diagnostic"]["fixes"][0], *fix);
    }
}
