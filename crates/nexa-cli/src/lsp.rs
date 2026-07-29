use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use nexa_embed::{
    EngineDiagnostic, EngineDiagnosticStage, PackageCandidate, SourceFileRegistry, SourceId,
};
use serde_json::{Value, json};

use crate::project;

#[derive(Clone)]
struct OpenDocument {
    text: String,
}

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    run_session(&mut reader, &mut writer)
}

fn run_session(reader: &mut impl BufRead, writer: &mut impl Write) -> Result<(), String> {
    let mut documents = BTreeMap::<String, OpenDocument>::new();
    let mut shutdown = false;
    while let Some(message) = read_message(reader)? {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        match method {
            Some("initialize") => {
                respond(
                    writer,
                    id,
                    &json!({
                        "capabilities": {
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 1,
                                "save": {"includeText": true}
                            }
                        },
                        "serverInfo": {
                            "name": "nexa",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                )?;
            }
            Some("initialized") => {}
            Some("textDocument/didOpen") => {
                let params = &message["params"]["textDocument"];
                let uri = required_str(params, "uri")?.to_owned();
                let text = required_str(params, "text")?.to_owned();
                documents.insert(uri.clone(), OpenDocument { text });
                publish_for(
                    writer,
                    &uri,
                    documents.get(&uri).map(|doc| doc.text.as_str()),
                )?;
            }
            Some("textDocument/didChange") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                let text = message["params"]["contentChanges"]
                    .as_array()
                    .and_then(|changes| changes.last())
                    .and_then(|change| change.get("text"))
                    .and_then(Value::as_str)
                    .ok_or("didChange has no full document text")?
                    .to_owned();
                documents.insert(uri.clone(), OpenDocument { text });
                publish_for(
                    writer,
                    &uri,
                    documents.get(&uri).map(|doc| doc.text.as_str()),
                )?;
            }
            Some("textDocument/didSave") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                if let Some(text) = message["params"].get("text").and_then(Value::as_str) {
                    documents.insert(
                        uri.clone(),
                        OpenDocument {
                            text: text.to_owned(),
                        },
                    );
                }
                publish_for(
                    writer,
                    &uri,
                    documents.get(&uri).map(|doc| doc.text.as_str()),
                )?;
            }
            Some("textDocument/didClose") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                documents.remove(&uri);
                let path = file_uri_to_path(&uri)?;
                if path.is_file() {
                    publish_for(writer, &uri, None)?;
                } else {
                    notify(
                        writer,
                        "textDocument/publishDiagnostics",
                        &json!({"uri": uri, "diagnostics": []}),
                    )?;
                }
            }
            Some("shutdown") => {
                shutdown = true;
                respond(writer, id, &Value::Null)?;
            }
            Some("exit") => {
                if !shutdown {
                    return Err("LSP client exited without shutdown".into());
                }
                break;
            }
            Some(method) if id.is_some() => {
                respond_error(writer, id, -32601, &format!("unsupported method {method}"))?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn publish_for(writer: &mut impl Write, uri: &str, overlay: Option<&str>) -> Result<(), String> {
    let path = file_uri_to_path(uri)?;
    let diagnostics = diagnostics_for_path(&path, overlay)?;
    let diagnostic_root = find_upward(&path, "package.toml")
        .and_then(|manifest| manifest.parent().map(Path::to_path_buf))
        .or_else(|| path.parent().map(Path::to_path_buf));
    let diagnostics = diagnostics
        .iter()
        .map(|diagnostic| lsp_diagnostic(diagnostic, diagnostic_root.as_deref()))
        .collect::<Vec<_>>();
    notify(
        writer,
        "textDocument/publishDiagnostics",
        &json!({"uri": uri, "diagnostics": diagnostics}),
    )
}

fn diagnostics_for_path(
    path: &Path,
    overlay: Option<&str>,
) -> Result<Vec<EngineDiagnostic>, String> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("nidl") => {
            let source = overlay.map_or_else(
                || {
                    std::fs::read_to_string(path)
                        .map_err(|error| format!("could not read {}: {error}", path.display()))
                },
                |source| Ok(source.to_owned()),
            )?;
            match nexa_idl::parse(&source) {
                Ok(_) => Ok(Vec::new()),
                Err(error) => {
                    let registry = SourceFileRegistry::from_files([(
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("contract.nidl"),
                        source,
                    )])
                    .map_err(|error| error.to_string())?;
                    let file = registry.files().next().cloned();
                    let mut diagnostic = EngineDiagnostic::without_source(
                        None,
                        None,
                        EngineDiagnosticStage::Parse,
                        nexa::ErrorCode::NX1002,
                        error.to_string(),
                    );
                    diagnostic.file = file;
                    Ok(vec![diagnostic])
                }
            }
        }
        Some("nexa") => diagnostics_for_nexa(path, overlay),
        _ => Ok(Vec::new()),
    }
}

fn diagnostics_for_nexa(
    path: &Path,
    overlay: Option<&str>,
) -> Result<Vec<EngineDiagnostic>, String> {
    if let Some(config) = find_upward(path, "nexa.dev.toml") {
        let project = project::LoadedProject::load(&config)?;
        if let Some(package) = find_upward(path, "package.toml")
            .and_then(|manifest| manifest.parent().map(Path::to_path_buf))
        {
            let (source_id, mut candidate) = match project::load_package_candidate(&package) {
                Ok(candidate) => candidate,
                Err(diagnostic) => return Ok(vec![diagnostic]),
            };
            if let Some(overlay) = overlay {
                let entry = package
                    .join(candidate.manifest.entry.as_path())
                    .canonicalize()
                    .ok();
                if entry.as_deref() == path.canonicalize().ok().as_deref() {
                    candidate = PackageCandidate::new(
                        candidate.manifest,
                        candidate.manifest_source,
                        overlay.to_owned(),
                    );
                }
            }
            return match nexa_embed::compile_package(
                &project.idl,
                &project.required_exports,
                &source_id,
                &candidate,
            ) {
                Ok(_) => Ok(Vec::new()),
                Err(diagnostic) => Ok(vec![diagnostic]),
            };
        }
    }
    let source = overlay.map_or_else(
        || {
            std::fs::read_to_string(path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))
        },
        |source| Ok(source.to_owned()),
    )?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("main.nexa");
    let registry = SourceFileRegistry::from_files([(file_name, source.clone())])
        .map_err(|error| error.to_string())?;
    let file_id = registry
        .file_id(file_name)
        .ok_or("overlay file was not registered")?;
    match nexa::compile_file(&source, file_id) {
        Ok(_) => Ok(Vec::new()),
        Err(nexa::NexaError::Diagnostic(diagnostic)) => Ok(vec![EngineDiagnostic::from_leaf(
            None,
            SourceId::new("editor").ok(),
            EngineDiagnosticStage::Compile,
            *diagnostic,
            Some(&registry),
        )]),
        Err(error) => Ok(vec![EngineDiagnostic::without_source(
            None,
            SourceId::new("editor").ok(),
            EngineDiagnosticStage::Verify,
            error.code(),
            error.to_string(),
        )]),
    }
}

