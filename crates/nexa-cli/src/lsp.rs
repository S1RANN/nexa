use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nexa_embed::{EngineDiagnostic, EngineDiagnosticStage, SourceId};
use serde_json::{Value, json};
use url::Url;

use crate::project;

#[path = "lsp_language_v3.rs"]
mod language_v3;
use language_v3::{
    completion_items as language_v3_completion_items,
    completion_items_for_owner as language_v3_completion_items_for_owner,
    completion_receiver_range as language_v3_completion_receiver_range, hover as language_v3_hover,
    semantic_tokens as language_v3_semantic_tokens,
};

#[derive(Clone)]
struct OpenDocument {
    uri: String,
    path: PathBuf,
    text: String,
    version: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildInputKind {
    NexaSource,
    ContractSource,
    /// A legacy `*.nidl` file. It is never parsed by the Contract parser (no compatibility
    /// parsing); instead it receives an actionable migration diagnostic pointing at the new
    /// `*.contract.nexa` file name and flat `contract <Name>;` header.
    LegacyNidl,
    PackageManifest,
    Lockfile,
    WorkspaceManifest,
}

impl BuildInputKind {
    fn for_path(path: &Path) -> Option<Self> {
        if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
            == nexa_syntax::SourceProfile::Contract
        {
            return Some(Self::ContractSource);
        }
        match path.file_name().and_then(|name| name.to_str()) {
            Some("package.toml") => Some(Self::PackageManifest),
            Some("nexa.lock") => Some(Self::Lockfile),
            Some("nexa.dev.toml") => Some(Self::WorkspaceManifest),
            _ => match path.extension().and_then(|extension| extension.to_str()) {
                Some("nexa") => Some(Self::NexaSource),
                Some("nidl") => Some(Self::LegacyNidl),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Opened,
    Changed,
    Saved,
    Closed,
    CreatedOnDisk,
    ChangedOnDisk,
    DeletedFromDisk,
    WorkspaceRemoved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BuildInputChange {
    path: PathBuf,
    input: BuildInputKind,
    change: ChangeKind,
}

impl BuildInputChange {
    fn document(path: &Path, change: ChangeKind) -> Option<Self> {
        let path = lexical_path(path);
        Some(Self {
            input: BuildInputKind::for_path(&path)?,
            path,
            change,
        })
    }

    fn watched(path: &Path, change_type: Option<i64>) -> Option<Self> {
        let change = match change_type {
            Some(1) => ChangeKind::CreatedOnDisk,
            Some(3) => ChangeKind::DeletedFromDisk,
            _ => ChangeKind::ChangedOnDisk,
        };
        Self::document(path, change)
    }
}

/// Immutable editor state passed to a package/workspace build.
///
/// The current adapter rebuilds the shared `ResolvedBuild` input with every relevant editor
/// overlay. This boundary stays independent of that concrete driver so a persistent/incremental
/// `nexa-analysis` database can consume the same complete snapshot later.
struct WorkspaceSnapshot<'a> {
    roots: &'a [PathBuf],
    documents: &'a BTreeMap<String, OpenDocument>,
    known_inputs: &'a BTreeSet<PathBuf>,
}

impl WorkspaceSnapshot<'_> {
    fn overlay_for_path(&self, path: &Path) -> Option<&str> {
        self.document_for_path(path)
            .map(|document| document.text.as_str())
    }

    fn document_for_path(&self, path: &Path) -> Option<&OpenDocument> {
        self.documents
            .values()
            .find(|document| same_file_path(&document.path, path))
    }

    fn text_for_path(&self, path: &Path) -> Result<Option<String>, String> {
        if let Some(overlay) = self.overlay_for_path(path) {
            return Ok(Some(overlay.to_owned()));
        }
        match std::fs::read_to_string(path) {
            Ok(source) => Ok(Some(source)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }

    fn contains_path(&self, path: &Path) -> bool {
        self.overlay_for_path(path).is_some() || path.is_file()
    }

    fn uri_for_path(&self, path: &Path) -> Result<String, String> {
        self.document_for_path(path)
            .map(|document| document.uri.clone())
            .map_or_else(|| path_to_file_uri(path), Ok)
    }

    /// Finds an open document whose path ends with the identity-relative `relative` path. This
    /// is the editor-snapshot fallback for related locations that carry only a standalone
    /// relative path (for example the Host Contract `api.nidl` spelled as a bare file name): the
    /// authoritative URI is the open document's URI, not a guess rooted at the diagnostic root.
    fn document_for_relative_path(&self, relative: &Path) -> Option<&OpenDocument> {
        let relative_components = relative.components().collect::<Vec<_>>();
        if relative_components.is_empty() {
            return None;
        }
        self.documents.values().find(|document| {
            let lexical = lexical_path(&document.path);
            let document_components = lexical.components().collect::<Vec<_>>();
            document_components.len() >= relative_components.len()
                && document_components[document_components.len() - relative_components.len()..]
                    == relative_components[..]
        })
    }

    fn version_for_uri(&self, uri: &str) -> Option<i64> {
        self.documents
            .get(uri)
            .map(|document| document.version)
            .or_else(|| {
                let path = file_uri_to_path(uri).ok()?;
                self.document_for_path(&path)
                    .map(|document| document.version)
            })
    }
}

#[derive(Clone)]
struct LocatedDiagnostic {
    diagnostic: EngineDiagnostic,
    root: PathBuf,
    fallback_path: PathBuf,
    source_paths: Arc<BTreeMap<nexa::SourceIdentity, PathBuf>>,
}

impl LocatedDiagnostic {
    fn new(diagnostic: EngineDiagnostic, root: PathBuf, fallback_path: PathBuf) -> Self {
        Self {
            diagnostic,
            root,
            fallback_path,
            source_paths: Arc::new(BTreeMap::new()),
        }
    }

    fn source_path(&self) -> PathBuf {
        self.diagnostic.file.as_ref().map_or_else(
            || self.fallback_path.clone(),
            |file| {
                if let Some(path) = self.source_paths.get(file) {
                    return path.clone();
                }
                if file.package_id().is_some() {
                    return self.fallback_path.clone();
                }
                let path = Path::new(file.path());
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.root.join(path)
                }
            },
        )
    }
}

#[derive(Default)]
struct AnalysisReport {
    checked_paths: BTreeSet<PathBuf>,
    diagnostics: Vec<LocatedDiagnostic>,
}

#[derive(Default)]
struct PublishedDiagnostics {
    diagnostic_uris: BTreeMap<PathBuf, String>,
    preferred_uris: BTreeMap<PathBuf, String>,
}

/// Adapter seam between LSP document management and the authoritative package/workspace build.
trait WorkspaceAnalyzer {
    fn analyze(
        &mut self,
        snapshot: &WorkspaceSnapshot<'_>,
        changes: &[BuildInputChange],
    ) -> Result<AnalysisReport, String>;

    fn semantic_for_path(&self, _path: &Path) -> Option<SemanticDocument> {
        None
    }
}

#[derive(Default)]
struct CurrentWorkspaceAnalyzer {
    package_sessions: BTreeMap<PathBuf, PackageAnalysisSession>,
    standalone_sessions: BTreeMap<PathBuf, PackageAnalysisSession>,
}

#[derive(Default)]
struct PackageAnalysisSession {
    package_id: Option<nexa_analysis::PackageId>,
    build_fingerprint: Option<nexa_analysis::BuildFingerprint>,
    generation: u64,
    build: nexa::PackageBuildSession,
    semantic: Option<SemanticSnapshot>,
}

#[derive(Clone)]
struct SemanticDocument {
    ir: Arc<nexa_analysis::TypedPackageIr>,
    source: nexa_analysis::SourceKey,
    paths_by_source: Arc<BTreeMap<nexa_analysis::SourceKey, PathBuf>>,
}

#[derive(Clone)]
struct SemanticSnapshot {
    ir: Arc<nexa_analysis::TypedPackageIr>,
    sources_by_path: BTreeMap<PathBuf, nexa_analysis::SourceKey>,
    paths_by_source: Arc<BTreeMap<nexa_analysis::SourceKey, PathBuf>>,
}

impl SemanticSnapshot {
    fn from_build(build: &project::ResolvedBuild, ir: Arc<nexa_analysis::TypedPackageIr>) -> Self {
        let mut sources_by_path = BTreeMap::new();
        let mut paths_by_source = BTreeMap::new();
        for package in build.packages.values() {
            for source_set in [&package.production_sources, &package.test_sources] {
                for key in source_set.units().keys() {
                    let path = lexical_path(&package.directory.join(key.path.as_path()));
                    sources_by_path.insert(path.clone(), key.clone());
                    paths_by_source.insert(key.clone(), path);
                }
            }
        }
        if let Some(origin) = &build.virtual_source_origin {
            let path = lexical_path(Path::new(origin.display_identity.path()));
            sources_by_path.insert(path.clone(), origin.source_key.clone());
            paths_by_source.insert(origin.source_key.clone(), path);
        }
        Self {
            ir,
            sources_by_path,
            paths_by_source: Arc::new(paths_by_source),
        }
    }

    fn document(&self, path: &Path) -> Option<SemanticDocument> {
        let source = self.sources_by_path.get(&lexical_path(path))?.clone();
        Some(SemanticDocument {
            ir: Arc::clone(&self.ir),
            source,
            paths_by_source: Arc::clone(&self.paths_by_source),
        })
    }
}

impl PackageAnalysisSession {
    fn prepare(&mut self, build: &project::ResolvedBuild) -> u64 {
        if self.package_id.as_ref() != Some(build.package_id()) {
            *self = Self {
                package_id: Some(build.package_id().clone()),
                build_fingerprint: None,
                generation: 0,
                build: nexa::PackageBuildSession::new(),
                semantic: None,
            };
        }
        if self.build_fingerprint != Some(build.build_fingerprint) {
            self.build_fingerprint = Some(build.build_fingerprint);
            self.generation = self.generation.saturating_add(1).max(1);
            self.semantic = None;
        }
        self.generation.max(1)
    }
}

fn document_key_for_path(
    documents: &BTreeMap<String, OpenDocument>,
    uri: &str,
    path: &Path,
) -> Option<String> {
    if documents.contains_key(uri) {
        return Some(uri.to_owned());
    }
    documents
        .iter()
        .find_map(|(key, document)| same_file_path(&document.path, path).then(|| key.clone()))
}

fn take_document_for_path(
    documents: &mut BTreeMap<String, OpenDocument>,
    uri: &str,
    path: &Path,
) -> Option<OpenDocument> {
    let key = document_key_for_path(documents, uri, path)?;
    documents.remove(&key)
}

fn insert_known_path(known_inputs: &mut BTreeSet<PathBuf>, path: PathBuf) {
    known_inputs.retain(|known| !same_file_path(known, &path));
    known_inputs.insert(path);
}

pub fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    run_session(&mut reader, &mut writer)
}

#[allow(clippy::too_many_lines)]
fn run_session(reader: &mut impl BufRead, writer: &mut impl Write) -> Result<(), String> {
    let mut analyzer = CurrentWorkspaceAnalyzer::default();
    run_session_with_analyzer(reader, writer, &mut analyzer)
}

#[allow(clippy::too_many_lines)]
fn run_session_with_analyzer(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    analyzer: &mut impl WorkspaceAnalyzer,
) -> Result<(), String> {
    let mut documents = BTreeMap::<String, OpenDocument>::new();
    let mut known_inputs = BTreeSet::<PathBuf>::new();
    let mut workspace_roots = Vec::<PathBuf>::new();
    let mut published_diagnostics = PublishedDiagnostics::default();
    let mut shutdown = false;
    while let Some(message) = read_message(reader)? {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        match method {
            Some("initialize") => {
                workspace_roots = workspace_roots_from_initialize(&message["params"]);
                respond(
                    writer,
                    id,
                    &json!({
                        "capabilities": {
                            "positionEncoding": "utf-16",
                            "textDocumentSync": {
                                "openClose": true,
                                "change": 1,
                                "save": {"includeText": true}
                            },
                            "completionProvider": {
                                "triggerCharacters": [".", ":"]
                            },
                            "hoverProvider": true,
                            "definitionProvider": true,
                            "semanticTokensProvider": {
                                "legend": {
                                    "tokenTypes": [
                                        "keyword",
                                        "type",
                                        "function",
                                        "variable",
                                        "string",
                                        "number",
                                        "comment",
                                        "operator"
                                    ],
                                    "tokenModifiers": []
                                },
                                "full": true
                            },
                            "documentSymbolProvider": true,
                            "workspace": {
                                "workspaceFolders": {
                                    "supported": true,
                                    "changeNotifications": true
                                }
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
                let version = params.get("version").and_then(Value::as_i64).unwrap_or(0);
                let path = lexical_path(&file_uri_to_path(&uri)?);
                let Some(change) = BuildInputChange::document(&path, ChangeKind::Opened) else {
                    continue;
                };
                take_document_for_path(&mut documents, &uri, &path);
                insert_known_path(&mut known_inputs, path.clone());
                documents.insert(
                    uri.clone(),
                    OpenDocument {
                        uri,
                        path,
                        text,
                        version,
                    },
                );
                analyze_and_publish(
                    writer,
                    analyzer,
                    &workspace_roots,
                    &documents,
                    &known_inputs,
                    &[change],
                    &mut published_diagnostics,
                )?;
            }
            Some("textDocument/didChange") => {
                let document = &message["params"]["textDocument"];
                let uri = required_str(document, "uri")?.to_owned();
                let requested_path = lexical_path(&file_uri_to_path(&uri)?);
                let version = document
                    .get("version")
                    .and_then(Value::as_i64)
                    .ok_or("didChange has no document version")?;
                let document_key = document_key_for_path(&documents, &uri, &requested_path);
                if document_key
                    .as_ref()
                    .and_then(|key| documents.get(key))
                    .is_some_and(|document| version <= document.version)
                {
                    continue;
                }
                let text = message["params"]["contentChanges"]
                    .as_array()
                    .and_then(|changes| changes.last())
                    .and_then(|change| change.get("text"))
                    .and_then(Value::as_str)
                    .ok_or("didChange has no full document text")?
                    .to_owned();
                let path = document_key
                    .as_ref()
                    .and_then(|key| documents.get(key))
                    .map(|document| document.path.clone())
                    .unwrap_or(requested_path);
                let Some(change) = BuildInputChange::document(&path, ChangeKind::Changed) else {
                    continue;
                };
                if let Some(key) = document_key {
                    documents.remove(&key);
                }
                insert_known_path(&mut known_inputs, path.clone());
                documents.insert(
                    uri.clone(),
                    OpenDocument {
                        uri,
                        path,
                        text,
                        version,
                    },
                );
                analyze_and_publish(
                    writer,
                    analyzer,
                    &workspace_roots,
                    &documents,
                    &known_inputs,
                    &[change],
                    &mut published_diagnostics,
                )?;
            }
            Some("textDocument/didSave") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                let requested_path = lexical_path(&file_uri_to_path(&uri)?);
                let open_document = take_document_for_path(&mut documents, &uri, &requested_path);
                let path = open_document
                    .as_ref()
                    .map_or_else(|| requested_path.clone(), |document| document.path.clone());
                if let Some(mut document) = open_document {
                    if let Some(text) = message["params"].get("text").and_then(Value::as_str) {
                        text.clone_into(&mut document.text);
                    }
                    document.uri.clone_from(&uri);
                    documents.insert(uri.clone(), document);
                }
                let Some(change) = BuildInputChange::document(&path, ChangeKind::Saved) else {
                    continue;
                };
                insert_known_path(&mut known_inputs, path);
                analyze_and_publish(
                    writer,
                    analyzer,
                    &workspace_roots,
                    &documents,
                    &known_inputs,
                    &[change],
                    &mut published_diagnostics,
                )?;
            }
            Some("textDocument/didClose") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                let requested_path = lexical_path(&file_uri_to_path(&uri)?);
                let path = take_document_for_path(&mut documents, &uri, &requested_path)
                    .map_or(requested_path, |document| document.path);
                let Some(change) = BuildInputChange::document(&path, ChangeKind::Closed) else {
                    continue;
                };
                if path.exists() {
                    insert_known_path(&mut known_inputs, path);
                } else {
                    known_inputs.retain(|known| !same_file_path(known, &path));
                }
                analyze_and_publish(
                    writer,
                    analyzer,
                    &workspace_roots,
                    &documents,
                    &known_inputs,
                    &[change],
                    &mut published_diagnostics,
                )?;
            }
            Some("textDocument/documentSymbol") => {
                let uri = required_str(&message["params"]["textDocument"], "uri")?.to_owned();
                let requested_path = lexical_path(&file_uri_to_path(&uri)?);
                let path = document_key_for_path(&documents, &uri, &requested_path)
                    .and_then(|key| documents.get(&key))
                    .map_or(requested_path, |document| document.path.clone());
                let source = match documents.get(&uri).map(|document| document.text.clone()) {
                    Some(opened) => opened,
                    None => std::fs::read_to_string(&path).unwrap_or_default(),
                };
                let symbols = if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
                    == nexa_syntax::SourceProfile::Contract
                {
                    contract_document_symbols(&source)
                } else {
                    Vec::new()
                };
                respond(writer, id, &Value::Array(symbols))?;
            }
            Some("textDocument/completion") => {
                let (path, source) =
                    request_document_source(&documents, &message["params"]["textDocument"])?;
                let items = if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
                    == nexa_syntax::SourceProfile::Executable
                {
                    let offset = request_byte_offset(&source, &message["params"]["position"])?;
                    analyzer
                        .semantic_for_path(&path)
                        .and_then(|document| semantic_completion_items(&document, &source, offset))
                        .unwrap_or_else(|| language_v3_completion_items(&source, offset))
                } else {
                    Vec::new()
                };
                respond(writer, id, &Value::Array(items))?;
            }
            Some("textDocument/hover") => {
                let (path, source) =
                    request_document_source(&documents, &message["params"]["textDocument"])?;
                let hover = if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
                    == nexa_syntax::SourceProfile::Executable
                {
                    let offset = request_byte_offset(&source, &message["params"]["position"])?;
                    analyzer
                        .semantic_for_path(&path)
                        .and_then(|document| semantic_hover(&document, &source, offset))
                        .or_else(|| language_v3_hover(&source, offset))
                } else {
                    None
                };
                respond(writer, id, hover.as_ref().unwrap_or(&Value::Null))?;
            }
            Some("textDocument/definition") => {
                let (path, source) =
                    request_document_source(&documents, &message["params"]["textDocument"])?;
                let definition = if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
                    == nexa_syntax::SourceProfile::Executable
                {
                    let offset = request_byte_offset(&source, &message["params"]["position"])?;
                    analyzer.semantic_for_path(&path).and_then(|document| {
                        semantic_definition_location(&document, &source, offset, &documents)
                    })
                } else {
                    None
                };
                respond(writer, id, definition.as_ref().unwrap_or(&Value::Null))?;
            }
            Some("textDocument/semanticTokens/full") => {
                let (path, source) =
                    request_document_source(&documents, &message["params"]["textDocument"])?;
                let data = if nexa_syntax::SourceProfile::from_path(&path.to_string_lossy())
                    == nexa_syntax::SourceProfile::Executable
                {
                    language_v3_semantic_tokens(&source)
                } else {
                    Vec::new()
                };
                respond(writer, id, &json!({"data": data}))?;
            }
            Some("workspace/didChangeWatchedFiles") => {
                let changes = message["params"]["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| {
                        let uri = change.get("uri").and_then(Value::as_str)?;
                        let path = lexical_path(&file_uri_to_path(uri).ok()?);
                        BuildInputChange::watched(&path, change.get("type").and_then(Value::as_i64))
                    })
                    .collect::<Vec<_>>();
                if !changes.is_empty() {
                    for change in &changes {
                        if change.change == ChangeKind::DeletedFromDisk
                            && !documents
                                .values()
                                .any(|document| same_file_path(&document.path, &change.path))
                        {
                            known_inputs.retain(|known| !same_file_path(known, &change.path));
                        } else {
                            insert_known_path(&mut known_inputs, change.path.clone());
                        }
                    }
                    analyze_and_publish(
                        writer,
                        analyzer,
                        &workspace_roots,
                        &documents,
                        &known_inputs,
                        &changes,
                        &mut published_diagnostics,
                    )?;
                }
            }
            Some("workspace/didChangeWorkspaceFolders") => {
                let folder_change = apply_workspace_folder_change(
                    &mut workspace_roots,
                    &message["params"]["event"],
                );
                if !folder_change.changes.is_empty() {
                    known_inputs.retain(|path| {
                        !folder_change
                            .removed
                            .iter()
                            .any(|root| path_within(path, root))
                            || workspace_roots.iter().any(|root| path_within(path, root))
                    });
                    analyze_and_publish(
                        writer,
                        analyzer,
                        &workspace_roots,
                        &documents,
                        &known_inputs,
                        &folder_change.changes,
                        &mut published_diagnostics,
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

fn analyze_and_publish(
    writer: &mut impl Write,
    analyzer: &mut impl WorkspaceAnalyzer,
    roots: &[PathBuf],
    documents: &BTreeMap<String, OpenDocument>,
    known_inputs: &BTreeSet<PathBuf>,
    changes: &[BuildInputChange],
    published_diagnostics: &mut PublishedDiagnostics,
) -> Result<(), String> {
    let snapshot = WorkspaceSnapshot {
        roots,
        documents,
        known_inputs,
    };
    let report = analyzer.analyze(&snapshot, changes)?;
    publish_report(writer, &snapshot, report, published_diagnostics)
}

fn publish_report(
    writer: &mut impl Write,
    snapshot: &WorkspaceSnapshot<'_>,
    report: AnalysisReport,
    published_diagnostics: &mut PublishedDiagnostics,
) -> Result<(), String> {
    let mut grouped = BTreeMap::<PathBuf, (String, Vec<Value>)>::new();
    let mut checked_uris = BTreeMap::<PathBuf, String>::new();
    for path in report.checked_paths {
        let identity = lexical_path(&path);
        let uri = preferred_uri_for_path(snapshot, &path, &identity, published_diagnostics)?;
        checked_uris.entry(identity).or_insert(uri);
    }
    for located in report.diagnostics {
        let path = located.source_path();
        let identity = lexical_path(&path);
        let uri = preferred_uri_for_path(snapshot, &path, &identity, published_diagnostics)?;
        checked_uris
            .entry(identity.clone())
            .or_insert_with(|| uri.clone());
        grouped
            .entry(identity)
            .or_insert_with(|| (uri, Vec::new()))
            .1
            .push(lsp_diagnostic_with_paths(
                &located.diagnostic,
                Some(&located.root),
                &located.source_paths,
                Some(snapshot),
            ));
    }

    let current_diagnostics = grouped
        .iter()
        .map(|(path, (uri, _))| (path.clone(), uri.clone()))
        .collect::<BTreeMap<_, _>>();
    let targets = checked_uris
        .keys()
        .cloned()
        .chain(published_diagnostics.diagnostic_uris.keys().cloned())
        .chain(current_diagnostics.keys().cloned())
        .collect::<BTreeSet<_>>();
    for path in targets {
        let previous_uri = published_diagnostics.diagnostic_uris.get(&path);
        let (uri, diagnostics) = grouped.remove(&path).map_or_else(
            || {
                let uri = previous_uri
                    .cloned()
                    .or_else(|| checked_uris.get(&path).cloned());
                (uri, Vec::new())
            },
            |(uri, diagnostics)| (Some(uri), diagnostics),
        );
        let Some(uri) = uri else {
            continue;
        };
        if let Some(previous_uri) = previous_uri.filter(|previous| **previous != uri) {
            publish_diagnostics(writer, snapshot, previous_uri, Vec::new())?;
        }
        publish_diagnostics(writer, snapshot, &uri, diagnostics)?;
    }
    published_diagnostics.diagnostic_uris = current_diagnostics;
    published_diagnostics.preferred_uris = checked_uris;
    Ok(())
}

fn preferred_uri_for_path(
    snapshot: &WorkspaceSnapshot<'_>,
    path: &Path,
    identity: &Path,
    published_diagnostics: &PublishedDiagnostics,
) -> Result<String, String> {
    if let Some(document) = snapshot.document_for_path(path) {
        return Ok(document.uri.clone());
    }
    published_diagnostics
        .preferred_uris
        .get(identity)
        .cloned()
        .map_or_else(|| snapshot.uri_for_path(path), Ok)
}

fn publish_diagnostics(
    writer: &mut impl Write,
    snapshot: &WorkspaceSnapshot<'_>,
    uri: &str,
    diagnostics: Vec<Value>,
) -> Result<(), String> {
    let mut params = serde_json::Map::from_iter([
        ("uri".to_owned(), Value::String(uri.to_owned())),
        ("diagnostics".to_owned(), Value::Array(diagnostics)),
    ]);
    if let Some(version) = snapshot.version_for_uri(uri) {
        params.insert("version".to_owned(), Value::from(version));
    }
    notify(
        writer,
        "textDocument/publishDiagnostics",
        &Value::Object(params),
    )
}

impl WorkspaceAnalyzer for CurrentWorkspaceAnalyzer {
    #[allow(clippy::too_many_lines)]
    fn analyze(
        &mut self,
        snapshot: &WorkspaceSnapshot<'_>,
        changes: &[BuildInputChange],
    ) -> Result<AnalysisReport, String> {
        let mut report = AnalysisReport::default();
        report
            .checked_paths
            .extend(snapshot.known_inputs.iter().cloned());
        report
            .checked_paths
            .extend(changes.iter().map(|change| change.path.clone()));

        let structural_change = changes.iter().any(|change| {
            matches!(
                change.input,
                BuildInputKind::ContractSource
                    | BuildInputKind::LegacyNidl
                    | BuildInputKind::PackageManifest
                    | BuildInputKind::Lockfile
                    | BuildInputKind::WorkspaceManifest
            )
        });
        let mut package_scopes = BTreeMap::<PathBuf, PathBuf>::new();
        let mut project_configs = BTreeSet::<PathBuf>::new();
        let mut standalone_sources = BTreeSet::<PathBuf>::new();
        let mut contract_sources = BTreeSet::<PathBuf>::new();
        let mut legacy_nidl_sources = BTreeSet::<PathBuf>::new();
        let mut affected_inputs = snapshot.known_inputs.clone();
        affected_inputs.extend(changes.iter().map(|change| change.path.clone()));
        for path in &affected_inputs {
            match BuildInputKind::for_path(path) {
                Some(BuildInputKind::NexaSource) => {
                    if let Some((package, config)) = package_scope(snapshot, path) {
                        project_configs.insert(config.clone());
                        package_scopes.insert(package, config);
                    } else {
                        standalone_sources.insert(path.clone());
                    }
                }
                Some(BuildInputKind::ContractSource) => {
                    contract_sources.insert(path.clone());
                    if let Some(config) = find_upward(snapshot, path, "nexa.dev.toml") {
                        project_configs.insert(config);
                    }
                }
                Some(BuildInputKind::LegacyNidl) => {
                    legacy_nidl_sources.insert(path.clone());
                    if let Some(config) = find_upward(snapshot, path, "nexa.dev.toml") {
                        project_configs.insert(config);
                    }
                }
                Some(BuildInputKind::PackageManifest | BuildInputKind::Lockfile) => {
                    if let Some((package, config)) = package_scope(snapshot, path) {
                        project_configs.insert(config.clone());
                        package_scopes.insert(package, config);
                    } else if let Some(config) = find_upward(snapshot, path, "nexa.dev.toml") {
                        project_configs.insert(config);
                    }
                }
                Some(BuildInputKind::WorkspaceManifest) => {
                    let removed = changes.iter().any(|change| {
                        change.input == BuildInputKind::WorkspaceManifest
                            && change.change == ChangeKind::WorkspaceRemoved
                            && same_file_path(&change.path, path)
                    });
                    if !removed {
                        project_configs.insert(path.clone());
                    }
                }
                None => {}
            }
        }
        if structural_change {
            for root in snapshot.roots {
                if let Some((package, config)) = package_scope(snapshot, &root.join("package.toml"))
                {
                    project_configs.insert(config.clone());
                    package_scopes.insert(package, config);
                }
                let config = root.join("nexa.dev.toml");
                if snapshot.contains_path(&config) {
                    project_configs.insert(config);
                }
            }
        }
        let overlays = build_overlays(snapshot);
        for config in project_configs {
            let project = match project::LoadedProject::load_editor_snapshot(&config, |path| {
                valid_project_metadata_overlay(snapshot, path)
            }) {
                Ok(project) => project,
                Err(error) => {
                    if let Some(precise) =
                        contract_nidl_load_diagnostics(snapshot, &config, &error.to_string())
                    {
                        // An invalid Host Contract overlay (or disk text) is the actionable cause;
                        // do not degrade the whole project to a generic NX7002 as well.
                        report.diagnostics.extend(precise);
                    } else {
                        report.diagnostics.push(LocatedDiagnostic::new(
                            EngineDiagnostic::without_source(
                                None,
                                SourceId::new("editor").ok(),
                                EngineDiagnosticStage::Manifest,
                                nexa::ErrorCode::NX7002,
                                error.to_string(),
                            ),
                            config
                                .parent()
                                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
                            config,
                        ));
                    }
                    continue;
                }
            };
            if let Err(message) = validate_required_entrypoints_for_contract(
                &project.contract,
                &project.required_entrypoints,
            ) {
                report.diagnostics.push(LocatedDiagnostic::new(
                    EngineDiagnostic::without_source(
                        None,
                        SourceId::new("editor").ok(),
                        EngineDiagnosticStage::Export,
                        nexa::ErrorCode::NX7010,
                        message,
                    ),
                    project.root.clone(),
                    project.contract_path.clone(),
                ));
            }
            match project.package_directories_with_overlays(&overlays) {
                Ok(packages) => {
                    package_scopes.extend(
                        packages
                            .into_iter()
                            .map(|package| (package.directory, project.config_path.clone())),
                    );
                }
                Err(error) => {
                    report.diagnostics.push(LocatedDiagnostic::new(
                        EngineDiagnostic::without_source(
                            None,
                            SourceId::new("editor").ok(),
                            EngineDiagnosticStage::SourceDiscovery,
                            nexa::ErrorCode::NX7001,
                            error.to_string(),
                        ),
                        project.root,
                        project.config_path,
                    ));
                }
            }
        }

        // Every discovered project package participates so dependency overlays can invalidate
        // consumers. Standalone files remain on the single-file fallback below.
        let live_package_paths = package_scopes
            .keys()
            .map(|path| lexical_path(path))
            .collect::<BTreeSet<_>>();
        self.package_sessions
            .retain(|path, _| live_package_paths.contains(path));
        for (package, config) in package_scopes {
            let session = self
                .package_sessions
                .entry(lexical_path(&package))
                .or_default();
            report.diagnostics.extend(analyze_package_snapshot(
                &package, &config, snapshot, session,
            )?);
        }
        for path in standalone_sources {
            let Some(source) = snapshot.text_for_path(&path)? else {
                self.standalone_sessions.remove(&lexical_path(&path));
                continue;
            };
            let session = self
                .standalone_sessions
                .entry(lexical_path(&path))
                .or_default();
            report
                .diagnostics
                .extend(diagnostics_for_nexa_source(&path, &source, session)?);
        }
        for path in contract_sources {
            let Some(source) = snapshot.text_for_path(&path)? else {
                continue;
            };
            let root = path
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            report.diagnostics.extend(
                diagnostics_for_contract_source(&path, &source)?
                    .into_iter()
                    .map(|diagnostic| {
                        LocatedDiagnostic::new(diagnostic, root.clone(), path.clone())
                    }),
            );
        }
        for path in legacy_nidl_sources {
            let Some(source) = snapshot.text_for_path(&path)? else {
                continue;
            };
            let root = path
                .parent()
                .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
            report.diagnostics.extend(
                diagnostics_for_nidl_migration(&path, &source)
                    .into_iter()
                    .map(|diagnostic| {
                        LocatedDiagnostic::new(diagnostic, root.clone(), path.clone())
                    }),
            );
        }
        deduplicate_diagnostics(&mut report.diagnostics);
        Ok(report)
    }

    fn semantic_for_path(&self, path: &Path) -> Option<SemanticDocument> {
        self.package_sessions
            .values()
            .chain(self.standalone_sessions.values())
            .find_map(|session| session.semantic.as_ref()?.document(path))
    }
}

fn valid_project_metadata_overlay(snapshot: &WorkspaceSnapshot<'_>, path: &Path) -> Option<String> {
    let source = snapshot.overlay_for_path(path)?;
    if path.extension().and_then(|extension| extension.to_str()) == Some("nidl") {
        // Legacy `.nidl` contract files are migrated to `*.contract.nexa`, not parsed by the
        // Contract grammar. Feeding them to the project resolver only reproduces a stale parse
        // error; the migration diagnostic is emitted once by `contract_nidl_load_diagnostics`.
        return None;
    }
    Some(source.to_owned())
}

/// When project loading fails because the Host Contract text the editor is working against (the
/// unsaved overlay, falling back to disk) is not valid Contract v3, the load error is only a
/// symptom of that broken contract. Report the precise parser/validation spans (or, for a legacy
/// `.nidl` file, a single migration diagnostic) at the contract URI instead of a generic NX7002
/// anchored at the project manifest, and return `None` when the failure must stay a generic
/// project-load diagnostic.
fn contract_nidl_load_diagnostics(
    snapshot: &WorkspaceSnapshot<'_>,
    config: &Path,
    load_error: &str,
) -> Option<Vec<LocatedDiagnostic>> {
    let root = config
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
    let config_source = snapshot
        .overlay_for_path(config)
        .map(str::to_owned)
        .or_else(|| std::fs::read_to_string(config).ok())?;
    let parsed: project::ProjectConfig = toml::from_str(&config_source).ok()?;
    let contract_path = root.join(parsed.contract);
    // The loader reports the failure with the resolved contract path; the editor document may
    // spell it differently (symlinked temp roots, `..` segments), so the file name is the stable
    // discriminator between a contract-caused failure and an unrelated manifest/environment error.
    let contract_name = contract_path.file_name().and_then(|name| name.to_str())?;
    if !load_error.contains(contract_name) {
        return None;
    }
    let source = snapshot.text_for_path(&contract_path).ok()??;
    if nexa_syntax::SourceProfile::from_path(&contract_path.to_string_lossy())
        == nexa_syntax::SourceProfile::Contract
    {
        // A valid or invalid modern Contract file: only precise parser spans if invalid.
        if nexa::parse_contract(&source).is_ok() {
            return None;
        }
        Some(
            diagnostics_for_contract_source(&contract_path, &source)
                .ok()?
                .into_iter()
                .map(|diagnostic| {
                    LocatedDiagnostic::new(diagnostic, root.clone(), contract_path.clone())
                })
                .collect(),
        )
    } else {
        // Legacy `.nidl` contract: one actionable migration diagnostic, never a Contract parse.
        Some(
            diagnostics_for_nidl_migration(&contract_path, &source)
                .into_iter()
                .map(|diagnostic| {
                    LocatedDiagnostic::new(diagnostic, root.clone(), contract_path.clone())
                })
                .collect(),
        )
    }
}

fn build_overlays(snapshot: &WorkspaceSnapshot<'_>) -> BTreeMap<PathBuf, String> {
    snapshot
        .documents
        .values()
        .filter(|document| {
            matches!(
                BuildInputKind::for_path(&document.path),
                Some(
                    BuildInputKind::NexaSource
                        | BuildInputKind::PackageManifest
                        | BuildInputKind::Lockfile
                )
            )
        })
        .map(|document| (document.path.clone(), document.text.clone()))
        .collect()
}

fn package_scope(snapshot: &WorkspaceSnapshot<'_>, path: &Path) -> Option<(PathBuf, PathBuf)> {
    let manifest = find_upward(snapshot, path, "package.toml")?;
    let package = manifest.parent()?.to_path_buf();
    let config = find_upward(snapshot, &manifest, "nexa.dev.toml")?;
    Some((package, config))
}

#[allow(clippy::too_many_lines)]
fn analyze_package_snapshot(
    package: &Path,
    config: &Path,
    snapshot: &WorkspaceSnapshot<'_>,
    session: &mut PackageAnalysisSession,
) -> Result<Vec<LocatedDiagnostic>, String> {
    let project = match project::LoadedProject::load_editor_snapshot(config, |path| {
        valid_project_metadata_overlay(snapshot, path)
    }) {
        Ok(project) => project,
        Err(error) => {
            if let Some(precise) =
                contract_nidl_load_diagnostics(snapshot, config, &error.to_string())
            {
                return Ok(precise);
            }
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    None,
                    SourceId::new("editor").ok(),
                    EngineDiagnosticStage::Manifest,
                    nexa::ErrorCode::NX7002,
                    error.to_string(),
                ),
                config
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
                config.to_path_buf(),
            )]);
        }
    };

    let contract_path = project.contract_path.clone();
    let is_contract_path = nexa_syntax::SourceProfile::from_path(&contract_path.to_string_lossy())
        == nexa_syntax::SourceProfile::Contract;
    let (validated_contract, contract_source) =
        if let Some(contract_overlay) = snapshot.overlay_for_path(&contract_path) {
            if !is_contract_path {
                // Legacy `.nidl` contract overlay: one migration diagnostic, never a Contract parse.
                let root = contract_path
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
                return Ok(
                    diagnostics_for_nidl_migration(&contract_path, contract_overlay)
                        .into_iter()
                        .map(|diagnostic| {
                            LocatedDiagnostic::new(diagnostic, root.clone(), contract_path.clone())
                        })
                        .collect(),
                );
            }
            let Ok(validated_contract) = nexa::parse_contract(contract_overlay) else {
                let root = contract_path
                    .parent()
                    .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
                return Ok(
                    diagnostics_for_contract_source(&contract_path, contract_overlay)?
                        .into_iter()
                        .map(|diagnostic| {
                            LocatedDiagnostic::new(diagnostic, root.clone(), contract_path.clone())
                        })
                        .collect(),
                );
            };
            if let Err(message) = validate_required_entrypoints_for_contract(
                &validated_contract,
                &project.required_entrypoints,
            ) {
                return Ok(vec![LocatedDiagnostic::new(
                    EngineDiagnostic::without_source(
                        None,
                        SourceId::new("editor").ok(),
                        EngineDiagnosticStage::Export,
                        nexa::ErrorCode::NX7010,
                        message,
                    ),
                    project.root.clone(),
                    contract_path,
                )]);
            }
            (validated_contract, Arc::<str>::from(contract_overlay))
        } else {
            (
                project.contract.clone(),
                Arc::<str>::from(project.contract_source.as_str()),
            )
        };
    let contract_identity =
        nexa::SourceIdentity::standalone(contract_path.to_string_lossy().into_owned());
    let contract = match nexa::HostContractInput::with_source(
        &validated_contract,
        contract_identity,
        Arc::clone(&contract_source),
    ) {
        Ok(contract) => contract,
        Err(error) => {
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    None,
                    SourceId::new("editor").ok(),
                    EngineDiagnosticStage::Manifest,
                    nexa::ErrorCode::NX7002,
                    error.to_string(),
                ),
                project.root.clone(),
                contract_path,
            )]);
        }
    };

    let overlays = build_overlays(snapshot);
    let discovered = match project.package_directories_with_overlays(&overlays) {
        Ok(packages) => packages
            .into_iter()
            .find(|discovered| same_file_path(&discovered.directory, package)),
        Err(error) => {
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    None,
                    SourceId::new("editor").ok(),
                    EngineDiagnosticStage::SourceDiscovery,
                    nexa::ErrorCode::NX7001,
                    error.to_string(),
                ),
                project.root.clone(),
                config.to_path_buf(),
            )]);
        }
    };
    let manifest_path = package.join("package.toml");
    let Some(discovered) = discovered else {
        return Ok(vec![LocatedDiagnostic::new(
            EngineDiagnostic::without_source(
                None,
                SourceId::new("editor").ok(),
                EngineDiagnosticStage::SourceDiscovery,
                nexa::ErrorCode::NX7001,
                format!(
                    "package {} is not part of the loaded project",
                    package.display()
                ),
            ),
            package.to_path_buf(),
            manifest_path,
        )]);
    };

    let build = match project.resolve_package_with_overlays(&discovered, true, &overlays) {
        Ok(build) => build,
        Err(error) => {
            let fallback = if error.to_string().contains("nexa.lock") {
                package.join("nexa.lock")
            } else {
                manifest_path
            };
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    None,
                    SourceId::new("editor").ok(),
                    EngineDiagnosticStage::SourceDiscovery,
                    nexa::ErrorCode::NX7001,
                    error.to_string(),
                ),
                package.to_path_buf(),
                fallback,
            )]);
        }
    };
    let build = match resolved_build_with_overlays(&build, snapshot, &contract) {
        Ok(build) => build,
        Err(message) => {
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    None,
                    SourceId::new("editor").ok(),
                    EngineDiagnosticStage::SourceDiscovery,
                    nexa::ErrorCode::NX7001,
                    message,
                ),
                package.to_path_buf(),
                manifest_path,
            )]);
        }
    };
    let generation = session.prepare(&build);
    let check = match session
        .build
        .check_package_with_contract(&build.input, &contract)
    {
        Ok(check) => check,
        Err(nexa::PackageBuildError::AnalysisFailed(batch)) => {
            session.semantic = None;
            return Ok(located_diagnostic_batch(&build, &batch, &contract_path));
        }
        Err(error) => {
            session.semantic = None;
            let stage = package_build_error_stage(&error);
            let code = package_build_error_code(&error);
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    Some(build.root.manifest.id.clone()),
                    Some(build.source_id.clone()),
                    stage,
                    code,
                    error.to_string(),
                ),
                package.to_path_buf(),
                manifest_path,
            )]);
        }
    };
    session.semantic = Some(SemanticSnapshot::from_build(
        &build,
        Arc::clone(&check.semantic_ir),
    ));
    match build.compile_with_contract_session(
        &mut session.build,
        generation,
        &contract,
        &project.required_entrypoints,
        true,
    ) {
        Ok(_) => Ok(Vec::new()),
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::AnalysisFailed(batch))) => {
            session.semantic = None;
            Ok(located_diagnostic_batch(&build, &batch, &contract_path))
        }
        Err(project::BuildCompileError::Facade(error)) => {
            let stage = package_build_error_stage(&error);
            let code = package_build_error_code(&error);
            Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    Some(build.root.manifest.id.clone()),
                    Some(build.source_id.clone()),
                    stage,
                    code,
                    error.to_string(),
                ),
                package.to_path_buf(),
                manifest_path,
            )])
        }
        Err(project::BuildCompileError::Cli(error)) => Ok(vec![LocatedDiagnostic::new(
            EngineDiagnostic::without_source(
                None,
                SourceId::new("editor").ok(),
                EngineDiagnosticStage::Compile,
                nexa::ErrorCode::NX7001,
                error.to_string(),
            ),
            package.to_path_buf(),
            manifest_path,
        )]),
    }
}

fn resolved_build_with_overlays(
    build: &project::ResolvedBuild,
    snapshot: &WorkspaceSnapshot<'_>,
    host_contract: &nexa::HostContractInput<'_>,
) -> Result<project::ResolvedBuild, String> {
    let mut packages = BTreeMap::new();
    for (package_id, loaded) in &build.packages {
        let production_sources = source_set_with_overlays(
            package_id,
            loaded,
            &loaded.production_sources,
            Path::new("src"),
            nexa_analysis::SourceRole::Production,
            snapshot,
        )?;
        let test_sources = source_set_with_overlays(
            package_id,
            loaded,
            &loaded.test_sources,
            Path::new("tests"),
            nexa_analysis::SourceRole::Test,
            snapshot,
        )?;
        packages.insert(
            package_id.clone(),
            Arc::new(nexa_analysis::LoadedPackageDirectory {
                directory: loaded.directory.clone(),
                manifest_source: Arc::clone(&loaded.manifest_source),
                manifest: Arc::clone(&loaded.manifest),
                production_sources,
                test_sources,
                lock: loaded.lock.clone(),
            }),
        );
    }

    build
        .rebuild_with_contract(packages, host_contract)
        .map_err(|error| error.to_string())
}

fn source_set_with_overlays(
    package_id: &nexa_analysis::PackageId,
    loaded: &nexa_analysis::LoadedPackageDirectory,
    original: &nexa_analysis::PackageSourceSet,
    source_root: &Path,
    role: nexa_analysis::SourceRole,
    snapshot: &WorkspaceSnapshot<'_>,
) -> Result<Arc<nexa_analysis::PackageSourceSet>, String> {
    let mut builder = nexa_analysis::SourceSetBuilder::new(
        package_id.clone(),
        nexa_analysis::CompilationLimits::default(),
    );
    let mut paths = BTreeSet::new();
    for unit in original.units().values().filter(|unit| unit.role == role) {
        let path = loaded.directory.join(unit.key.path.as_path());
        let text = snapshot
            .overlay_for_path(&path)
            .map_or_else(|| Arc::clone(&unit.text), Arc::from);
        builder
            .add(unit.key.path.clone(), text, role)
            .map_err(|error| error.to_string())?;
        paths.insert(unit.key.path.clone());
    }

    let package_root = lexical_path(&loaded.directory);
    for document in snapshot.documents.values() {
        if document.path.extension().and_then(|value| value.to_str()) != Some("nexa") {
            continue;
        }
        let document_path = lexical_path(&document.path);
        let Ok(relative) = document_path.strip_prefix(&package_root) else {
            continue;
        };
        if !relative.starts_with(source_root) {
            continue;
        }
        let relative = nexa_analysis::NormalizedPackagePath::from_path(relative)
            .map_err(|error| error.to_string())?;
        if paths.insert(relative.clone()) {
            builder
                .add(relative, document.text.as_str(), role)
                .map_err(|error| error.to_string())?;
        }
    }

    builder
        .build()
        .map(Arc::new)
        .map_err(|error| error.to_string())
}