fn lsp_diagnostic(diagnostic: &EngineDiagnostic, diagnostic_root: Option<&Path>) -> Value {
    let range = diagnostic
        .diagnostic
        .primary
        .as_ref()
        .zip(diagnostic.file.as_ref())
        .map_or_else(
            || json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}),
            |(label, file)| {
                let start = file.lsp_position(label.span.start as usize);
                let end = file.lsp_position(label.span.end as usize);
                json!({
                    "start": {"line": start.line, "character": start.character},
                    "end": {"line": end.line, "character": end.character}
                })
            },
        );
    let related_information = diagnostic
        .related
        .iter()
        .filter_map(|related| {
            let file = related.file.as_ref()?;
            let span = related.span?;
            let start = file.lsp_position(span.start as usize);
            let end = file.lsp_position(span.end as usize);
            let path = Path::new(&file.path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                diagnostic_root?.join(path)
            };
            Some(json!({
                "location": {
                    "uri": path_to_file_uri(&path),
                    "range": {
                        "start": {"line": start.line, "character": start.character},
                        "end": {"line": end.line, "character": end.character}
                    }
                },
                "message": related.message
            }))
        })
        .collect::<Vec<_>>();
    json!({
        "range": range,
        "severity": 1,
        "code": diagnostic.diagnostic.code.as_str(),
        "codeDescription": {
            "href": format!("https://github.com/S1RANN/nexa/search?q={}", diagnostic.diagnostic.code.as_str())
        },
        "source": "nexa",
        "message": diagnostic.diagnostic.message.to_string(),
        "relatedInformation": related_information
    })
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("LSP header read failed: {error}"))?;
        if read == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| format!("invalid Content-Length: {error}"))?,
            );
        }
    }
    let length = content_length.ok_or("LSP message has no Content-Length")?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("LSP body read failed: {error}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| format!("invalid LSP JSON: {error}"))
}