fn located_diagnostic_batch(
    build: &project::ResolvedBuild,
    batch: &nexa::DiagnosticBatch,
    contract_path: &Path,
) -> Vec<LocatedDiagnostic> {
    let source_paths = resolved_source_paths(build, batch, contract_path);
    let root = build.root.directory.clone();
    let fallback_path = root.join("package.toml");
    locate_diagnostic_batch(build, batch, &source_paths, &root, &fallback_path)
}

fn locate_diagnostic_batch(
    build: &project::ResolvedBuild,
    batch: &nexa::DiagnosticBatch,
    source_paths: &Arc<BTreeMap<nexa::SourceIdentity, PathBuf>>,
    root: &Path,
    fallback_path: &Path,
) -> Vec<LocatedDiagnostic> {
    EngineDiagnostic::from_diagnostic_batch(
        Some(build.root.manifest.id.clone()),
        Some(build.source_id.clone()),
        EngineDiagnosticStage::TypeCheck,
        batch,
    )
    .into_iter()
    .map(|diagnostic| {
        let fallback_path = diagnostic
            .file
            .as_ref()
            .and_then(|identity| source_paths.get(identity))
            .cloned()
            .unwrap_or_else(|| fallback_path.to_path_buf());
        LocatedDiagnostic {
            diagnostic,
            root: root.to_path_buf(),
            fallback_path,
            source_paths: Arc::clone(source_paths),
        }
    })
    .collect()
}

fn resolved_source_paths(
    build: &project::ResolvedBuild,
    batch: &nexa::DiagnosticBatch,
    contract_path: &Path,
) -> Arc<BTreeMap<nexa::SourceIdentity, PathBuf>> {
    let mut paths = BTreeMap::new();
    for package in build.packages.values() {
        for source_set in [&package.production_sources, &package.test_sources] {
            for source in source_set.units().values() {
                paths.insert(
                    nexa::SourceIdentity::package(
                        package.manifest.id.as_str(),
                        source.key.path.as_str(),
                    ),
                    package.directory.join(source.key.path.as_path()),
                );
            }
        }
    }
    for (identity, _) in batch.sources().iter() {
        if identity.package_id().is_none()
            && Path::new(identity.path())
                .extension()
                .is_some_and(|extension| extension == "nidl")
        {
            paths.insert(identity.clone(), contract_path.to_path_buf());
        }
    }
    Arc::new(paths)
}

const fn package_build_error_stage(error: &nexa::PackageBuildError) -> EngineDiagnosticStage {
    match error {
        nexa::PackageBuildError::Verify(_)
        | nexa::PackageBuildError::InvalidTestArtifact(_)
        | nexa::PackageBuildError::Integrity(_) => EngineDiagnosticStage::Verify,
        nexa::PackageBuildError::MissingRequiredEntrypoint(_)
        | nexa::PackageBuildError::EntrypointSignatureMismatch { .. }
        | nexa::PackageBuildError::HostContractMismatch
        | nexa::PackageBuildError::HostContractSourceMismatch
        | nexa::PackageBuildError::HostRequiredEntrypointsMismatch
        | nexa::PackageBuildError::HostContractIdMismatch => EngineDiagnosticStage::Export,
        nexa::PackageBuildError::AnalysisFailed(_) => EngineDiagnosticStage::TypeCheck,
        _ => EngineDiagnosticStage::Compile,
    }
}

fn package_build_error_code(error: &nexa::PackageBuildError) -> nexa::ErrorCode {
    match error {
        nexa::PackageBuildError::Compile(error) => {
            nexa::Diagnostic::new(error, nexa::FileId::default()).code
        }
        nexa::PackageBuildError::Verify(error) => nexa::ClassifiedError::metadata(error).code,
        nexa::PackageBuildError::MissingRequiredEntrypoint(_) => nexa::ErrorCode::NX7010,
        nexa::PackageBuildError::EntrypointSignatureMismatch { .. } => nexa::ErrorCode::NX7011,
        nexa::PackageBuildError::HostContractMismatch
        | nexa::PackageBuildError::HostContractSourceMismatch
        | nexa::PackageBuildError::HostRequiredEntrypointsMismatch
        | nexa::PackageBuildError::HostContractIdMismatch => nexa::ErrorCode::NX4001,
        _ => nexa::ErrorCode::NX7001,
    }
}

fn validate_required_entrypoints_for_contract(
    contract: &nexa::ValidatedContract,
    names: &[String],
) -> Result<(), String> {
    for name in names {
        if !contract
            .nexa_functions
            .iter()
            .any(|entrypoint| entrypoint.name == *name)
        {
            return Err(format!(
                "required Nexa entrypoint `{name}` is not declared by the current Contract"
            ));
        }
    }
    Ok(())
}

fn deduplicate_diagnostics(diagnostics: &mut Vec<LocatedDiagnostic>) {
    let mut seen = BTreeSet::new();
    diagnostics.retain(|located| {
        let primary = located
            .diagnostic
            .diagnostic
            .primary
            .as_ref()
            .map(|label| (label.span.start, label.span.end));
        seen.insert((
            lexical_path(&located.source_path()),
            located.diagnostic.diagnostic.code.as_str().to_owned(),
            primary,
            located.diagnostic.diagnostic.message.to_string(),
        ))
    });
}

#[cfg(test)]
fn diagnostics_for_path(
    path: &Path,
    overlay: Option<&str>,
) -> Result<Vec<EngineDiagnostic>, String> {
    let source = overlay.map_or_else(
        || {
            std::fs::read_to_string(path)
                .map_err(|error| format!("could not read {}: {error}", path.display()))
        },
        |source| Ok(source.to_owned()),
    )?;
    match nexa_syntax::SourceProfile::from_path(&path.to_string_lossy()) {
        nexa_syntax::SourceProfile::Contract => diagnostics_for_contract_source(path, &source),
        nexa_syntax::SourceProfile::Executable => {
            if path.extension().and_then(|extension| extension.to_str()) == Some("nidl") {
                Ok(diagnostics_for_nidl_migration(path, &source))
            } else {
                let mut session = PackageAnalysisSession::default();
                diagnostics_for_nexa_source(path, &source, &mut session).map(|diagnostics| {
                    diagnostics
                        .into_iter()
                        .map(|located| located.diagnostic)
                        .collect()
                })
            }
        }
    }
}

/// Publishes an actionable migration diagnostic for a legacy `*.nidl` file instead of parsing it
/// with the Contract grammar. Tells the user the new file name and flat header form so they can
/// move to `*.contract.nexa` and satisfy the zero-`.nidl` migration gate.
fn diagnostics_for_nidl_migration(path: &Path, source: &str) -> Vec<EngineDiagnostic> {
    let source_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<contract>");
    let new_name = source_name.strip_suffix(".nidl").map_or_else(
        || "<name>.contract.nexa".to_owned(),
        |base| {
            // `game.nidl` -> `game.contract.nexa`; an already-suffixed name is kept intact.
            if base.ends_with(".contract") {
                format!("{base}.nexa")
            } else {
                format!("{base}.contract.nexa")
            }
        },
    );
    let identity = nexa::SourceIdentity::standalone(path.to_string_lossy().into_owned());
    let mut diagnostic = nexa::LeafDiagnostic::new(
        nexa::ErrorCode::NX1001,
        nexa::Severity::Error,
        format!("`.nidl` is removed; rename to `{new_name}` with flat `contract <Name>;`"),
    )
    .with_label(nexa::LeafLabel::primary(
        identity.clone(),
        nexa::ByteRange::new(0, 0),
        "legacy `.nidl` contract file",
    ));
    diagnostic
        .notes
        .push("migrate to *.contract.nexa; legacy .nidl is never parsed".into());
    let mut sources = nexa::SourceSnapshotRegistry::builder();
    if sources.insert(identity.clone(), source.to_owned()).is_err() {
        return vec![EngineDiagnostic::without_source(
            None,
            SourceId::new("editor").ok(),
            EngineDiagnosticStage::Parse,
            nexa::ErrorCode::NX1001,
            &diagnostic.message,
        )];
    }
    let sources = sources.build();
    vec![EngineDiagnostic::from_leaf_diagnostic(
        None,
        SourceId::new("editor").ok(),
        EngineDiagnosticStage::Parse,
        &diagnostic,
        &sources,
    )]
}

/// Maps a [`nexa::ContractErrorKind`] to a short, stable diagnostic category. This is what the
/// checklist describes as the six Contract diagnostic families, anchored to the LSP contract path.
fn contract_error_kind_category(kind: nexa::ContractErrorKind) -> &'static str {
    match kind {
        nexa::ContractErrorKind::Syntax => "contract.01.syntax",
        nexa::ContractErrorKind::Duplicate | nexa::ContractErrorKind::InvalidName => {
            "contract.02.naming"
        }
        nexa::ContractErrorKind::UnknownType
        | nexa::ContractErrorKind::InvalidType
        | nexa::ContractErrorKind::RecursiveLayout => "contract.03.type",
        nexa::ContractErrorKind::InvalidAttribute => "contract.04.attribute",
        nexa::ContractErrorKind::StableIdCollision => "contract.05.stable-id",
        nexa::ContractErrorKind::RustNameCollision => "contract.06.generated-rust",
    }
}

/// Publishes Contract diagnostics for a single source file. The Contract profile is chosen by
/// [`SourceProfile::from_path`] (`.contract.nexa`) in the LSP; project-config selection is handled
/// by the project resolver in task #6 and feeds the same `diagnostics_for_contract_source` path
/// through the project-contract overlay above.
fn diagnostics_for_contract_source(
    path: &Path,
    source: &str,
) -> Result<Vec<EngineDiagnostic>, String> {
    let identity = nexa::SourceIdentity::standalone(path.to_string_lossy().into_owned());
    let mut sources = nexa::SourceSnapshotRegistry::builder();
    sources
        .insert(identity.clone(), source.to_owned())
        .map_err(|error| error.to_string())?;
    let sources = sources.build();

    let errors = match nexa::parse_contract_ast(source) {
        Ok(ast) => {
            if let Ok(_validated) = nexa::validate_contract(&ast) {
                return Ok(Vec::new());
            }
            // `validate_contract` collapses to the first error; surface it through the same path
            // so every detected problem carries its contract diagnostic category.
            let Err(first) = nexa::validate_contract(&ast) else {
                return Ok(Vec::new());
            };
            vec![first]
        }
        Err(first) => vec![first],
    };

    Ok(errors
        .into_iter()
        .map(|error| {
            let span = error.span;
            let category = contract_error_kind_category(error.kind);
            // The six Contract families are surfaced as the machine error code itself (e.g.
            // `contract.03.type`) so the client receives a structured category without any
            // string-parsing of display text.
            let diagnostic = nexa::LeafDiagnostic::new(
                nexa::ErrorCode::new(category),
                nexa::Severity::Error,
                error.message,
            )
            .with_label(nexa::LeafLabel::primary(
                identity.clone(),
                nexa::ByteRange::new(span.start, span.end),
                "invalid Contract declaration",
            ));
            EngineDiagnostic::from_leaf_diagnostic(
                None,
                SourceId::new("editor").ok(),
                EngineDiagnosticStage::Parse,
                &diagnostic,
                &sources,
            )
        })
        .collect())
}

/// Builds the LSP `DocumentSymbol[]` payload for a Contract file. The outline exposes the six
/// Contract symbol families required by the Editor/LSP checklist: Contract / Struct / Enum /
/// Handle / Host Function / Nexa Function. Range conversions use the same byte→UTF-16 position
/// mapping as the diagnostic renderer, so editor cursors align with the parser spans.
fn contract_document_symbols(source: &str) -> Vec<serde_json::Value> {
    use nexa::prelude::SourceSpan;
    use serde_json::json;
    let Ok(ast) = nexa::parse_contract_ast(source) else {
        return Vec::new();
    };
    let range = |span: SourceSpan| {
        let start = byte_offset_to_lsp_position(source, span.start as usize);
        let end = byte_offset_to_lsp_position(source, span.end as usize);
        json!({
            "start": { "line": start.line, "character": start.character },
            "end": { "line": end.line, "character": end.character }
        })
    };
    let symbol = |name: &str, kind: u32, span: SourceSpan, selection: SourceSpan| {
        json!({
            "name": name,
            "kind": kind,
            "range": range(span),
            "selectionRange": range(selection),
            "children": []
        })
    };

    let contract = &ast.contract;
    let mut symbols = Vec::new();
    symbols.push(symbol(
        &contract.name,
        2, // SymbolKind.Module: the Contract header
        contract.span,
        contract.name_span,
    ));
    for structure in &contract.structs {
        symbols.push(symbol(
            &structure.name,
            23, // SymbolKind.Struct
            structure.span,
            structure.name_span,
        ));
    }
    for enumeration in &contract.enums {
        symbols.push(symbol(
            &enumeration.name,
            10, // SymbolKind.Enum
            enumeration.span,
            enumeration.name_span,
        ));
    }
    for handle in &contract.handles {
        symbols.push(symbol(
            &handle.name,
            11, // SymbolKind.Interface: a host token type
            handle.span,
            handle.name_span,
        ));
    }
    for function in contract
        .host
        .as_ref()
        .into_iter()
        .flat_map(|block| &block.functions)
    {
        symbols.push(symbol(
            &function.name,
            12, // SymbolKind.Function
            function.span,
            function.name_span,
        ));
    }
    for function in contract
        .nexa
        .as_ref()
        .into_iter()
        .flat_map(|block| &block.functions)
    {
        symbols.push(symbol(
            &function.name,
            12, // SymbolKind.Function
            function.span,
            function.name_span,
        ));
    }
    symbols
}

fn request_document_source(
    documents: &BTreeMap<String, OpenDocument>,
    text_document: &Value,
) -> Result<(PathBuf, String), String> {
    let uri = required_str(text_document, "uri")?;
    let path = file_uri_to_path(uri)?;
    if let Some(document) = documents.get(uri).or_else(|| {
        documents
            .values()
            .find(|document| same_file_path(&document.path, &path))
    }) {
        return Ok((document.path.clone(), document.text.clone()));
    }
    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read LSP document {}: {error}", path.display()))?;
    Ok((path, source))
}

fn request_byte_offset(source: &str, position: &Value) -> Result<usize, String> {
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
        .ok_or("missing or invalid LSP position line")?;
    let column = position
        .get("character")
        .and_then(Value::as_u64)
        .and_then(|column| u32::try_from(column).ok())
        .ok_or("missing or invalid LSP position character")?;
    let source = nexa_syntax::SourceText::new(source)
        .map_err(|error| format!("LSP source is too large: {error:?}"))?;
    nexa_syntax::LineIndex::new(&source)
        .offset(
            nexa_syntax::LineColumn { line, column },
            nexa_syntax::TextEncoding::Utf16,
        )
        .map(nexa_syntax::TextSize::to_usize)
        .ok_or_else(|| format!("LSP position {line}:{column} is outside the document"))
}

fn semantic_hover(document: &SemanticDocument, source: &str, offset: usize) -> Option<Value> {
    let offset = u32::try_from(offset).ok()?;
    if let Some(signature) =
        nexa_analysis::call_signature_at(&document.ir, &document.source, offset)
    {
        let parameters = signature
            .parameters
            .iter()
            .map(|parameter| {
                format!(
                    "{}: {}",
                    parameter.name,
                    nexa_analysis::display_type(&parameter.ty, &document.ir)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let prefix = if signature.effect == nexa_analysis::IrEffect::Task {
            "async "
        } else {
            ""
        };
        let value = format!(
            "```nexa\n{prefix}fn {}({parameters}) -> {}\n```",
            signature.name,
            nexa_analysis::display_type(&signature.result, &document.ir)
        );
        return Some(hover_value(source, &signature.span, &value));
    }

    let definition = nexa_analysis::definition_at(&document.ir, &document.source, offset)
        .and_then(|definition| document.ir.definition(definition))?;
    let span = nexa_analysis::semantic_span_at(&document.ir, &document.source, offset)
        .unwrap_or_else(|| definition.span.clone());
    let ty = nexa_analysis::display_type(&definition.ty, &document.ir);
    let value = match definition.kind {
        nexa_analysis::DefinitionKind::Local | nexa_analysis::DefinitionKind::Parameter => {
            format!("```nexa\n{}: {ty}\n```", definition.name)
        }
        nexa_analysis::DefinitionKind::Struct
        | nexa_analysis::DefinitionKind::Enum
        | nexa_analysis::DefinitionKind::Class => {
            format!(
                "```nexa\n{} {}\n```",
                definition_kind_name(definition.kind),
                definition.name
            )
        }
        _ => format!(
            "```nexa\n{} {}: {ty}\n```",
            definition_kind_name(definition.kind),
            definition.name
        ),
    };
    Some(hover_value(source, &span, &value))
}

fn definition_kind_name(kind: nexa_analysis::DefinitionKind) -> &'static str {
    match kind {
        nexa_analysis::DefinitionKind::Function => "fn",
        nexa_analysis::DefinitionKind::Task => "async fn",
        nexa_analysis::DefinitionKind::Struct => "struct",
        nexa_analysis::DefinitionKind::Enum => "enum",
        nexa_analysis::DefinitionKind::Class => "class",
        nexa_analysis::DefinitionKind::Const => "const",
        nexa_analysis::DefinitionKind::Field => "field",
        nexa_analysis::DefinitionKind::Variant => "variant",
        nexa_analysis::DefinitionKind::Parameter => "parameter",
        nexa_analysis::DefinitionKind::Local => "local",
        nexa_analysis::DefinitionKind::HostContract => "contract",
        nexa_analysis::DefinitionKind::HostFunction => "host fn",
        nexa_analysis::DefinitionKind::StandardLibrary => "std fn",
    }
}

fn hover_value(source: &str, span: &nexa_analysis::SourceRange, value: &str) -> Value {
    let start = byte_offset_to_lsp_position(source, span.start as usize);
    let end = byte_offset_to_lsp_position(source, span.end as usize);
    json!({
        "contents": {"kind": "markdown", "value": value},
        "range": {
            "start": {"line": start.line, "character": start.character},
            "end": {"line": end.line, "character": end.character}
        }
    })
}

fn semantic_completion_items(
    document: &SemanticDocument,
    source: &str,
    offset: usize,
) -> Option<Vec<Value>> {
    let (receiver_start, _, is_static) = language_v3_completion_receiver_range(source, offset)?;
    let ty = nexa_analysis::type_at(
        &document.ir,
        &document.source,
        u32::try_from(receiver_start).ok()?,
    )?;
    let methods = if is_static {
        nexa_analysis::associated_functions_for_type(&document.ir, &document.source, &ty)
    } else {
        nexa_analysis::methods_for_type(&document.ir, &document.source, &ty)
    };
    let mut items = methods
        .into_iter()
        .map(|method| {
            let type_parameters = if method.type_parameters.is_empty() {
                String::new()
            } else {
                format!("<{}>", method.type_parameters.join(", "))
            };
            let parameters = method
                .parameters
                .iter()
                .map(|parameter| {
                    format!(
                        "{}: {}",
                        parameter.name,
                        nexa_analysis::display_type(&parameter.ty, &document.ir)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let effect = if method.effect == nexa_analysis::IrEffect::Task {
                "async "
            } else {
                ""
            };
            json!({
                "label": method.name,
                "kind": 2,
                "detail": format!(
                    "{effect}fn {}{type_parameters}({parameters}) -> {}",
                    method.name,
                    nexa_analysis::display_type(&method.result, &document.ir)
                ),
                "documentation": method.documentation,
            })
        })
        .collect::<Vec<_>>();
    if is_static {
        return Some(items);
    }
    let owner = match &ty {
        nexa_analysis::IrType::Array(_) => Some("Array"),
        nexa_analysis::IrType::Map(_, _) => Some("Map"),
        nexa_analysis::IrType::Set(_) => Some("Set"),
        nexa_analysis::IrType::Buffer(_) => Some("Buffer"),
        _ => None,
    };
    if let Some(owner) = owner {
        items.extend(language_v3_completion_items_for_owner(owner, false));
        return Some(items);
    }
    (!items.is_empty() || matches!(ty, nexa_analysis::IrType::Named(_))).then_some(items)
}

fn semantic_definition_location(
    document: &SemanticDocument,
    current_source: &str,
    offset: usize,
    documents: &BTreeMap<String, OpenDocument>,
) -> Option<Value> {
    let target =
        nexa_analysis::definition_at(&document.ir, &document.source, u32::try_from(offset).ok()?)?;
    let definition = document.ir.definition(target)?;
    let path = document.paths_by_source.get(&definition.span.source)?;
    let target_document = documents
        .values()
        .find(|open| same_file_path(&open.path, path));
    let target_source = target_document
        .map(|open| open.text.clone())
        .or_else(|| std::fs::read_to_string(path).ok())?;
    let uri = target_document
        .map(|open| open.uri.clone())
        .or_else(|| path_to_file_uri(path).ok())?;
    let source = if definition.span.source == document.source {
        current_source
    } else {
        target_source.as_str()
    };
    let start = byte_offset_to_lsp_position(source, definition.span.start as usize);
    let end = byte_offset_to_lsp_position(source, definition.span.end as usize);
    Some(json!({
        "uri": uri,
        "range": {
            "start": {"line": start.line, "character": start.character},
            "end": {"line": end.line, "character": end.character}
        }
    }))
}

#[allow(clippy::too_many_lines)]
fn diagnostics_for_nexa_source(
    path: &Path,
    source: &str,
    session: &mut PackageAnalysisSession,
) -> Result<Vec<LocatedDiagnostic>, String> {
    let root = path
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
    let build =
        project::virtual_standalone_script(source, path).map_err(|error| error.to_string())?;
    let generation = session.prepare(&build);
    let contract = build
        .host_contract
        .input()
        .map_err(|error| format!("retained standalone Host contract is invalid: {error}"))?;
    let check = match session
        .build
        .check_standalone_with_contract(&build.input, &contract)
    {
        Ok(check) => check,
        Err(nexa::PackageBuildError::AnalysisFailed(batch)) => {
            session.semantic = None;
            let origin = build
                .virtual_source_origin
                .as_ref()
                .ok_or("virtual snippet build has no source origin")?;
            let internal_identity = nexa::SourceIdentity::package(
                origin.source_key.package_id.as_str(),
                origin.source_key.path.as_str(),
            );
            let source_paths = Arc::new(BTreeMap::from([(internal_identity, path.to_path_buf())]));
            return Ok(locate_diagnostic_batch(
                &build,
                &batch,
                &source_paths,
                &root,
                path,
            ));
        }
        Err(error) => {
            session.semantic = None;
            let stage = package_build_error_stage(&error);
            let code = package_build_error_code(&error);
            return Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    Some(build.root.manifest.id.clone()),
                    Some(build.source_id.clone()),
                    stage,
                    code,
                    error.to_string(),
                ),
                root,
                path.to_path_buf(),
            )]);
        }
    };
    let mut semantic = SemanticSnapshot::from_build(&build, Arc::clone(&check.semantic_ir));
    if let Some(origin) = &build.virtual_source_origin {
        let path = lexical_path(path);
        semantic
            .sources_by_path
            .insert(path.clone(), origin.source_key.clone());
        Arc::make_mut(&mut semantic.paths_by_source).insert(origin.source_key.clone(), path);
    }
    session.semantic = Some(semantic);
    match build.compile_standalone_with_session_and_limits(
        &mut session.build,
        generation,
        nexa::VerifierLimits::default(),
    ) {
        Ok(_) => Ok(Vec::new()),
        Err(project::BuildCompileError::Facade(nexa::PackageBuildError::AnalysisFailed(batch))) => {
            session.semantic = None;
            let origin = build
                .virtual_source_origin
                .as_ref()
                .ok_or("virtual snippet build has no source origin")?;
            let internal_identity = nexa::SourceIdentity::package(
                origin.source_key.package_id.as_str(),
                origin.source_key.path.as_str(),
            );
            let source_paths = Arc::new(BTreeMap::from([(internal_identity, path.to_path_buf())]));
            Ok(locate_diagnostic_batch(
                &build,
                &batch,
                &source_paths,
                &root,
                path,
            ))
        }
        Err(project::BuildCompileError::Facade(error)) => {
            let stage = package_build_error_stage(&error);
            let code = package_build_error_code(&error);
            Ok(vec![LocatedDiagnostic::new(
                EngineDiagnostic::without_source(
                    Some(build.root.manifest.id.clone()),
                    Some(build.source_id.clone()),
                    stage,
                    code,
                    error.to_string(),
                ),
                root,
                path.to_path_buf(),
            )])
        }
        Err(project::BuildCompileError::Cli(error)) => Ok(vec![LocatedDiagnostic::new(
            EngineDiagnostic::without_source(
                Some(build.root.manifest.id.clone()),
                Some(build.source_id.clone()),
                EngineDiagnosticStage::Compile,
                nexa::ErrorCode::NX7001,
                error.to_string(),
            ),
            root,
            path.to_path_buf(),
        )]),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LspPosition {
    line: u32,
    character: u32,
}

fn byte_offset_to_lsp_position(source: &str, byte_offset: usize) -> LspPosition {
    let mut offset = byte_offset.min(source.len());
    while !source.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }

    let bytes = source.as_bytes();
    let mut lines = Vec::<(usize, usize)>::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\n' => {
                lines.push((start, cursor));
                cursor += 1;
                start = cursor;
            }
            b'\r' => {
                lines.push((start, cursor));
                cursor += usize::from(bytes.get(cursor + 1) == Some(&b'\n')) + 1;
                start = cursor;
            }
            _ => cursor += 1,
        }
    }
    lines.push((start, source.len()));

    let line_index = lines
        .partition_point(|(line_start, _)| *line_start <= offset)
        .saturating_sub(1);
    let (line_start, content_end) = lines[line_index];
    let logical_offset = offset.min(content_end);
    let character = source[line_start..logical_offset].encode_utf16().count();
    LspPosition {
        line: u32::try_from(line_index).unwrap_or(u32::MAX),
        character: u32::try_from(character).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
fn lsp_diagnostic(diagnostic: &EngineDiagnostic, diagnostic_root: Option<&Path>) -> Value {
    lsp_diagnostic_with_paths(diagnostic, diagnostic_root, &BTreeMap::new(), None)
}

fn lsp_diagnostic_with_paths(
    diagnostic: &EngineDiagnostic,
    diagnostic_root: Option<&Path>,
    source_paths: &BTreeMap<nexa::SourceIdentity, PathBuf>,
    snapshot: Option<&WorkspaceSnapshot<'_>>,
) -> Value {
    let range = diagnostic
        .diagnostic
        .primary
        .as_ref()
        .zip(diagnostic.file.as_ref())
        .map_or_else(
            || json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}),
            |(label, file)| {
                let Some(source) = diagnostic.source_by_identity(file) else {
                    return json!({
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    });
                };
                let start = byte_offset_to_lsp_position(source.text(), label.span.start as usize);
                let end = byte_offset_to_lsp_position(source.text(), label.span.end as usize);
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
            let source = diagnostic.source_by_identity(file)?;
            let start = byte_offset_to_lsp_position(source.text(), span.start as usize);
            let end = byte_offset_to_lsp_position(source.text(), span.end as usize);
            let path = if let Some(path) = source_paths.get(file) {
                path.clone()
            } else if file.package_id().is_none() {
                let relative = Path::new(file.path());
                if relative.is_absolute() {
                    relative.to_path_buf()
                } else if let Some(document) =
                    snapshot.and_then(|snapshot| snapshot.document_for_relative_path(relative))
                {
                    // A relative standalone identity (e.g. a bare `api.nidl` contract) is
                    // authoritative when the editor has that file open; prefer the open document
                    // URI over a path guessed under the diagnostic root.
                    document.path.clone()
                } else {
                    diagnostic_root?.join(relative)
                }
            } else {
                // A related location inside a package we have no identity-to-path mapping for
                // (for example an embedded standard-library source) cannot be rendered as a
                // truthful file URI. Drop it rather than inventing a path under the wrong root.
                return None;
            };
            let uri = snapshot.map_or_else(
                || path_to_file_uri(&path),
                |snapshot| snapshot.uri_for_path(&path),
            );
            Some(json!({
                "location": {
                    "uri": uri.ok()?,
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

fn workspace_roots_from_initialize(params: &Value) -> Vec<PathBuf> {
    let mut roots = params
        .get("workspaceFolders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|folder| folder.get("uri").and_then(Value::as_str))
        .filter_map(|uri| file_uri_to_path(uri).ok())
        .map(|root| lexical_path(&root))
        .collect::<Vec<_>>();
    if roots.is_empty()
        && let Some(root_uri) = params.get("rootUri").and_then(Value::as_str)
        && let Ok(root) = file_uri_to_path(root_uri)
    {
        roots.push(lexical_path(&root));
    }
    let mut unique = Vec::with_capacity(roots.len());
    for root in roots {
        if !unique
            .iter()
            .any(|known: &PathBuf| same_file_path(known, &root))
        {
            unique.push(root);
        }
    }
    unique.sort();
    unique
}

#[derive(Default)]
struct WorkspaceFolderChange {
    removed: Vec<PathBuf>,
    changes: Vec<BuildInputChange>,
}

fn apply_workspace_folder_change(roots: &mut Vec<PathBuf>, event: &Value) -> WorkspaceFolderChange {
    let mut result = WorkspaceFolderChange::default();
    for removed in event
        .get("removed")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(uri) = removed.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let Ok(path) = file_uri_to_path(uri) else {
            continue;
        };
        let path = lexical_path(&path);
        let was_present = roots.iter().any(|root| same_file_path(root, &path));
        roots.retain(|root| !same_file_path(root, &path));
        if was_present {
            result.removed.push(path.clone());
            result.changes.push(BuildInputChange {
                path: path.join("nexa.dev.toml"),
                input: BuildInputKind::WorkspaceManifest,
                change: ChangeKind::WorkspaceRemoved,
            });
        }
    }
    for added in event
        .get("added")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(uri) = added.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let Ok(path) = file_uri_to_path(uri) else {
            continue;
        };
        let path = lexical_path(&path);
        if !roots.iter().any(|root| same_file_path(root, &path)) {
            result.changes.push(BuildInputChange {
                path: path.join("nexa.dev.toml"),
                input: BuildInputKind::WorkspaceManifest,
                change: ChangeKind::CreatedOnDisk,
            });
            roots.push(path);
        }
    }
    roots.sort();
    result
}

fn path_within(path: &Path, root: &Path) -> bool {
    let path = lexical_path(path);
    let root = lexical_path(root);
    path == root || path.starts_with(root)
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    if left == right || lexical_path(left) == lexical_path(right) {
        return true;
    }
    left.canonicalize()
        .ok()
        .zip(right.canonicalize().ok())
        .is_some_and(|(left, right)| left == right)
}

fn lexical_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::Normal(part) => normalized.push(part),
        }
    }
    if let Ok(canonical) = normalized.canonicalize() {
        return canonical;
    }
    let mut missing = Vec::new();
    let mut cursor = normalized.as_path();
    while let Some(name) = cursor.file_name() {
        missing.push(name.to_owned());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
        if let Ok(mut canonical) = cursor.canonicalize() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return canonical;
        }
    }
    normalized
}

fn find_upward(snapshot: &WorkspaceSnapshot<'_>, path: &Path, name: &str) -> Option<PathBuf> {
    let mut directory = path.parent()?;
    loop {
        let candidate = directory.join(name);
        if snapshot.contains_path(&candidate) {
            return Some(candidate);
        }
        directory = directory.parent()?;
    }
}

fn file_uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let url = Url::parse(uri).map_err(|error| format!("invalid document URI `{uri}`: {error}"))?;
    if url.scheme() != "file" {
        return Err(format!("unsupported document URI scheme: {}", url.scheme()));
    }
    if let Some(host) = url
        .host_str()
        .filter(|host| !host.is_empty() && *host != "localhost")
    {
        let decoded = percent_decoded_path(&url)?;
        return Ok(PathBuf::from(format!("//{host}{decoded}")));
    }
    let mut path = url
        .to_file_path()
        .map_err(|()| format!("document URI is not a valid file path: {uri}"))?;
    let rendered = path.to_string_lossy();
    if rendered.len() >= 4
        && rendered.starts_with('/')
        && rendered.as_bytes()[1].is_ascii_alphabetic()
        && rendered.as_bytes()[2] == b':'
        && rendered.as_bytes()[3] == b'/'
    {
        path = PathBuf::from(&rendered[1..]);
    }
    Ok(path)
}

fn path_to_file_uri(path: &Path) -> Result<String, String> {
    let rendered = path.to_string_lossy().replace('\\', "/");
    if rendered.len() >= 3
        && rendered.as_bytes()[0].is_ascii_alphabetic()
        && rendered.as_bytes()[1] == b':'
        && rendered.as_bytes()[2] == b'/'
    {
        let (drive, remainder) = rendered
            .split_once('/')
            .expect("recognized Windows path contains a separator");
        let mut url = Url::parse(&format!("file:///{drive}/"))
            .map_err(|error| format!("invalid Windows drive `{drive}`: {error}"))?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| "Windows file URL cannot contain path segments")?;
            segments.pop_if_empty();
            segments.extend(remainder.split('/'));
        }
        return Ok(url.into());
    }
    if let Some(unc) = rendered.strip_prefix("//") {
        let (host, path) = unc
            .split_once('/')
            .ok_or("UNC path must contain a server and share")?;
        let mut url = Url::parse(&format!("file://{host}/"))
            .map_err(|error| format!("invalid UNC host `{host}`: {error}"))?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| "UNC file URL cannot contain path segments")?;
            segments.pop_if_empty();
            segments.extend(path.split('/'));
        }
        return Ok(url.into());
    }
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| format!("path is not absolute: {}", path.display()))
}