fn respond(writer: &mut impl Write, id: Option<&Value>, result: &Value) -> Result<(), String> {
    write_message(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn respond_error(
    writer: &mut impl Write,
    id: Option<&Value>,
    code: i32,
    message: &str,
) -> Result<(), String> {
    write_message(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

fn notify(writer: &mut impl Write, method: &str, params: &Value) -> Result<(), String> {
    write_message(
        writer,
        &json!({"jsonrpc": "2.0", "method": method, "params": params}),
    )
}

fn write_message(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())
        .and_then(|()| writer.write_all(&body))
        .and_then(|()| writer.flush())
        .map_err(|error| format!("LSP response write failed: {error}"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field `{key}`"))
}

fn find_upward(path: &Path, name: &str) -> Option<PathBuf> {
    let mut directory = path.parent()?;
    loop {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        directory = directory.parent()?;
    }
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let encoded = uri
        .strip_prefix("file://")
        .ok_or_else(|| format!("unsupported document URI: {uri}"))?;
    let mut decoded = Vec::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = hex(bytes[index + 1]).ok_or("invalid URI escape")?;
            let low = hex(bytes[index + 2]).ok_or("invalid URI escape")?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map(PathBuf::from)
        .map_err(|error| format!("document URI is not UTF-8: {error}"))
}

fn path_to_file_uri(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    format!("file://{}", rendered.replace(' ', "%20"))
}

const fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;

    use serde_json::json;

    fn framed(value: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("message JSON");
        let mut message = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        message.extend(body);
        message
    }

    #[test]
    fn lsp_protocol_reads_framed_messages() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut reader = Cursor::new(framed.into_bytes());
        let message = super::read_message(&mut reader)
            .expect("framed message")
            .expect("message");
        assert_eq!(message["method"], "initialize");
    }

    #[test]
    fn lsp_utf16_range_handles_unicode_before_error() {
        let source = "fn Value() -> i32 {\n    let label = \"界\";\n    return label;\n}";
        let diagnostics =
            super::diagnostics_for_path(Path::new("/tmp/nexa-lsp-overlay.nexa"), Some(source))
                .expect("diagnostics");
        assert_eq!(diagnostics.len(), 1);
        let rendered = super::lsp_diagnostic(&diagnostics[0], Some(Path::new("/tmp")));
        assert!(rendered["range"]["start"]["character"].is_u64());
        assert_eq!(rendered["code"].as_str(), Some("NX2101"));
    }

    #[test]
    fn lsp_idl_diagnostics_clear_after_valid_overlay() {
        let path = Path::new("/tmp/nexa-lsp-overlay.nidl");
        assert_eq!(
            super::diagnostics_for_path(path, Some("interface Broken {"))
                .expect("invalid IDL")
                .len(),
            1
        );
        assert!(
            super::diagnostics_for_path(path, Some("interface Valid {}"))
                .expect("valid IDL")
                .is_empty()
        );
    }

    #[test]
    fn lsp_session_publishes_and_clears_overlay_diagnostics() {
        let uri = "file:///tmp/nexa-lsp-session.nexa";
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didOpen",
                "params":{"textDocument":{
                    "uri":uri,
                    "languageId":"nexa",
                    "version":1,
                    "text":"fn Value() -> i32 { return \"界\"; }"
                }}
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didChange",
                "params":{
                    "textDocument":{"uri":uri,"version":2},
                    "contentChanges":[{"text":"fn Value() -> i32 { return 1; }"}]
                }
            }),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        super::run_session(&mut reader, &mut output).expect("LSP session");
        let output = String::from_utf8(output).expect("UTF-8 output");
        assert!(output.contains("\"code\":\"NX2101\""));
        assert!(output.contains("\"diagnostics\":[]"));
        assert!(output.contains("\"textDocumentSync\""));
    }
}