fn percent_decoded_path(url: &Url) -> Result<String, String> {
    let local = Url::parse(&format!("file://{}", url.path()))
        .map_err(|error| format!("invalid file URI path: {error}"))?;
    local
        .to_file_path()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|()| "file URI path could not be decoded".into())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use nexa_embed::{EngineDiagnosticStage, SourceId};
    use serde_json::json;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let suffix = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("nexa-lsp-{name}-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn framed(value: &serde_json::Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).expect("message JSON");
        let mut message = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        message.extend(body);
        message
    }

    fn decoded_messages(output: Vec<u8>) -> Vec<serde_json::Value> {
        let mut reader = Cursor::new(output);
        let mut messages = Vec::new();
        while let Some(message) = super::read_message(&mut reader).expect("server message") {
            messages.push(message);
        }
        messages
    }

    fn publish_messages(messages: &[serde_json::Value]) -> Vec<&serde_json::Value> {
        messages
            .iter()
            .filter(|message| {
                message.get("method").and_then(serde_json::Value::as_str)
                    == Some("textDocument/publishDiagnostics")
            })
            .collect()
    }

    fn lsp_range_contains_source_range(
        rendered: &serde_json::Value,
        source: &str,
        start: usize,
        end: usize,
    ) {
        let diagnostic_start = (
            rendered["range"]["start"]["line"]
                .as_u64()
                .expect("diagnostic start line"),
            rendered["range"]["start"]["character"]
                .as_u64()
                .expect("diagnostic start character"),
        );
        let diagnostic_end = (
            rendered["range"]["end"]["line"]
                .as_u64()
                .expect("diagnostic end line"),
            rendered["range"]["end"]["character"]
                .as_u64()
                .expect("diagnostic end character"),
        );
        let expected_start = super::byte_offset_to_lsp_position(source, start);
        let expected_end = super::byte_offset_to_lsp_position(source, end);
        assert!(
            diagnostic_start
                <= (
                    u64::from(expected_start.line),
                    u64::from(expected_start.character)
                ),
            "diagnostic starts after the relevant source range: {rendered}"
        );
        assert!(
            diagnostic_end
                >= (
                    u64::from(expected_end.line),
                    u64::from(expected_end.character)
                ),
            "diagnostic ends before the relevant source range: {rendered}"
        );
    }

    fn lsp_range_equals_source_range(
        rendered: &serde_json::Value,
        source: &str,
        start: usize,
        end: usize,
    ) {
        let expected_start = super::byte_offset_to_lsp_position(source, start);
        let expected_end = super::byte_offset_to_lsp_position(source, end);
        assert_eq!(
            rendered["range"]["start"]["line"], expected_start.line,
            "diagnostic start line differs from the exact source range: {rendered}"
        );
        assert_eq!(
            rendered["range"]["start"]["character"], expected_start.character,
            "diagnostic start character differs from the exact source range: {rendered}"
        );
        assert_eq!(
            rendered["range"]["end"]["line"], expected_end.line,
            "diagnostic end line differs from the exact source range: {rendered}"
        );
        assert_eq!(
            rendered["range"]["end"]["character"], expected_end.character,
            "diagnostic end character differs from the exact source range: {rendered}"
        );
    }

    fn nexa_diagnostic_with_code(
        path: &Path,
        source: &str,
        code: nexa::ErrorCode,
    ) -> nexa_embed::EngineDiagnostic {
        super::diagnostics_for_path(path, Some(source))
            .expect("Nexa diagnostics")
            .into_iter()
            .find(|diagnostic| diagnostic.diagnostic.code == code)
            .unwrap_or_else(|| panic!("missing diagnostic {code} for source:\n{source}"))
    }

    #[derive(Clone, Debug)]
    struct RecordedCall {
        roots: Vec<PathBuf>,
        documents: BTreeMap<PathBuf, (String, i64)>,
        resolved_text: BTreeMap<PathBuf, Option<String>>,
        changes: Vec<super::BuildInputChange>,
    }

    #[derive(Default)]
    struct RecordingAnalyzer {
        calls: Vec<RecordedCall>,
    }

    impl super::WorkspaceAnalyzer for RecordingAnalyzer {
        fn analyze(
            &mut self,
            snapshot: &super::WorkspaceSnapshot<'_>,
            changes: &[super::BuildInputChange],
        ) -> Result<super::AnalysisReport, String> {
            let documents = snapshot
                .documents
                .values()
                .map(|document| {
                    (
                        document.path.clone(),
                        (document.text.clone(), document.version),
                    )
                })
                .collect();
            let resolved_text = snapshot
                .known_inputs
                .iter()
                .map(|path| Ok((path.clone(), snapshot.text_for_path(path)?)))
                .collect::<Result<BTreeMap<_, _>, String>>()?;
            self.calls.push(RecordedCall {
                roots: snapshot.roots.to_vec(),
                documents,
                resolved_text,
                changes: changes.to_vec(),
            });
            Ok(super::AnalysisReport {
                checked_paths: snapshot.known_inputs.iter().cloned().collect(),
                diagnostics: Vec::new(),
            })
        }
    }

    struct ScriptedAnalyzer {
        reports: VecDeque<super::AnalysisReport>,
    }

    impl super::WorkspaceAnalyzer for ScriptedAnalyzer {
        fn analyze(
            &mut self,
            _snapshot: &super::WorkspaceSnapshot<'_>,
            _changes: &[super::BuildInputChange],
        ) -> Result<super::AnalysisReport, String> {
            self.reports
                .pop_front()
                .ok_or_else(|| "missing scripted analysis report".to_owned())
        }
    }

    fn located_type_error(path: &Path) -> super::LocatedDiagnostic {
        let source = "fn main(args: Array<string>) -> i32 { return \"error\"; }";
        let mut session = super::PackageAnalysisSession::default();
        super::diagnostics_for_nexa_source(path, source, &mut session)
            .expect("type diagnostics")
            .into_iter()
            .next()
            .expect("type error")
    }

    fn fixture_project_config(source_root: &str, required_entrypoints: &str) -> String {
        format!(
            "schema = 2\n\
             contract = \"api.contract.nexa\"\n\
             required_entrypoints = [{required_entrypoints}]\n\
             [[sources]]\n\
             id = \"fixture\"\n\
             root = \"{source_root}\"\n\
             trust = \"first-party\"\n\
             activation = [\"default-enabled\"]\n\
             capabilities = []\n\
             allow_entitlement = false\n\
             max_packages = 4\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 100000\n\
             heap_objects = 1024\n\
             heap_bytes = 67108864\n\
             string_bytes = 1048576\n\
             collection_bytes = 33554432\n\
             host_resources = 32\n\
             tasks = 4\n\
             release_records = 64\n"
        )
    }

    fn fixture_application_manifest(package_id: &str) -> String {
        format!(
            "schema = 2\n\
             kind = \"application\"\n\
             id = \"{package_id}\"\n\
             name = \"Fixture Application\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n\
             entry = \"app.main\"\n\
             activation = \"default-enabled\"\n"
        )
    }

    fn package_deletion_fixture(name: &str) -> (TestDirectory, PathBuf) {
        let directory = TestDirectory::new(name);
        let application = directory.path().join("packages/app");
        let source = application.join("src/app/main.nexa");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("Application source directory");
        fs::write(
            directory.path().join("nexa.dev.toml"),
            fixture_project_config("packages", ""),
        )
        .expect("project configuration");
        fs::write(
            directory.path().join("api.contract.nexa"),
            "contract FixtureHost;\n",
        )
        .expect("Host contract");
        fs::write(
            application.join("package.toml"),
            fixture_application_manifest("fixture.deletion"),
        )
        .expect("Package Manifest");
        fs::write(
            application.join("nexa.lock"),
            "schema = 1\n\
             root = \"fixture.deletion\"\n\
             \n\
             [[packages]]\n\
             id = \"fixture.deletion\"\n\
             version = \"1.0.0\"\n\
             path = \"app\"\n",
        )
        .expect("Package Lockfile");
        fs::write(&source, "pub fn run() -> i32 { return 1; }\n").expect("Package entry source");
        (directory, source)
    }

    struct DeleteSourceAfterFirstAnalysis {
        analyzer: super::CurrentWorkspaceAnalyzer,
        source: PathBuf,
        diagnostic_messages: Vec<Vec<String>>,
    }

    impl DeleteSourceAfterFirstAnalysis {
        fn new(source: PathBuf) -> Self {
            Self {
                analyzer: super::CurrentWorkspaceAnalyzer::default(),
                source,
                diagnostic_messages: Vec::new(),
            }
        }
    }

    impl super::WorkspaceAnalyzer for DeleteSourceAfterFirstAnalysis {
        fn analyze(
            &mut self,
            snapshot: &super::WorkspaceSnapshot<'_>,
            changes: &[super::BuildInputChange],
        ) -> Result<super::AnalysisReport, String> {
            let report = super::WorkspaceAnalyzer::analyze(&mut self.analyzer, snapshot, changes)?;
            self.diagnostic_messages.push(
                report
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.diagnostic.diagnostic.message.to_string())
                    .collect(),
            );
            if self.diagnostic_messages.len() == 1 {
                fs::remove_file(&self.source).map_err(|error| {
                    format!(
                        "could not delete Package entry source {}: {error}",
                        self.source.display()
                    )
                })?;
            }
            Ok(report)
        }
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
    fn lsp_utf8_byte_positions_handle_astral_text_and_crlf_exactly() {
        let source = "a😀\r\n界b\rz";
        let position = |offset| super::byte_offset_to_lsp_position(source, offset);
        assert_eq!(position(0), super::LspPosition::default());
        assert_eq!(
            position(1),
            super::LspPosition {
                line: 0,
                character: 1
            }
        );
        assert_eq!(position(3), position(1), "floors an astral byte offset");
        assert_eq!(
            position(5),
            super::LspPosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(position(6), position(5), "CRLF has no UTF-16 column");
        assert_eq!(
            position(7),
            super::LspPosition {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            position(10),
            super::LspPosition {
                line: 1,
                character: 1
            }
        );
        assert_eq!(
            position(12),
            super::LspPosition {
                line: 2,
                character: 0
            }
        );
    }

    #[test]
    fn lsp_candidate_generation_tracks_fingerprint_changes_and_aba() {
        let path = Path::new("/tmp/nexa-lsp-generation.nexa");
        let first =
            crate::project::virtual_snippet("fn value() -> i32 { return 1; }", path).expect("A");
        let second =
            crate::project::virtual_snippet("fn value() -> i32 { return 2; }", path).expect("B");
        let restored =
            crate::project::virtual_snippet("fn value() -> i32 { return 1; }", path).expect("A2");
        assert_eq!(first.build_fingerprint, restored.build_fingerprint);
        assert_ne!(first.build_fingerprint, second.build_fingerprint);

        let mut session = super::PackageAnalysisSession::default();
        assert_eq!(session.prepare(&first), 1);
        assert_eq!(
            session.prepare(&first),
            1,
            "same fingerprint is the same generation"
        );
        assert_eq!(session.prepare(&second), 2);
        assert_eq!(
            session.prepare(&restored),
            3,
            "ABA restoration is a new Candidate generation"
        );
    }

    #[test]
    fn lsp_snapshot_retains_every_overlay_and_close_restores_disk() {
        let directory = TestDirectory::new("snapshot");
        let nexa_path = directory.path().join("main.nexa");
        let nidl_path = directory.path().join("api.contract.nexa");
        fs::write(&nexa_path, "disk nexa").expect("disk Nexa");
        fs::write(&nidl_path, "disk nidl").expect("disk NIDL");
        let nexa_uri = super::path_to_file_uri(&nexa_path).expect("Nexa URI");
        let nidl_uri = super::path_to_file_uri(&nidl_path).expect("NIDL URI");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "rootUri":super::path_to_file_uri(directory.path()).expect("root URI")
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":nexa_uri,"languageId":"nexa","version":1,"text":"overlay nexa 1"}
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":nidl_uri,"languageId":"nexa-idl","version":4,"text":"overlay nidl 1"}
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
                "textDocument":{"uri":nexa_uri,"version":2},
                "contentChanges":[{"text":"overlay nexa 2"}]
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didSave","params":{
                "textDocument":{"uri":nidl_uri},"text":"overlay nidl 2"
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{
                "textDocument":{"uri":nexa_uri}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut analyzer = RecordingAnalyzer::default();
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut Vec::new(), &mut analyzer)
            .expect("snapshot session");

        assert_eq!(analyzer.calls.len(), 5);
        let after_change = &analyzer.calls[2];
        let nexa_identity = super::lexical_path(&nexa_path);
        let nidl_identity = super::lexical_path(&nidl_path);
        assert_eq!(
            after_change.documents.get(&nexa_identity),
            Some(&("overlay nexa 2".to_owned(), 2))
        );
        assert_eq!(
            after_change.documents.get(&nidl_identity),
            Some(&("overlay nidl 1".to_owned(), 4))
        );
        let after_save = &analyzer.calls[3];
        assert_eq!(
            after_save.documents.get(&nidl_identity),
            Some(&("overlay nidl 2".to_owned(), 4)),
            "didSave preserves the document version"
        );
        let after_close = &analyzer.calls[4];
        assert!(!after_close.documents.contains_key(&nexa_identity));
        assert_eq!(
            after_close.resolved_text.get(&nexa_identity),
            Some(&Some("disk nexa".to_owned()))
        );
        assert_eq!(after_close.changes[0].change, super::ChangeKind::Closed);
    }

    #[test]
    fn lsp_workspace_folder_add_analyzes_immediately_and_remove_clears_diagnostics() {
        let directory = TestDirectory::new("workspace-folders");
        let application = directory.path().join("packages/app");
        let source = application.join("src/app/main.nexa");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("Application source directory");
        fs::write(
            directory.path().join("nexa.dev.toml"),
            fixture_project_config("packages", ""),
        )
        .expect("project configuration");
        fs::write(
            directory.path().join("api.contract.nexa"),
            "contract FixtureHost;\n",
        )
        .expect("Host contract");
        fs::write(
            application.join("package.toml"),
            fixture_application_manifest("fixture.workspace"),
        )
        .expect("Package Manifest");
        fs::write(&source, "pub fn run() -> i32 { return \"wrong\"; }\n")
            .expect("invalid Package source");

        #[cfg(unix)]
        let workspace_path = {
            let alias = directory.path().join("workspace-alias");
            std::os::unix::fs::symlink(directory.path(), &alias).expect("workspace path alias");
            assert!(super::same_file_path(&alias, directory.path()));
            alias
        };
        #[cfg(not(unix))]
        let workspace_path = directory.path().to_path_buf();
        let root_uri = super::path_to_file_uri(&workspace_path).expect("workspace URI");
        let removed_root_uri =
            super::path_to_file_uri(directory.path()).expect("removed workspace URI");
        let added_folder = json!({"uri":root_uri,"name":"fixture"});
        let removed_folder = json!({"uri":removed_root_uri,"name":"fixture"});
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "workspaceFolders":[]
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWorkspaceFolders","params":{
                "event":{"added":[added_folder],"removed":[]}
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWorkspaceFolders","params":{
                "event":{"added":[],"removed":[removed_folder]}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        super::run_session(&mut Cursor::new(input), &mut output).expect("workspace-folder session");
        let messages = decoded_messages(output);
        let published = publish_messages(&messages);
        let diagnostic_index = published
            .iter()
            .position(|message| {
                message["params"]["uri"]
                    .as_str()
                    .and_then(|uri| super::file_uri_to_path(uri).ok())
                    .is_some_and(|path| super::same_file_path(&path, &source))
                    && message["params"]["diagnostics"][0]["code"] == "NX2101"
            })
            .unwrap_or_else(|| panic!("added workspace diagnostic: {published:?}"));
        assert!(
            published[diagnostic_index + 1..].iter().any(|message| {
                message["params"]["uri"]
                    .as_str()
                    .and_then(|uri| super::file_uri_to_path(uri).ok())
                    .is_some_and(|path| super::same_file_path(&path, &source))
                    && message["params"]["diagnostics"] == json!([])
            }),
            "removed workspace diagnostics were not cleared: {published:?}"
        );
    }

    #[test]
    fn lsp_workspace_folder_change_reaches_the_analyzer_with_new_roots() {
        let directory = TestDirectory::new("workspace-root-snapshot");
        let root_uri = super::path_to_file_uri(directory.path()).expect("workspace URI");
        let folder = json!({"uri":root_uri,"name":"fixture"});
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "workspaceFolders":[]
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWorkspaceFolders","params":{
                "event":{"added":[folder.clone()],"removed":[]}
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWorkspaceFolders","params":{
                "event":{"added":[],"removed":[folder]}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut analyzer = RecordingAnalyzer::default();
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut Vec::new(), &mut analyzer)
            .expect("workspace-folder recording session");
        assert_eq!(analyzer.calls.len(), 2);
        assert_eq!(analyzer.calls[0].roots.len(), 1);
        assert!(super::same_file_path(
            &analyzer.calls[0].roots[0],
            directory.path()
        ));
        assert!(analyzer.calls[1].roots.is_empty());
        assert_eq!(
            analyzer.calls[0].changes[0].change,
            super::ChangeKind::CreatedOnDisk
        );
        assert_eq!(
            analyzer.calls[1].changes[0].change,
            super::ChangeKind::WorkspaceRemoved
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_project_load_uses_a_valid_nidl_overlay_over_invalid_disk_text() {
        let directory = TestDirectory::new("nidl-project-overlay");
        let packages = directory.path().join("packages");
        let contract = directory.path().join("api.contract.nexa");
        let config = directory.path().join("nexa.dev.toml");
        fs::create_dir_all(&packages).expect("package source root");
        fs::write(&contract, "contract Broken").expect("invalid disk NIDL");
        fs::write(
            &config,
            "schema = 2\n\
             contract = \"api.contract.nexa\"\n\
             required_entrypoints = [\"run\"]\n\
             [[sources]]\n\
             id = \"fixture\"\n\
             root = \"packages\"\n\
             trust = \"first-party\"\n\
             activation = [\"programmatic\"]\n\
             capabilities = []\n\
             allow_entitlement = false\n\
             max_packages = 1\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 100000\n\
             heap_objects = 1024\n\
             heap_bytes = 67108864\n\
             string_bytes = 1048576\n\
             collection_bytes = 33554432\n\
             host_resources = 32\n\
             tasks = 4\n\
             release_records = 64\n",
        )
        .expect("project configuration");

        assert!(
            crate::project::LoadedProject::load(&config).is_err(),
            "the disk NIDL is intentionally invalid"
        );
        let project = crate::project::LoadedProject::load_with_overlays(&config, |requested| {
            super::same_file_path(requested, &contract)
                .then(|| "contract OverlayHost; nexa { fn run() -> i32; }\n".to_owned())
        })
        .expect("valid editor NIDL overlay");
        assert_eq!(project.contract.name, "OverlayHost");
        assert_eq!(
            project.contract_source,
            "contract OverlayHost; nexa { fn run() -> i32; }\n"
        );

        fs::write(&contract, "contract DiskHost; nexa { fn run() -> i32; }\n")
            .expect("valid disk NIDL");
        let uri = super::path_to_file_uri(&contract).expect("contract URI");
        let documents = BTreeMap::from([(
            uri.clone(),
            super::OpenDocument {
                uri,
                path: contract.clone(),
                text: "contract Broken".to_owned(),
                version: 2,
            },
        )]);
        let known_inputs = [contract.clone()].into_iter().collect();
        let roots = [directory.path().to_path_buf()];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        let mut analyzer = super::CurrentWorkspaceAnalyzer::default();
        let report = super::WorkspaceAnalyzer::analyze(
            &mut analyzer,
            &snapshot,
            &[super::BuildInputChange {
                path: contract.clone(),
                input: super::BuildInputKind::ContractSource,
                change: super::ChangeKind::Changed,
            }],
        )
        .expect("invalid NIDL overlay analysis");
        assert_eq!(
            report.diagnostics.len(),
            1,
            "invalid NIDL overlay must not produce a generic project-load duplicate"
        );
        assert_eq!(
            report.diagnostics[0].diagnostic.diagnostic.code,
            nexa::ErrorCode::new(super::contract_error_kind_category(
                nexa::ContractErrorKind::Syntax
            ))
        );

        let missing_entrypoint_uri =
            super::path_to_file_uri(&contract).expect("missing entrypoint contract URI");
        let missing_entrypoint_documents = BTreeMap::from([(
            missing_entrypoint_uri.clone(),
            super::OpenDocument {
                uri: missing_entrypoint_uri,
                path: contract.clone(),
                text: "contract MissingExport;\n".to_owned(),
                version: 3,
            },
        )]);
        let missing_entrypoint_snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &missing_entrypoint_documents,
            known_inputs: &known_inputs,
        };
        let missing_entrypoint = super::WorkspaceAnalyzer::analyze(
            &mut analyzer,
            &missing_entrypoint_snapshot,
            &[super::BuildInputChange {
                path: contract.clone(),
                input: super::BuildInputKind::ContractSource,
                change: super::ChangeKind::Changed,
            }],
        )
        .expect("missing required entrypoint analysis");
        assert_eq!(
            missing_entrypoint.diagnostics.len(),
            1,
            "a missing required entrypoint must be reported once"
        );
        assert_eq!(
            missing_entrypoint.diagnostics[0].diagnostic.diagnostic.code,
            nexa::ErrorCode::NX7010
        );
        assert!(super::same_file_path(
            &missing_entrypoint.diagnostics[0].source_path(),
            &contract
        ));
    }

    #[test]
    fn lsp_overlay_only_workspace_and_renamed_package_manifests_build_resolved_input() {
        let directory = TestDirectory::new("overlay-manifest-add-rename");
        let workspace = directory.path().join("renamed-workspace");
        let application = workspace.join("packages/renamed-package");
        let source = application.join("src/app/main.nexa");
        let config = workspace.join("nexa.dev.toml");
        let manifest = application.join("package.toml");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("renamed Package source directory");
        fs::write(
            workspace.join("api.contract.nexa"),
            "contract FixtureHost; nexa { fn run() -> i32; }\n",
        )
        .expect("Host contract");
        fs::write(&source, "pub fn run() -> i32 { return 7; }\n").expect("Package source");
        assert!(!config.exists(), "workspace Manifest must be overlay-only");
        assert!(!manifest.exists(), "Package Manifest must be overlay-only");

        let config_uri = super::path_to_file_uri(&config).expect("workspace Manifest URI");
        let manifest_uri = super::path_to_file_uri(&manifest).expect("Package Manifest URI");
        let documents = BTreeMap::from([
            (
                config_uri.clone(),
                super::OpenDocument {
                    uri: config_uri,
                    path: config.clone(),
                    text: fixture_project_config("packages", "\"run\""),
                    version: 1,
                },
            ),
            (
                manifest_uri.clone(),
                super::OpenDocument {
                    uri: manifest_uri,
                    path: manifest.clone(),
                    text: fixture_application_manifest("fixture.renamed"),
                    version: 2,
                },
            ),
        ]);
        let known_inputs = [config.clone(), manifest.clone(), source]
            .into_iter()
            .collect();
        let roots = [workspace];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        crate::project::LoadedProject::load_editor_snapshot(&config, |path| {
            snapshot.overlay_for_path(path).map(str::to_owned)
        })
        .unwrap_or_else(|error| panic!("overlay-only project load failed: {error}"));
        let changes = [
            super::BuildInputChange {
                path: config,
                input: super::BuildInputKind::WorkspaceManifest,
                change: super::ChangeKind::Opened,
            },
            super::BuildInputChange {
                path: manifest,
                input: super::BuildInputKind::PackageManifest,
                change: super::ChangeKind::Opened,
            },
        ];
        let mut analyzer = super::CurrentWorkspaceAnalyzer::default();
        let report = super::WorkspaceAnalyzer::analyze(&mut analyzer, &snapshot, &changes)
            .expect("overlay-only Manifest analysis");
        assert!(
            report.diagnostics.is_empty(),
            "overlay-only Manifests must produce a canonical ResolvedBuildInput: {:?}",
            report
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{:?}", diagnostic.diagnostic))
                .collect::<Vec<_>>()
        );
        let package = analyzer
            .package_sessions
            .get(&super::lexical_path(&application))
            .expect("renamed overlay Package session");
        assert_eq!(
            package.package_id.as_ref().map(ToString::to_string),
            Some("fixture.renamed".to_owned())
        );
        assert!(
            package.build_fingerprint.is_some(),
            "the overlay Package must reach ResolvedBuildInput compilation"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_dependency_overlay_rebuilds_input_file_ids_and_candidate_fingerprint() {
        let directory = TestDirectory::new("dependency-identity");
        let application = directory.path().join("application");
        let dependency = directory.path().join("library");
        fs::create_dir_all(application.join("src")).expect("Application source directory");
        fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
        fs::write(
            application.join("package.toml"),
            "schema = 2\n\
             kind = \"application\"\n\
             id = \"fixture.application\"\n\
             name = \"Fixture Application\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n\
             entry = \"main\"\n\
             activation = \"programmatic\"\n\
             [dependencies]\n\
             helper = { path = \"../library\" }\n",
        )
        .expect("Application Manifest");
        fs::write(
            dependency.join("package.toml"),
            "schema = 2\n\
             kind = \"library\"\n\
             id = \"fixture.library\"\n\
             name = \"Fixture Library\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n",
        )
        .expect("dependency Manifest");
        fs::write(application.join("src/main.nexa"), "").expect("Application source");
        fs::write(dependency.join("src/helper.nexa"), "").expect("dependency source");

        let build = crate::project::resolve_direct_package_for_lock(
            &application,
            nexa_analysis::SourceId::new("lsp-test").expect("Source ID"),
        )
        .expect("resolved build");
        let old_fingerprint = build.build_fingerprint;
        let host_idl = nexa::parse_contract("contract NexaCliEmptyHost; host {}\n")
            .expect("built-in Host IDL");
        let host_contract = nexa::HostContractInput::canonical(&host_idl);
        let dependency = build
            .packages
            .values()
            .find(|package| package.manifest.id.as_str() == "fixture.library")
            .expect("resolved dependency");
        let old_dependency_fingerprint =
            nexa_analysis::source_set_fingerprint(&dependency.production_sources);
        let changed_path = dependency.directory.join("src/helper.nexa");
        let added_path = dependency.directory.join("src/extra.nexa");
        let changed_uri = super::path_to_file_uri(&changed_path).expect("changed URI");
        let added_uri = super::path_to_file_uri(&added_path).expect("added URI");
        let documents = BTreeMap::from([
            (
                changed_uri.clone(),
                super::OpenDocument {
                    uri: changed_uri,
                    path: changed_path.clone(),
                    text: "pub fn changed() -> i32 { return 2; }\n".to_owned(),
                    version: 2,
                },
            ),
            (
                added_uri.clone(),
                super::OpenDocument {
                    uri: added_uri,
                    path: added_path.clone(),
                    text: "pub fn added() -> i32 { return 3; }\n".to_owned(),
                    version: 1,
                },
            ),
        ]);
        let known_inputs = [changed_path, added_path].into_iter().collect();
        let roots = Vec::new();
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };

        let rebuilt = super::resolved_build_with_overlays(&build, &snapshot, &host_contract)
            .expect("overlay build");

        assert_ne!(rebuilt.build_fingerprint, old_fingerprint);
        let rebuilt_dependency = rebuilt
            .packages
            .values()
            .find(|package| package.manifest.id.as_str() == "fixture.library")
            .expect("rebuilt dependency");
        assert_ne!(
            nexa_analysis::source_set_fingerprint(&rebuilt_dependency.production_sources),
            old_dependency_fingerprint,
            "dependency overlays must replace the immutable dependency source snapshot"
        );
        assert_eq!(
            rebuilt.build_fingerprint, rebuilt.input.build_fingerprint,
            "ResolvedBuild and its immutable input must share one fresh identity"
        );
        assert_eq!(
            rebuilt.candidate.build_fingerprint, rebuilt.build_fingerprint,
            "Candidate must not retain the disk snapshot fingerprint"
        );
        assert_eq!(
            rebuilt
                .identity(1)
                .expect("Candidate identity")
                .build_fingerprint,
            rebuilt.build_fingerprint
        );
        assert!(
            rebuilt.input.artifact_files.files().iter().any(|file| {
                file.key.package_id.as_str() == "fixture.library"
                    && file.key.path.as_str() == "src/extra.nexa"
            }),
            "new dependency overlays must receive an Artifact FileId"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_manifest_and_lock_overlays_retarget_the_dependency_closure() {
        let directory = TestDirectory::new("manifest-lock-overlay");
        let packages = directory.path().join("packages");
        let application = packages.join("application");
        let left = packages.join("left");
        let right = packages.join("right");
        for package in [&application, &left, &right] {
            fs::create_dir_all(package.join("src")).expect("Package source directory");
        }
        let manifest = |dependency: &str| {
            format!(
                "schema = 2\n\
                 kind = \"application\"\n\
                 id = \"fixture.application\"\n\
                 name = \"Fixture Application\"\n\
                 version = \"1.0.0\"\n\
                 source_root = \"src\"\n\
                 entry = \"main\"\n\
                 activation = \"programmatic\"\n\
                 [dependencies]\n\
                 helper = {{ path = \"../{dependency}\" }}\n"
            )
        };
        let left_manifest = manifest("left");
        let right_manifest = manifest("right");
        fs::write(application.join("package.toml"), &left_manifest).expect("Application Manifest");
        fs::write(
            left.join("package.toml"),
            "schema = 2\n\
             kind = \"library\"\n\
             id = \"fixture.left\"\n\
             name = \"Fixture Left\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n",
        )
        .expect("left Manifest");
        fs::write(
            right.join("package.toml"),
            "schema = 2\n\
             kind = \"library\"\n\
             id = \"fixture.right\"\n\
             name = \"Fixture Right\"\n\
             version = \"1.0.0\"\n\
             source_root = \"src\"\n",
        )
        .expect("right Manifest");
        fs::write(
            application.join("src/main.nexa"),
            "fn value() -> i32 { return 1; }\n",
        )
        .expect("Application source");
        fs::write(
            left.join("src/value.nexa"),
            "pub fn value() -> i32 { return 1; }\n",
        )
        .expect("left source");
        fs::write(
            right.join("src/value.nexa"),
            "pub fn value() -> i32 { return 2; }\n",
        )
        .expect("right source");
        fs::write(
            directory.path().join("api.contract.nexa"),
            "contract OverlayHost;\n",
        )
        .expect("Host Contract");
        let config = directory.path().join("nexa.dev.toml");
        fs::write(
            &config,
            "schema = 2\n\
             contract = \"api.contract.nexa\"\n\
             [[sources]]\n\
             id = \"fixture\"\n\
             root = \"packages\"\n\
             trust = \"first-party\"\n\
             activation = [\"programmatic\"]\n\
             capabilities = []\n\
             allow_entitlement = false\n\
             max_packages = 3\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 100000\n\
             heap_objects = 1024\n\
             heap_bytes = 67108864\n\
             string_bytes = 1048576\n\
             collection_bytes = 33554432\n\
             host_resources = 32\n\
             tasks = 4\n\
             release_records = 64\n",
        )
        .expect("project configuration");

        let project = crate::project::LoadedProject::load(&config).expect("loaded project");
        let discovered = project
            .package_directories()
            .expect("Package discovery")
            .into_iter()
            .find(|package| super::same_file_path(&package.directory, &application))
            .expect("Application discovery");
        let left_build = project
            .resolve_package_for_lock(&discovered)
            .expect("left dependency build");
        let left_lock = left_build.canonical_lock.render();

        fs::write(application.join("package.toml"), &right_manifest)
            .expect("temporary right Manifest");
        let right_build = project
            .resolve_package_for_lock(&discovered)
            .expect("right dependency build");
        let right_lock = right_build.canonical_lock.render();
        assert_ne!(left_lock, right_lock);

        let manifest_path = application.join("package.toml");
        let lock_path = application.join("nexa.lock");
        fs::write(&manifest_path, "not valid TOML").expect("invalid disk Manifest");
        fs::write(&lock_path, "not valid TOML").expect("invalid disk Lockfile");

        let stale = BTreeMap::from([
            (manifest_path.clone(), right_manifest.clone()),
            (lock_path.clone(), left_lock),
        ]);
        let stale_error = project
            .resolve_package_with_overlays(&discovered, true, &stale)
            .expect_err("stale overlay Lockfile");
        assert!(
            stale_error.to_string().contains("nexa.lock"),
            "{stale_error}"
        );

        let overlays = BTreeMap::from([(manifest_path, right_manifest), (lock_path, right_lock)]);
        let rebuilt = project
            .resolve_package_with_overlays(&discovered, true, &overlays)
            .expect("authoritative Manifest/Lock overlays");
        assert!(
            rebuilt
                .dependency_graph
                .packages
                .contains_key(&nexa_analysis::PackageId::new("fixture.right").expect("right ID"))
        );
        assert!(
            !rebuilt
                .dependency_graph
                .packages
                .contains_key(&nexa_analysis::PackageId::new("fixture.left").expect("left ID"))
        );
        assert_ne!(rebuilt.build_fingerprint, left_build.build_fingerprint);
        assert_eq!(rebuilt.build_fingerprint, rebuilt.input.build_fingerprint);
        assert_eq!(
            rebuilt.candidate.build_fingerprint,
            rebuilt.build_fingerprint
        );

        let manifest_path = application.join("package.toml");
        let lock_path = application.join("nexa.lock");
        let manifest_uri = super::path_to_file_uri(&manifest_path).expect("Manifest URI");
        let lock_uri = super::path_to_file_uri(&lock_path).expect("Lockfile URI");
        let documents = BTreeMap::from([
            (
                manifest_uri.clone(),
                super::OpenDocument {
                    uri: manifest_uri,
                    path: manifest_path.clone(),
                    text: overlays[&manifest_path].clone(),
                    version: 2,
                },
            ),
            (
                lock_uri.clone(),
                super::OpenDocument {
                    uri: lock_uri,
                    path: lock_path.clone(),
                    text: overlays[&lock_path].clone(),
                    version: 3,
                },
            ),
        ]);
        let known_inputs = [manifest_path.clone(), lock_path.clone()]
            .into_iter()
            .collect();
        let roots = [directory.path().to_path_buf()];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        let mut analyzer = super::CurrentWorkspaceAnalyzer::default();
        let report = super::WorkspaceAnalyzer::analyze(
            &mut analyzer,
            &snapshot,
            &[
                super::BuildInputChange {
                    path: manifest_path,
                    input: super::BuildInputKind::PackageManifest,
                    change: super::ChangeKind::Changed,
                },
                super::BuildInputChange {
                    path: lock_path,
                    input: super::BuildInputKind::Lockfile,
                    change: super::ChangeKind::Changed,
                },
            ],
        )
        .expect("LSP overlay analysis");
        assert!(
            report.diagnostics.is_empty(),
            "valid Manifest/Lock overlays must compile despite invalid disk bytes: {:?}",
            report
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{:?}", diagnostic.diagnostic))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn lsp_source_path_mapping_uses_package_qualified_identity() {
        let directory = TestDirectory::new("source-identity");
        let first_path = directory.path().join("first/src/main.nexa");
        let second_path = directory.path().join("second/src/main.nexa");
        let first = nexa::SourceIdentity::package("fixture.first", "src/main.nexa");
        let second = nexa::SourceIdentity::package("fixture.second", "src/main.nexa");
        let mut located = located_type_error(&first_path);
        located.source_paths = Arc::new(BTreeMap::from([
            (first.clone(), first_path.clone()),
            (second.clone(), second_path.clone()),
        ]));

        located.diagnostic.file = Some(first);
        assert_eq!(located.source_path(), first_path);
        located.diagnostic.file = Some(second);
        assert_eq!(located.source_path(), second_path);
    }

    #[test]
    fn lsp_watched_build_inputs_reach_the_workspace_analyzer() {
        let directory = TestDirectory::new("watch-hooks");
        let paths = [
            directory.path().join("dependency/src/lib.nexa"),
            directory.path().join("package.toml"),
            directory.path().join("nexa.lock"),
            directory.path().join("api.contract.nexa"),
            directory.path().join("nexa.dev.toml"),
        ];
        let changes = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                json!({
                    "uri": super::path_to_file_uri(path).expect("watched URI"),
                    "type": if index == 3 { 1 } else { 2 }
                })
            })
            .collect::<Vec<_>>();
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{
                "changes":changes
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut analyzer = RecordingAnalyzer::default();
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut Vec::new(), &mut analyzer)
            .expect("watch session");

        assert_eq!(analyzer.calls.len(), 1);
        let kinds = analyzer.calls[0]
            .changes
            .iter()
            .map(|change| (change.input, change.change))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                (
                    super::BuildInputKind::NexaSource,
                    super::ChangeKind::ChangedOnDisk
                ),
                (
                    super::BuildInputKind::PackageManifest,
                    super::ChangeKind::ChangedOnDisk
                ),
                (
                    super::BuildInputKind::Lockfile,
                    super::ChangeKind::ChangedOnDisk
                ),
                (
                    super::BuildInputKind::ContractSource,
                    super::ChangeKind::CreatedOnDisk
                ),
                (
                    super::BuildInputKind::WorkspaceManifest,
                    super::ChangeKind::ChangedOnDisk
                ),
            ]
        );
    }

    #[test]
    fn lsp_watched_delete_reanalyzes_package_after_last_known_source_disappears() {
        let (directory, source) = package_deletion_fixture("watched-delete");
        let source_uri = super::path_to_file_uri(&source).expect("source URI");
        let root_uri = super::path_to_file_uri(directory.path()).expect("workspace URI");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "rootUri":root_uri
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{
                "changes":[{"uri":source_uri,"type":2}]
            }}),
            json!({"jsonrpc":"2.0","method":"workspace/didChangeWatchedFiles","params":{
                "changes":[{"uri":source_uri,"type":3}]
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut analyzer = DeleteSourceAfterFirstAnalysis::new(source);
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut output, &mut analyzer)
            .expect("watched-delete session");

        assert_eq!(analyzer.diagnostic_messages.len(), 2);
        assert!(
            analyzer.diagnostic_messages[0].is_empty(),
            "the Package must be valid before its source is deleted"
        );
        assert!(
            analyzer.diagnostic_messages[1]
                .iter()
                .any(|message| message.contains("entry source is missing")),
            "watched deletion skipped affected Package analysis: {:?}",
            analyzer.diagnostic_messages[1]
        );
        assert!(
            publish_messages(&decoded_messages(output))
                .iter()
                .any(|message| {
                    message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|diagnostics| {
                            diagnostics.iter().any(|diagnostic| {
                                diagnostic["message"].as_str().is_some_and(|message| {
                                    message.contains("entry source is missing")
                                })
                            })
                        })
                })
        );
    }

    #[test]
    fn lsp_groups_by_true_source_clears_old_uri_and_uses_each_version() {
        let directory = TestDirectory::new("diagnostic-routing");
        let first_path = directory.path().join("first.nexa");
        let second_path = directory.path().join("second.nexa");
        let first_uri = super::path_to_file_uri(&first_path).expect("first URI");
        let second_uri = super::path_to_file_uri(&second_path).expect("second URI");
        let mut analyzer = ScriptedAnalyzer {
            reports: VecDeque::from([
                super::AnalysisReport {
                    checked_paths: [first_path.clone()].into_iter().collect(),
                    diagnostics: vec![located_type_error(&first_path)],
                },
                super::AnalysisReport {
                    checked_paths: [first_path.clone(), second_path.clone()]
                        .into_iter()
                        .collect(),
                    diagnostics: vec![located_type_error(&first_path)],
                },
                super::AnalysisReport {
                    checked_paths: [first_path.clone(), second_path.clone()]
                        .into_iter()
                        .collect(),
                    diagnostics: vec![located_type_error(&second_path)],
                },
            ]),
        };
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":first_uri,"languageId":"nexa","version":3,"text":"first"}
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":second_uri,"languageId":"nexa","version":7,"text":"second"}
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
                "textDocument":{"uri":first_uri,"version":4},
                "contentChanges":[{"text":"first changed"}]
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut output, &mut analyzer)
            .expect("routing session");
        let messages = decoded_messages(output);
        let published = publish_messages(&messages);
        assert_eq!(published.len(), 5);

        let after_second_open = &published[1..3];
        let first = after_second_open
            .iter()
            .find(|message| message["params"]["uri"] == first_uri)
            .expect("first diagnostic after second open");
        assert_eq!(first["params"]["version"], 3);
        assert_eq!(first["params"]["diagnostics"][0]["code"], "NX2101");
        let second = after_second_open
            .iter()
            .find(|message| message["params"]["uri"] == second_uri)
            .expect("second clear after second open");
        assert_eq!(second["params"]["version"], 7);
        assert_eq!(second["params"]["diagnostics"], json!([]));

        let after_move = &published[3..5];
        let cleared = after_move
            .iter()
            .find(|message| message["params"]["uri"] == first_uri)
            .expect("old source cleared");
        assert_eq!(cleared["params"]["version"], 4);
        assert_eq!(cleared["params"]["diagnostics"], json!([]));
        let moved = after_move
            .iter()
            .find(|message| message["params"]["uri"] == second_uri)
            .expect("new true source diagnostic");
        assert_eq!(moved["params"]["version"], 7);
        assert_eq!(moved["params"]["diagnostics"][0]["code"], "NX2101");
    }

    #[test]
    fn lsp_did_close_reanalyzes_disk_without_an_overlay_version() {
        let directory = TestDirectory::new("close-disk");
        let path = directory.path().join("main.nexa");
        fs::write(
            &path,
            "fn main(args: Array<string>) -> i32 { return \"disk error\"; }",
        )
        .expect("disk source");
        let uri = super::path_to_file_uri(&path).expect("source URI");
        #[cfg(unix)]
        let close_uri = {
            let alias = directory.path().join("main-alias.nexa");
            std::os::unix::fs::symlink(&path, &alias).expect("source path alias");
            super::path_to_file_uri(&alias).expect("source alias URI")
        };
        #[cfg(not(unix))]
        let close_uri = uri.clone();
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{
                    "uri":uri,
                    "languageId":"nexa",
                    "version":11,
                    "text":"fn main(args: Array<string>) -> i32 { return 1; }"
                }
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{
                "textDocument":{"uri":close_uri}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        super::run_session(&mut Cursor::new(input), &mut output).expect("close session");
        let messages = decoded_messages(output);
        let published = publish_messages(&messages);
        assert_eq!(published.len(), 2);
        assert_eq!(published[0]["params"]["version"], 11);
        assert_eq!(published[0]["params"]["diagnostics"], json!([]));
        assert!(published[1]["params"].get("version").is_none());
        assert_eq!(published[1]["params"]["uri"], uri);
        assert_eq!(published[1]["params"]["diagnostics"][0]["code"], "NX2101");
    }

    #[cfg(unix)]
    #[test]
    fn lsp_missing_document_aliases_share_overlay_version_and_close_identity() {
        let directory = TestDirectory::new("missing-document-alias");
        let alias_root = directory.path().join("alias");
        std::os::unix::fs::symlink(directory.path(), &alias_root).expect("workspace path alias");
        let real_path = directory.path().join("missing.nexa");
        let alias_path = alias_root.join("missing.nexa");
        assert!(!real_path.exists());
        assert_eq!(
            super::lexical_path(&real_path),
            super::lexical_path(&alias_path)
        );
        let real_uri = super::path_to_file_uri(&real_path).expect("real source URI");
        let alias_uri = super::path_to_file_uri(&alias_path).expect("alias source URI");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{
                    "uri":alias_uri,
                    "languageId":"nexa",
                    "version":1,
                    "text":"fn main(args: Array<string>) -> i32 { return \"alias error\"; }"
                }
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
                "textDocument":{"uri":real_uri,"version":2},
                "contentChanges":[{"text":"fn main(args: Array<string>) -> i32 { return 2; }"}]
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{
                "textDocument":{"uri":alias_uri,"version":1},
                "contentChanges":[{"text":"fn main(args: Array<string>) -> i32 { return \"stale\"; }"}]
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{
                "textDocument":{"uri":alias_uri}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        super::run_session(&mut Cursor::new(input), &mut output)
            .expect("missing alias document session");
        let decoded = decoded_messages(output);
        let published = publish_messages(&decoded);
        assert_eq!(
            published
                .iter()
                .filter(|message| message["params"]["diagnostics"][0]["code"] == "NX2101")
                .count(),
            1,
            "the stale alias change must not restore the old overlay"
        );
        assert!(published.iter().any(|message| {
            message["params"]["uri"] == alias_uri
                && message["params"]["diagnostics"] == json!([])
                && message["params"]["version"] == 2
        }));
    }

    #[test]
    fn lsp_did_close_reanalyzes_package_after_last_known_source_disappears() {
        let (directory, source) = package_deletion_fixture("close-delete");
        let source_text = fs::read_to_string(&source).expect("Package entry source");
        let source_uri = super::path_to_file_uri(&source).expect("source URI");
        let root_uri = super::path_to_file_uri(directory.path()).expect("workspace URI");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "rootUri":root_uri
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{
                    "uri":source_uri,
                    "languageId":"nexa",
                    "version":1,
                    "text":source_text
                }
            }}),
            json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{
                "textDocument":{"uri":source_uri}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut analyzer = DeleteSourceAfterFirstAnalysis::new(source);
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut output, &mut analyzer)
            .expect("didClose-delete session");

        assert_eq!(analyzer.diagnostic_messages.len(), 2);
        assert!(
            analyzer.diagnostic_messages[0].is_empty(),
            "the Package must be valid before its source is deleted"
        );
        assert!(
            analyzer.diagnostic_messages[1]
                .iter()
                .any(|message| message.contains("entry source is missing")),
            "didClose skipped affected Package analysis: {:?}",
            analyzer.diagnostic_messages[1]
        );
        assert!(
            publish_messages(&decoded_messages(output))
                .iter()
                .any(|message| {
                    message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|diagnostics| {
                            diagnostics.iter().any(|diagnostic| {
                                diagnostic["message"].as_str().is_some_and(|message| {
                                    message.contains("entry source is missing")
                                })
                            })
                        })
                })
        );
    }

    #[test]
    fn lsp_facade_errors_keep_stable_entrypoint_and_host_codes() {
        assert_eq!(
            super::package_build_error_code(&nexa::PackageBuildError::MissingRequiredEntrypoint(
                "run".to_owned()
            )),
            nexa::ErrorCode::NX7010
        );
        let expected_idl = nexa::parse_contract("contract Expected; nexa { fn run() -> i32; }")
            .expect("expected IDL");
        let actual_idl = nexa::parse_contract("contract Actual; nexa { fn run() -> bool; }")
            .expect("actual IDL");
        let mismatch = nexa::PackageBuildError::EntrypointSignatureMismatch {
            name: "run".to_owned(),
            expected: nexa::entrypoint_signature(&expected_idl.nexa_functions[0]),
            actual: nexa::entrypoint_signature(&actual_idl.nexa_functions[0]),
        };
        assert_eq!(
            super::package_build_error_code(&mismatch),
            nexa::ErrorCode::NX7011
        );
        assert_eq!(
            super::package_build_error_code(&nexa::PackageBuildError::HostContractIdMismatch),
            nexa::ErrorCode::NX4001
        );
    }

    #[test]
    fn lsp_utf16_range_handles_unicode_before_error() {
        let source =
            "fn main(args: Array<string>) -> i32 {\n    let label = \"界\";\n    return label;\n}";
        let diagnostics =
            super::diagnostics_for_path(Path::new("/tmp/nexa-lsp-overlay.nexa"), Some(source))
                .expect("diagnostics");
        assert_eq!(diagnostics.len(), 1);
        let rendered = super::lsp_diagnostic(&diagnostics[0], Some(Path::new("/tmp")));
        assert!(rendered["range"]["start"]["character"].is_u64());
        assert_eq!(rendered["code"].as_str(), Some("NX2101"));
    }

    #[test]
    fn lsp_v2_rejects_legacy_surface_at_the_legacy_token() {
        let path = Path::new("/tmp/nexa-lsp-v2-legacy.nexa");
        let source = "module legacy;\nfn main(args: Array<string>) -> i32 { return 1; }\n";
        let diagnostic = nexa_diagnostic_with_code(path, source, nexa::ErrorCode::NX1002);
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.find("module").expect("legacy token");
        lsp_range_contains_source_range(&rendered, source, start, start + "module".len());
    }

    #[test]
    fn lsp_v2_locates_an_unresolved_use_path() {
        let path = Path::new("/tmp/nexa-lsp-v2-use.nexa");
        let source = "use package::missing;\nfn main(args: Array<string>) -> i32 { return 1; }\n";
        let diagnostic = nexa_diagnostic_with_code(path, source, nexa::ErrorCode::NX2703);
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.find("missing").expect("missing path segment");
        lsp_range_contains_source_range(&rendered, source, start, start + "missing".len());
    }

    #[test]
    fn lsp_v2_locates_immutable_struct_field_assignment() {
        let path = Path::new("/tmp/nexa-lsp-v2-field-mutability.nexa");
        let source = "struct Counter {\n    locked: i32,\n}\n\
                      fn change() {\n\
                          const counter: Counter = Counter { locked: 1 };\n\
                          counter.locked = 2;\n\
                      }\n\
                      fn main(args: Array<string>) -> i32 { return 0; }\n";
        let diagnostic = nexa_diagnostic_with_code(path, source, nexa::ErrorCode::NX2501);
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.rfind("counter").expect("assigned binding");
        lsp_range_contains_source_range(&rendered, source, start, start + "counter".len());
    }

    #[test]
    fn lsp_v2_locates_postfix_await_in_a_synchronous_function() {
        let path = Path::new("/tmp/nexa-lsp-v2-await.nexa");
        let source = "async fn load() -> i32 { return 1; }\n\
                      fn main(args: Array<string>) -> i32 { return load().await; }\n";
        let diagnostic = nexa_diagnostic_with_code(path, source, nexa::ErrorCode::NX2301);
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.find(".await").expect("postfix await");
        lsp_range_equals_source_range(&rendered, source, start, start + ".await".len());
    }

    #[test]
    fn lsp_v2_locates_invalid_nidl_attribute_values_in_utf16() {
        let path = Path::new("/tmp/Nexa LSP 属性.contract.nexa");
        let source = "contract Profile;\r\n\
                          /// 🚀界\r\n\
                          host {\r\n\
                              @fuel(\"many\")\r\n\
                              fn load() -> i32;\r\n\
                          }\r\n";
        let diagnostic = super::diagnostics_for_path(path, Some(source))
            .expect("invalid NIDL diagnostics")
            .into_iter()
            .next()
            .expect("invalid NIDL attribute diagnostic");
        assert_eq!(
            diagnostic.diagnostic.code,
            nexa::ErrorCode::new(super::contract_error_kind_category(
                nexa::ContractErrorKind::InvalidAttribute
            ))
        );
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.find("many").expect("invalid fuel value");
        lsp_range_contains_source_range(&rendered, source, start, start + "many".len());
    }

    #[test]
    fn lsp_v2_locates_invalid_standalone_main_signature() {
        let path = Path::new("/tmp/nexa-lsp-v2-main.nexa");
        let source = "fn main(args: i32) -> i32 { return 0; }\n";
        let diagnostic = super::diagnostics_for_path(path, Some(source))
            .expect("invalid standalone main diagnostics")
            .into_iter()
            .find(|diagnostic| {
                diagnostic
                    .diagnostic
                    .message
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("main")
            })
            .unwrap_or_else(|| panic!("missing invalid main signature diagnostic:\n{source}"));
        let rendered = super::lsp_diagnostic(&diagnostic, path.parent());
        let start = source.find("main").expect("main name");
        lsp_range_contains_source_range(&rendered, source, start, start + "main".len());
    }

    #[test]
    fn lsp_idl_diagnostics_clear_after_valid_overlay() {
        let path = Path::new("/tmp/nexa-lsp-overlay.contract.nexa");
        assert_eq!(
            super::diagnostics_for_path(path, Some("contract Broken"))
                .expect("invalid Contract")
                .len(),
            1
        );
        assert!(
            super::diagnostics_for_path(path, Some("contract Valid;"))
                .expect("valid Contract")
                .is_empty()
        );
    }

    #[test]
    fn lsp_idl_diagnostic_uses_the_parser_token_span() {
        let source = "contract Valid;\nhost { fn broken(value: i32) i32; }\n";
        let diagnostics = super::diagnostics_for_path(
            Path::new("/tmp/nexa-lsp-precise.contract.nexa"),
            Some(source),
        )
        .expect("invalid Contract diagnostics");
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        let primary = diagnostic
            .diagnostic
            .primary
            .as_ref()
            .expect("Contract primary Span");
        let actual_start = source.rfind("i32;").expect("actual token");
        assert_eq!(primary.span.start as usize, actual_start);
        assert_eq!(primary.span.end as usize, actual_start + 3);
        let rendered = super::lsp_diagnostic(diagnostic, Some(Path::new("/tmp")));
        assert_eq!(rendered["range"]["start"]["line"], 1);
        assert!(rendered["range"]["start"]["character"].as_u64().unwrap() > 0);
    }

    #[test]
    fn lsp_contract_profile_is_selected_by_contract_nexa_suffix() {
        let path = Path::new("/tmp/nexa-lsp.profile.contract.nexa");
        // The `.contract.nexa` suffix must route to the Contract parser even though the file
        // extension is `nexa`.
        let diagnostics =
            super::diagnostics_for_path(path, Some("contract Broken")).expect("contract parse");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].diagnostic.code,
            nexa::ErrorCode::new(super::contract_error_kind_category(
                nexa::ContractErrorKind::Syntax
            ))
        );
        // The executable `.nexa` source path must not be hijacked into the Contract parser; the
        // malformed fragment yields either a standalone-module diagnostic or none.
        let exe_diagnostics = super::diagnostics_for_path(&exe_path(), Some("contract Broken"))
            .expect("executable path parse");
        assert!(
            !exe_diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .diagnostic
                    .message
                    .to_string()
                    .contains("after contract header")
            }),
            "executable `.nexa` path must not be treated as a Contract file"
        );
    }

    #[test]
    fn lsp_contract_error_kind_categories_map_all_six_families() {
        use nexa::ContractErrorKind;
        let mapping = [
            (ContractErrorKind::Syntax, "contract.01.syntax"),
            (ContractErrorKind::Duplicate, "contract.02.naming"),
            (ContractErrorKind::InvalidName, "contract.02.naming"),
            (ContractErrorKind::UnknownType, "contract.03.type"),
            (ContractErrorKind::InvalidType, "contract.03.type"),
            (ContractErrorKind::RecursiveLayout, "contract.03.type"),
            (ContractErrorKind::InvalidAttribute, "contract.04.attribute"),
            (
                ContractErrorKind::StableIdCollision,
                "contract.05.stable-id",
            ),
            (
                ContractErrorKind::RustNameCollision,
                "contract.06.generated-rust",
            ),
        ];
        for (kind, expected) in mapping {
            assert_eq!(super::contract_error_kind_category(kind), expected);
        }
    }

    #[test]
    fn lsp_legacy_nidl_emits_a_migration_diagnostic_not_a_contract_parse() {
        // Legacy `*.nidl` files must not be parsed by the Contract grammar (no compatibility
        // parser). They receive an actionable migration diagnostic pointing at `*.contract.nexa`.
        let path = Path::new("/tmp/nexa-lsp-migrate.game.nidl");
        let diagnostics = super::diagnostics_for_path(path, Some("contract Game;"))
            .expect("nidl migration diagnostics");
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        assert_eq!(diagnostic.diagnostic.code, nexa::ErrorCode::NX1001);
        let message = diagnostic.diagnostic.message.to_string();
        assert!(
            message.contains("`.nidl` is removed"),
            "migration must flag the old extension: {message}"
        );
        assert!(
            message.contains("contract.nexa"),
            "migration must name the new file: {message}"
        );
        assert!(
            message.contains("contract <Name>;"),
            "migration must mention the flat header: {message}"
        );
        let note = &diagnostic.diagnostic.notes[0];
        assert!(
            note.to_string().contains("migrate to *.contract.nexa"),
            "migration note must guide the rename: {note:?}"
        );
    }

    fn exe_path() -> PathBuf {
        Path::new("/tmp/nexa-lsp.profile.nexa").to_path_buf()
    }

    #[test]
    fn lsp_contract_diagnostics_cover_the_six_category_families() {
        // Each fixture must genuinely trigger the target `ContractErrorKind` as the first error,
        // and the stable category must reach the rendered LSP payload's machine `code`
        // (e.g. `contract.03.type`) -- not a parsed display-note field.
        // (not merely a discarded leaf note).
        let cases: Vec<(&str, &str, &str)> = vec![
            (
                "syntax",
                "contract Broken\nstruct A { x: i32, }",
                "contract.01.syntax",
            ),
            (
                "naming",
                "contract S;\nstruct snake_case { x: i32, }",
                "contract.02.naming",
            ),
            (
                "type",
                "contract T;\nstruct A { x: Missing, }",
                "contract.03.type",
            ),
            (
                "attribute",
                "contract A;\nhost { @fuel(\"many\") fn value() -> i32; }",
                "contract.04.attribute",
            ),
            (
                "stable-id",
                "contract Stable;\n@stable(\"shared\")\nhandle Entity;\n@stable(\"shared\")\nhandle Actor;",
                "contract.05.stable-id",
            ),
            (
                "generated-rust",
                "contract R;\nhost { fn match() -> i32; }",
                "contract.06.generated-rust",
            ),
        ];
        for (label, source, expected_category) in cases {
            let path = Path::new("/tmp/nexa-lsp-cat.contract.nexa");
            let diagnostics = super::diagnostics_for_path(path, Some(source)).expect(label);
            assert_eq!(
                diagnostics.len(),
                1,
                "{label}: expected exactly one diagnostic, got {}",
                diagnostics.len()
            );
            let rendered = super::lsp_diagnostic(&diagnostics[0], path.parent());
            assert_eq!(
                rendered["code"], expected_category,
                "{label}: rendered payload must carry the stable contract category as the machine code"
            );
        }
    }

    #[test]
    fn lsp_contract_outline_exposes_all_six_symbol_kinds() {
        let source = r"
            contract SnakeApi;
            struct Cell { x: i32, y: i32, }
            enum SnakeEvent { Started, Ended, }
            handle Entity;
            host { fn log(message: string); }
            nexa { fn on_event(event: SnakeEvent) -> Array<i32>; }
        ";
        let symbols = super::contract_document_symbols(source);
        let names = symbols
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "SnakeApi",
                "Cell",
                "SnakeEvent",
                "Entity",
                "log",
                "on_event"
            ]
        );
        let kinds = symbols
            .iter()
            .map(|symbol| symbol["kind"].as_u64().unwrap_or(0))
            .collect::<Vec<_>>();
        // Module=2 (Contract), Struct=23, Enum=10, Interface=11 (Handle), Function=12.
        assert_eq!(kinds, vec![2, 23, 10, 11, 12, 12]);
    }

    #[test]
    fn lsp_contract_outline_is_empty_for_executable_source() {
        assert!(super::contract_document_symbols("fn main() -> i32 { return 0; }\n").is_empty());
    }

    #[test]
    fn lsp_document_symbol_request_round_trips_contract_outline() {
        let uri = "file:///tmp/nexa-lsp-symbols.contract.nexa";
        let contract = "\ncontract SnakeApi;\nstruct Cell { x: i32, y: i32, }\nenum SnakeEvent { Started, Ended, }\nhandle Entity;\nhost { fn log(message: string); }\nnexa { fn on_event(event: SnakeEvent) -> Array<i32>; }\n";
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":uri,"languageId":"nexa-contract","version":1,"text":contract}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"textDocument/documentSymbol","params":{
                "textDocument":{"uri":uri}
            }}),
            json!({"jsonrpc":"2.0","id":3,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        super::run_session(&mut reader, &mut output).expect("LSP session");
        // Decode every framed message so assertions lock onto the exact request/response whose
        // id==2 (the documentSymbol request) rather than scanning a raw blob.
        let decoded = decoded_messages(output);
        let initialize = decoded
            .iter()
            .find(|message| message["id"] == json!(1))
            .expect("initialize response");
        assert_eq!(
            initialize["result"]["capabilities"]["documentSymbolProvider"],
            true
        );

        let symbols_response = decoded
            .iter()
            .find(|message| message["id"] == json!(2))
            .expect("documentSymbol response");
        let symbols = symbols_response["result"].as_array().expect("symbol array");
        let names = symbols
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "SnakeApi",
                "Cell",
                "SnakeEvent",
                "Entity",
                "log",
                "on_event"
            ]
        );
        // Module=2, Struct=23, Enum=10, Interface=11, Function=12.
        let kinds = symbols
            .iter()
            .map(|symbol| symbol["kind"].as_u64().unwrap_or(0))
            .collect::<Vec<_>>();
        assert_eq!(kinds, vec![2, 23, 10, 11, 12, 12]);
        // Every symbol must carry a non-empty range and selectionRange.
        for symbol in symbols {
            assert!(symbol["range"]["start"]["line"].is_u64(), "range: {symbol}");
            assert!(symbol["selectionRange"]["start"]["line"].is_u64());
        }
    }

    #[test]
    fn lsp_language_v3_completion_hover_and_semantic_tokens_are_utf16_exact() {
        let uri = "file:///tmp/nexa-language-v3-tools.nexa";
        let source = "fn run() { let marker: string = \"😀\"; let values: Set<i32> = Set::new(); values. }\n";
        let completion_offset =
            source.find("values. }").expect("completion receiver") + "values.".len();
        let completion_position = super::byte_offset_to_lsp_position(source, completion_offset);
        let set_offset = source.find("Set<i32>").expect("Set type");
        let hover_position = super::byte_offset_to_lsp_position(source, set_offset + 1);
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
            json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{
                "textDocument":{"uri":uri,"languageId":"nexa","version":1,"text":source}
            }}),
            json!({"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{
                "textDocument":{"uri":uri},
                "position":{"line":completion_position.line,"character":completion_position.character}
            }}),
            json!({"jsonrpc":"2.0","id":3,"method":"textDocument/hover","params":{
                "textDocument":{"uri":uri},
                "position":{"line":hover_position.line,"character":hover_position.character}
            }}),
            json!({"jsonrpc":"2.0","id":4,"method":"textDocument/semanticTokens/full","params":{
                "textDocument":{"uri":uri}
            }}),
            json!({"jsonrpc":"2.0","id":5,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut output = Vec::new();
        let mut analyzer = RecordingAnalyzer::default();
        super::run_session_with_analyzer(&mut Cursor::new(input), &mut output, &mut analyzer)
            .expect("LSP session");
        let decoded = decoded_messages(output);

        let initialize = decoded
            .iter()
            .find(|message| message["id"] == json!(1))
            .expect("initialize response");
        assert_eq!(initialize["result"]["capabilities"]["hoverProvider"], true);
        assert_eq!(
            initialize["result"]["capabilities"]["semanticTokensProvider"]["full"],
            true
        );

        let completion = decoded
            .iter()
            .find(|message| message["id"] == json!(2))
            .expect("completion response")["result"]
            .as_array()
            .expect("completion items");
        let labels = completion
            .iter()
            .map(|item| item["label"].as_str().expect("completion label"))
            .collect::<Vec<_>>();
        assert_eq!(labels, ["len", "contains", "insert", "remove", "clear"]);

        let hover = &decoded
            .iter()
            .find(|message| message["id"] == json!(3))
            .expect("hover response")["result"];
        let expected_start = super::byte_offset_to_lsp_position(source, set_offset);
        let expected_end = super::byte_offset_to_lsp_position(source, set_offset + 3);
        assert_eq!(
            hover["range"]["start"]["character"],
            expected_start.character
        );
        assert_eq!(hover["range"]["end"]["character"], expected_end.character);
        assert!(
            hover["contents"]["value"]
                .as_str()
                .is_some_and(|contents| contents.contains("Set"))
        );

        let semantic = decoded
            .iter()
            .find(|message| message["id"] == json!(4))
            .expect("semantic token response")["result"]["data"]
            .as_array()
            .expect("semantic token data");
        assert_eq!(semantic.len() % 5, 0);
        assert!(!semantic.is_empty());
    }

    #[test]
    fn lsp_semantic_queries_use_concrete_generic_types_and_source_declarations() {
        let path = Path::new("/tmp/nexa-lsp-semantic-generics.nexa");
        let uri = "file:///tmp/nexa-lsp-semantic-generics.nexa";
        let source = r"
fn identity<T>(value: T) -> T {
    return value;
}

fn calculate() -> i32 {
    return 10;
}

fn main() -> i32 {
    let result = identity(calculate());
    return result.abs();
}
";
        let mut session = super::PackageAnalysisSession::default();
        let diagnostics = super::diagnostics_for_nexa_source(path, source, &mut session)
            .expect("standalone semantic analysis");
        assert_eq!(
            diagnostics.len(),
            0,
            "{}",
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.diagnostic.diagnostic.message.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let document = session
            .semantic
            .as_ref()
            .and_then(|semantic| semantic.document(path))
            .expect("fresh semantic document");

        let call_offset = source.rfind("identity(").expect("generic call") + 2;
        let hover = super::semantic_hover(&document, source, call_offset).expect("semantic hover");
        assert!(
            hover["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("identity<i32>(value: i32) -> i32")),
            "{hover}"
        );

        let completion_offset = source.rfind("result.abs").expect("receiver") + "result.".len();
        let completions = super::semantic_completion_items(&document, source, completion_offset)
            .expect("semantic completion");
        assert!(
            completions.iter().any(|item| item["label"] == "abs"),
            "{completions:?}"
        );

        let documents = BTreeMap::from([(
            uri.to_owned(),
            super::OpenDocument {
                uri: uri.to_owned(),
                path: path.to_path_buf(),
                text: source.to_owned(),
                version: 1,
            },
        )]);
        let location =
            super::semantic_definition_location(&document, source, call_offset, &documents)
                .expect("generic definition location");
        assert_eq!(location["uri"], uri);
        assert_eq!(location["range"]["start"]["line"], 1);

        let invalid = super::diagnostics_for_nexa_source(path, "fn main( {", &mut session)
            .expect("invalid revision diagnostics");
        assert!(!invalid.is_empty());
        assert!(
            session.semantic.is_none(),
            "an invalid current revision must not retain last-known-good semantic data"
        );
    }

    #[test]
    fn lsp_idl_attribute_diagnostic_keeps_the_exact_unicode_source_span() {
        let path = Path::new("/tmp/Nexa Host 合同.contract.nexa");
        let source = "contract Ready;\r\n\
                          /// 🚀界\r\n\
                          host {\r\n\
                              @fuel(\"many\")\r\n\
                              fn value() -> i32;\r\n\
                          }\r\n";
        let diagnostics =
            super::diagnostics_for_path(path, Some(source)).expect("invalid IDL diagnostics");
        assert_eq!(diagnostics.len(), 1);
        let diagnostic = &diagnostics[0];
        let primary = diagnostic
            .diagnostic
            .primary
            .as_ref()
            .expect("NIDL primary Span");
        let start = source.find("\"many\"").expect("invalid attribute value");
        let end = start + "\"many\"".len();
        assert_eq!(primary.span.start as usize, start);
        assert_eq!(primary.span.end as usize, end);
        let identity = diagnostic.file.as_ref().expect("NIDL source identity");
        assert_eq!(identity.package_id(), None);
        assert_eq!(identity.path(), path.to_string_lossy());
        assert_eq!(
            diagnostic
                .source_by_identity(identity)
                .expect("exact NIDL snapshot")
                .text(),
            source
        );
        let leaf = diagnostic.leaf_diagnostic();
        assert_eq!(leaf.labels[0].source, *identity);
        assert_eq!(
            leaf.labels[0].range,
            nexa::ByteRange::new(u32::try_from(start).unwrap(), u32::try_from(end).unwrap())
        );
        let engine_json: serde_json::Value = serde_json::from_str(
            &nexa_embed::DiagnosticRenderer::json(diagnostic).expect("Engine diagnostic JSON"),
        )
        .expect("valid Engine diagnostic JSON");
        assert_eq!(
            engine_json["sourceIdentity"],
            path.to_string_lossy().as_ref()
        );
        let expected_start = super::byte_offset_to_lsp_position(source, start);
        let expected_end = super::byte_offset_to_lsp_position(source, end);
        assert_eq!(engine_json["range"]["start"]["line"], expected_start.line);
        assert_eq!(
            engine_json["range"]["start"]["character"],
            expected_start.character
        );
        assert_eq!(engine_json["range"]["end"]["line"], expected_end.line);
        assert_eq!(
            engine_json["range"]["end"]["character"],
            expected_end.character
        );
        let rendered = super::lsp_diagnostic(diagnostic, Some(Path::new("/tmp")));
        assert_eq!(rendered["code"], "contract.04.attribute");
        assert_eq!(rendered["range"]["start"]["line"], expected_start.line);
        assert_eq!(
            rendered["range"]["start"]["character"],
            expected_start.character
        );
        assert_eq!(rendered["range"]["end"]["line"], expected_end.line);
        assert_eq!(
            rendered["range"]["end"]["character"],
            expected_end.character
        );
    }

    #[test]
    fn file_uri_matrix_uses_standard_percent_encoding() {
        for path in [
            PathBuf::from("/tmp/Nexa 界 # percent% query?.nexa"),
            PathBuf::from("C:/Users/Nexa User/界#%?.nexa"),
            PathBuf::from("//server/share/Nexa User/界#%?.nexa"),
        ] {
            let uri = super::path_to_file_uri(&path).expect("file URI");
            assert!(uri.starts_with("file://"));
            assert!(!uri.contains(' '));
            assert!(!uri.contains('#'));
            assert!(!uri.contains('?'));
            assert!(uri.contains("%23"));
            assert!(uri.contains("%25"));
            let decoded = super::file_uri_to_path(&uri).expect("decoded path");
            assert_eq!(decoded, path, "{uri}");
        }
        assert!(super::file_uri_to_path("https://example.com/main.nexa").is_err());
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
                    "text":"fn main(args: Array<string>) -> i32 { return \"界\"; }"
                }}
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didChange",
                "params":{
                    "textDocument":{"uri":uri,"version":2},
                    "contentChanges":[{"text":"fn main(args: Array<string>) -> i32 { return 1; }"}]
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
        assert!(output.contains("\"code\":\"NX2101\""), "{output}");
        assert!(output.contains("\"diagnostics\":[]"), "{output}");
        assert!(output.contains("\"textDocumentSync\""));
    }

    #[test]
    fn lsp_document_versions_prevent_stale_diagnostics_and_close_clears() {
        let uri = "file:///tmp/nexa-lsp-lifecycle.nexa";
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
                    "text":"fn main(args: Array<string>) -> i32 { return \"界\"; }"
                }}
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didChange",
                "params":{
                    "textDocument":{"uri":uri,"version":2},
                    "contentChanges":[{"text":"fn main(args: Array<string>) -> i32 { return 1; }"}]
                }
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didChange",
                "params":{
                    "textDocument":{"uri":uri,"version":1},
                    "contentChanges":[{"text":"fn main(args: Array<string>) -> i32 { return \"stale\"; }"}]
                }
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didSave",
                "params":{"textDocument":{"uri":uri},"text":"fn main(args: Array<string>) -> i32 { return 2; }"}
            }),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":uri}}
            }),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        super::run_session(&mut reader, &mut output).expect("LSP lifecycle");
        let output = String::from_utf8(output).expect("UTF-8 output");
        assert_eq!(output.matches("\"code\":\"NX2101\"").count(), 1);
        assert!(output.contains("\"version\":1"));
        assert!(output.contains("\"version\":2"));
        assert!(output.matches("\"diagnostics\":[]").count() >= 3);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_invalid_nidl_overlay_keeps_precise_diagnostics_when_disk_is_invalid_too() {
        let directory = TestDirectory::new("nidl-overlay-invalid-both");
        let packages = directory.path().join("packages");
        let contract = directory.path().join("api.contract.nexa");
        let config = directory.path().join("nexa.dev.toml");
        fs::create_dir_all(&packages).expect("package source root");
        fs::write(&contract, "contract Broken").expect("invalid disk NIDL");
        fs::write(
            &config,
            "schema = 2\n\
             contract = \"api.contract.nexa\"\n\
             required_entrypoints = [\"run\"]\n\
             [[sources]]\n\
             id = \"fixture\"\n\
             root = \"packages\"\n\
             trust = \"first-party\"\n\
             activation = [\"programmatic\"]\n\
             capabilities = []\n\
             allow_entitlement = false\n\
             max_packages = 1\n\
             [sources.limits]\n\
             handler_fuel = 20000\n\
             cumulative_budget = 100000\n\
             heap_objects = 1024\n\
             heap_bytes = 67108864\n\
             string_bytes = 1048576\n\
             collection_bytes = 33554432\n\
             host_resources = 32\n\
             tasks = 4\n\
             release_records = 64\n",
        )
        .expect("project configuration");
        assert!(
            crate::project::LoadedProject::load(&config).is_err(),
            "the disk NIDL is intentionally invalid"
        );

        let overlay = "contract AlsoBroken";
        assert!(
            nexa::parse_contract(overlay).is_err(),
            "overlay NIDL is invalid"
        );
        let uri = super::path_to_file_uri(&contract).expect("contract URI");
        let documents = BTreeMap::from([(
            uri.clone(),
            super::OpenDocument {
                uri,
                path: contract.clone(),
                text: overlay.to_owned(),
                version: 2,
            },
        )]);
        let known_inputs = [contract.clone()].into_iter().collect();
        let roots = [directory.path().to_path_buf()];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        let mut analyzer = super::CurrentWorkspaceAnalyzer::default();
        let report = super::WorkspaceAnalyzer::analyze(
            &mut analyzer,
            &snapshot,
            &[super::BuildInputChange {
                path: contract.clone(),
                input: super::BuildInputKind::ContractSource,
                change: super::ChangeKind::Changed,
            }],
        )
        .expect("invalid NIDL overlay analysis");
        assert_eq!(
            report.diagnostics.len(),
            1,
            "an invalid unsaved NIDL overlay must degrade to one precise parse diagnostic, \
             not a generic project-load error plus a duplicate: {:?}",
            report
                .diagnostics
                .iter()
                .map(|diagnostic| format!("{:?}", diagnostic.diagnostic))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            report.diagnostics[0].diagnostic.diagnostic.code,
            nexa::ErrorCode::new(super::contract_error_kind_category(
                nexa::ContractErrorKind::Syntax
            ))
        );
        assert!(
            super::same_file_path(&report.diagnostics[0].source_path(), &contract),
            "the precise diagnostic must be anchored at the NIDL file"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_cross_package_related_information_uses_the_true_file_uri() {
        let directory = TestDirectory::new("related-uri");
        let workspace = directory.path().join("workspace");
        let application = workspace.join("packages/app");
        let source = application.join("src/app/main.nexa");
        let config = workspace.join("nexa.dev.toml");
        let contract = workspace.join("api.contract.nexa");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("Package source directory");
        fs::write(
            &contract,
            "contract FixtureHost; nexa { fn run() -> i32; }\n",
        )
        .expect("Host contract");
        fs::write(&config, fixture_project_config("packages", "\"run\""))
            .expect("project configuration");
        fs::write(
            application.join("package.toml"),
            fixture_application_manifest("fixture.related"),
        )
        .expect("Package Manifest");
        fs::write(&source, "pub fn run() -> bool { return true; }\n").expect("Package source");

        let project = crate::project::LoadedProject::load(&config).expect("loaded project");
        let discovered = project
            .package_directories()
            .expect("Package discovery")
            .into_iter()
            .find(|package| super::same_file_path(&package.directory, &application))
            .expect("Application discovery");
        project
            .resolve_package_for_lock(&discovered)
            .expect("generate nexa.lock");

        let contract_uri = super::path_to_file_uri(&contract).expect("contract URI");
        let source_uri = super::path_to_file_uri(&source).expect("source URI");
        let documents = BTreeMap::from([(
            contract_uri.clone(),
            super::OpenDocument {
                uri: contract_uri.clone(),
                path: contract.clone(),
                text: fs::read_to_string(&contract).expect("contract text"),
                version: 1,
            },
        )]);
        let known_inputs = [contract.clone(), source.clone()].into_iter().collect();
        let roots = [workspace];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        let mut analyzer = super::CurrentWorkspaceAnalyzer::default();
        let report = super::WorkspaceAnalyzer::analyze(
            &mut analyzer,
            &snapshot,
            &[super::BuildInputChange {
                path: source,
                input: super::BuildInputKind::NexaSource,
                change: super::ChangeKind::Opened,
            }],
        )
        .expect("entrypoint mismatch analysis");
        let mismatch = report
            .diagnostics
            .iter()
            .find(|located| located.diagnostic.diagnostic.code == nexa::ErrorCode::NX7011)
            .unwrap_or_else(|| {
                panic!(
                    "missing NX7011 entrypoint mismatch: {:?}",
                    report
                        .diagnostics
                        .iter()
                        .map(|diagnostic| format!("{:?}", diagnostic.diagnostic))
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            !mismatch.diagnostic.related.is_empty(),
            "NX7011 must carry a related location at the Host contract"
        );
        let rendered = super::lsp_diagnostic_with_paths(
            &mismatch.diagnostic,
            Some(&mismatch.root),
            &mismatch.source_paths,
            Some(&snapshot),
        );
        let related = rendered["relatedInformation"]
            .as_array()
            .expect("relatedInformation array");
        assert!(
            related.iter().any(|entry| {
                entry["location"]["uri"] == contract_uri && entry["location"]["uri"] != source_uri
            }),
            "related location must point at the Host contract URI {contract_uri}, got {related:?}"
        );
        for entry in related {
            let uri = entry["location"]["uri"].as_str().expect("related URI");
            assert!(
                uri == contract_uri,
                "every related location must use the true contract file URI: {related:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_related_information_fallback_uses_open_documents_and_drops_unmapped_packages() {
        let identity = |package: Option<&str>, path: &str| {
            package.map_or_else(
                || nexa::SourceIdentity::standalone(path.to_owned()),
                |package| nexa::SourceIdentity::package(package, path),
            )
        };
        let mapped = identity(Some("fixture.lib"), "src/helper.nexa");
        let unmapped = identity(Some("fixture.other"), "src/other.nexa");
        let open_contract = identity(None, "api.contract.nexa");
        let on_disk_contract = identity(None, "other.nidl");
        let mut sources = nexa::SourceSnapshotRegistry::builder();
        for (source, text) in [
            (&mapped, "pub fn value(x: i32) -> i32 { return x; }\n"),
            (&unmapped, "pub fn other() -> i32 { return 1; }\n"),
            (&open_contract, "contract OpenHost;\n"),
            (&on_disk_contract, "contract DiskHost;\n"),
        ] {
            sources
                .insert(source.clone(), text.to_owned())
                .expect("source insert");
        }
        let sources = Arc::new(sources.build());
        let mut leaf = nexa::LeafDiagnostic::new(
            nexa::ErrorCode::NX1002,
            nexa::Severity::Error,
            "fallback fixture",
        )
        .with_label(nexa::LeafLabel::primary(
            mapped.clone(),
            nexa::ByteRange::new(0, 1),
            "primary",
        ));
        for (source, message) in [
            (&mapped, "mapped dependency"),
            (&unmapped, "unmapped dependency"),
            (&open_contract, "open contract"),
            (&on_disk_contract, "on-disk contract"),
        ] {
            leaf = leaf.with_related(nexa::LeafRelatedLocation::new(
                source.clone(),
                nexa::ByteRange::new(0, 1),
                message,
            ));
        }
        let diagnostic = nexa_embed::EngineDiagnostic::from_leaf_diagnostic(
            None,
            SourceId::new("editor").ok(),
            EngineDiagnosticStage::Parse,
            &leaf,
            &sources,
        );
        let source_paths = Arc::new(BTreeMap::from([(
            mapped.clone(),
            PathBuf::from("/tmp/packages/lib/src/helper.nexa"),
        )]));
        let diagnostic_root = PathBuf::from("/tmp/packages/app");
        let open_uri = "file:///editor/api.contract.nexa".to_owned();
        let documents = BTreeMap::from([(
            open_uri.clone(),
            super::OpenDocument {
                uri: open_uri.clone(),
                path: PathBuf::from("/tmp/root/api.contract.nexa"),
                text: "contract OpenHost;\n".to_owned(),
                version: 1,
            },
        )]);
        let known_inputs = BTreeSet::from([PathBuf::from("/tmp/root/api.contract.nexa")]);
        let roots = [PathBuf::from("/tmp/root")];
        let snapshot = super::WorkspaceSnapshot {
            roots: &roots,
            documents: &documents,
            known_inputs: &known_inputs,
        };
        let rendered = super::lsp_diagnostic_with_paths(
            &diagnostic,
            Some(&diagnostic_root),
            &source_paths,
            Some(&snapshot),
        );
        let related = rendered["relatedInformation"]
            .as_array()
            .expect("relatedInformation array");
        let uris = related
            .iter()
            .map(|entry| entry["location"]["uri"].as_str().expect("related URI"))
            .collect::<Vec<_>>();
        assert!(
            uris.contains(&"file:///tmp/packages/lib/src/helper.nexa"),
            "mapped cross-package related location must use its true file URI: {related:?}"
        );
        assert!(
            uris.contains(&"file:///editor/api.contract.nexa"),
            "relative standalone identity must prefer the open document URI: {related:?}"
        );
        assert!(
            uris.contains(&"file:///tmp/packages/app/other.nidl"),
            "relative standalone identity without an open document must stay bounded to the \
             diagnostic root: {related:?}"
        );
        assert!(
            !uris.contains(&"file:///tmp/packages/app/src/other.nexa")
                && !uris.iter().any(|uri| uri.contains("src/other.nexa")),
            "an unmapped package identity must be dropped, never rendered under the wrong root: \
             {related:?}"
        );
        assert_eq!(
            related.len(),
            3,
            "exactly one related entry is dropped: {related:?}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lsp_session_related_information_publishes_the_contract_file_uri() {
        let directory = TestDirectory::new("related-e2e");
        let workspace = directory.path().join("workspace");
        let application = workspace.join("packages/app");
        let source = application.join("src/app/main.nexa");
        let config = workspace.join("nexa.dev.toml");
        let contract = workspace.join("api.contract.nexa");
        fs::create_dir_all(source.parent().expect("source parent"))
            .expect("Package source directory");
        fs::write(
            &contract,
            "contract FixtureHost; nexa { fn run() -> i32; }\n",
        )
        .expect("Host contract");
        fs::write(&config, fixture_project_config("packages", "\"run\""))
            .expect("project configuration");
        fs::write(
            application.join("package.toml"),
            fixture_application_manifest("fixture.e2e"),
        )
        .expect("Package Manifest");
        fs::write(&source, "pub fn run() -> bool { return true; }\n").expect("Package source");

        let project = crate::project::LoadedProject::load(&config).expect("loaded project");
        let discovered = project
            .package_directories()
            .expect("Package discovery")
            .into_iter()
            .find(|package| super::same_file_path(&package.directory, &application))
            .expect("Application discovery");
        project
            .resolve_package_for_lock(&discovered)
            .expect("generate nexa.lock");

        let contract_uri = super::path_to_file_uri(&contract).expect("contract URI");
        let source_uri = super::path_to_file_uri(&source).expect("source URI");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "rootUri":super::path_to_file_uri(&workspace).expect("workspace URI")
            }}),
            json!({
                "jsonrpc":"2.0",
                "method":"textDocument/didOpen",
                "params":{"textDocument":{
                    "uri":source_uri,
                    "languageId":"nexa",
                    "version":1,
                    "text":"pub fn run() -> bool { return true; }\n"
                }}
            }),
            json!({"jsonrpc":"2.0","id":2,"method":"shutdown","params":null}),
            json!({"jsonrpc":"2.0","method":"exit","params":null}),
        ];
        let input = messages.iter().flat_map(framed).collect::<Vec<_>>();
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        super::run_session(&mut reader, &mut output).expect("LSP session");
        let messages = decoded_messages(output);
        let published = publish_messages(&messages);
        let mismatch = published
            .iter()
            .flat_map(|message| {
                message["params"]["diagnostics"]
                    .as_array()
                    .expect("diagnostics array")
                    .iter()
            })
            .find(|diagnostic| diagnostic["code"] == "NX7011")
            .unwrap_or_else(|| panic!("missing NX7011 in published diagnostics"));
        let related = mismatch["relatedInformation"]
            .as_array()
            .expect("relatedInformation array");
        assert!(
            !related.is_empty(),
            "NX7011 must publish its Host contract related location: {mismatch}"
        );
        for entry in related {
            let uri = entry["location"]["uri"].as_str().expect("related URI");
            let related_path = super::file_uri_to_path(uri).expect("related file path");
            assert!(
                super::same_file_path(&related_path, &contract),
                "cross-file related location must publish the true contract file URI (got {uri}, expected {contract_uri})"
            );
            assert_ne!(uri, source_uri);
        }
    }
}
