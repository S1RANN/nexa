const cp = require("node:child_process");
const vscode = require("vscode");

const BUILD_INPUT_GLOB =
  "**/{*.nexa,*.nidl,package.toml,nexa.lock,nexa.dev.toml}";
const BUILD_INPUT_NAMES = new Set([
  "package.toml",
  "nexa.lock",
  "nexa.dev.toml",
]);

let server;
let diagnostics;
let output;
let receiveBuffer = Buffer.alloc(0);
let nextRequestId = 1;
let initializeRequestId;

function isBuildInputUri(uri) {
  if (uri.scheme !== "file") return false;
  const name = uri.path.slice(uri.path.lastIndexOf("/") + 1);
  return (
    name.endsWith(".nexa") ||
    name.endsWith(".nidl") ||
    BUILD_INPUT_NAMES.has(name)
  );
}

function supports(document) {
  return isBuildInputUri(document.uri);
}

function write(message) {
  if (!server || !server.stdin.writable) return;
  const body = Buffer.from(JSON.stringify(message), "utf8");
  server.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
  server.stdin.write(body);
}

function notify(method, params) {
  write({ jsonrpc: "2.0", method, params });
}

function request(method, params) {
  const id = nextRequestId++;
  write({ jsonrpc: "2.0", id, method, params });
  return id;
}

function openDocument(document) {
  if (!supports(document)) return;
  notify("textDocument/didOpen", {
    textDocument: {
      uri: document.uri.toString(),
      languageId: document.languageId,
      version: document.version,
      text: document.getText(),
    },
  });
}

function changeDocument(event) {
  if (!supports(event.document)) return;
  notify("textDocument/didChange", {
    textDocument: {
      uri: event.document.uri.toString(),
      version: event.document.version,
    },
    contentChanges: [{ text: event.document.getText() }],
  });
}

function notifyWatchedFiles(changes) {
  const buildInputChanges = changes
    .filter((change) => isBuildInputUri(change.uri))
    .map((change) => ({
      uri: change.uri.toString(),
      type: change.type,
    }));
  if (buildInputChanges.length === 0) return;
  notify("workspace/didChangeWatchedFiles", {
    changes: buildInputChanges,
  });
}

function workspaceFolder(folder) {
  return {
    uri: folder.uri.toString(),
    name: folder.name,
  };
}

function parseMessages(chunk) {
  receiveBuffer = Buffer.concat([receiveBuffer, chunk]);
  for (;;) {
    const headerEnd = receiveBuffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const header = receiveBuffer.subarray(0, headerEnd).toString("ascii");
    const match = /^Content-Length:\s*(\d+)$/im.exec(header);
    if (!match) {
      receiveBuffer = receiveBuffer.subarray(headerEnd + 4);
      continue;
    }
    const length = Number(match[1]);
    const bodyStart = headerEnd + 4;
    if (receiveBuffer.length < bodyStart + length) return;
    const body = receiveBuffer.subarray(bodyStart, bodyStart + length);
    receiveBuffer = receiveBuffer.subarray(bodyStart + length);
    try {
      handleMessage(JSON.parse(body.toString("utf8")));
    } catch (error) {
      output.appendLine(`Invalid language server message: ${error}`);
    }
  }
}

function handleMessage(message) {
  if (message.id === initializeRequestId) {
    initializeRequestId = undefined;
    notify("initialized", {});
    for (const document of vscode.workspace.textDocuments) openDocument(document);
    return;
  }
  if (message.method !== "textDocument/publishDiagnostics") return;
  const uri = vscode.Uri.parse(message.params.uri);
  const converted = message.params.diagnostics.map((item) => {
    const range = new vscode.Range(
      item.range.start.line,
      item.range.start.character,
      item.range.end.line,
      item.range.end.character,
    );
    const diagnostic = new vscode.Diagnostic(
      range,
      item.message,
      item.severity === 2
        ? vscode.DiagnosticSeverity.Warning
        : vscode.DiagnosticSeverity.Error,
    );
    diagnostic.source = item.source || "nexa";
    if (item.code) {
      diagnostic.code = item.codeDescription?.href
        ? { value: String(item.code), target: vscode.Uri.parse(item.codeDescription.href) }
        : String(item.code);
    }
    if (Array.isArray(item.relatedInformation)) {
      diagnostic.relatedInformation = item.relatedInformation.map(
        (related) =>
          new vscode.DiagnosticRelatedInformation(
            new vscode.Location(
              vscode.Uri.parse(related.location.uri),
              new vscode.Range(
                related.location.range.start.line,
                related.location.range.start.character,
                related.location.range.end.line,
                related.location.range.end.character,
              ),
            ),
            related.message,
          ),
      );
    }
    return diagnostic;
  });
  diagnostics.set(uri, converted);
}

function start() {
  const executable = vscode.workspace
    .getConfiguration("nexa")
    .get("server.path", "nexa");
  receiveBuffer = Buffer.alloc(0);
  server = cp.spawn(executable, ["lsp"], {
    cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
    stdio: ["pipe", "pipe", "pipe"],
  });
  server.stdout.on("data", parseMessages);
  server.stderr.on("data", (chunk) => output.append(chunk.toString("utf8")));
  server.on("error", (error) => {
    output.appendLine(`Could not start Nexa language server: ${error.message}`);
    vscode.window.showErrorMessage(
      `Could not start Nexa language server (${executable}): ${error.message}`,
    );
  });
  server.on("exit", (code, signal) => {
    output.appendLine(`Nexa language server exited: code=${code} signal=${signal}`);
  });
  const workspaceFolders = vscode.workspace.workspaceFolders || [];
  initializeRequestId = request("initialize", {
    processId: process.pid,
    rootUri: workspaceFolders[0]?.uri.toString() || null,
    workspaceFolders: workspaceFolders.map(workspaceFolder),
    capabilities: {
      workspace: {
        workspaceFolders: true,
        didChangeWatchedFiles: {
          dynamicRegistration: false,
        },
      },
    },
  });
}

async function stop() {
  if (!server) return;
  request("shutdown", null);
  notify("exit", null);
  const old = server;
  server = undefined;
  await new Promise((resolve) => {
    const timer = setTimeout(() => {
      old.kill();
      resolve();
    }, 750);
    old.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

function activate(context) {
  diagnostics = vscode.languages.createDiagnosticCollection("nexa");
  output = vscode.window.createOutputChannel("Nexa");
  const buildInputWatcher =
    vscode.workspace.createFileSystemWatcher(BUILD_INPUT_GLOB);
  context.subscriptions.push(diagnostics, output, buildInputWatcher);
  context.subscriptions.push(
    vscode.workspace.onDidOpenTextDocument(openDocument),
    vscode.workspace.onDidChangeTextDocument(changeDocument),
    vscode.workspace.onDidSaveTextDocument((document) => {
      if (!supports(document)) return;
      notify("textDocument/didSave", {
        textDocument: { uri: document.uri.toString() },
        text: document.getText(),
      });
    }),
    vscode.workspace.onDidCloseTextDocument((document) => {
      if (!supports(document)) return;
      diagnostics.delete(document.uri);
      notify("textDocument/didClose", {
        textDocument: { uri: document.uri.toString() },
      });
    }),
    buildInputWatcher.onDidCreate((uri) => {
      notifyWatchedFiles([{ uri, type: 1 }]);
    }),
    buildInputWatcher.onDidChange((uri) => {
      notifyWatchedFiles([{ uri, type: 2 }]);
    }),
    buildInputWatcher.onDidDelete((uri) => {
      notifyWatchedFiles([{ uri, type: 3 }]);
    }),
    vscode.workspace.onDidRenameFiles((event) => {
      notifyWatchedFiles(
        event.files.flatMap((file) => [
          { uri: file.oldUri, type: 3 },
          { uri: file.newUri, type: 1 },
        ]),
      );
    }),
    vscode.workspace.onDidChangeWorkspaceFolders((event) => {
      notify("workspace/didChangeWorkspaceFolders", {
        event: {
          added: event.added.map(workspaceFolder),
          removed: event.removed.map(workspaceFolder),
        },
      });
    }),
    vscode.commands.registerCommand("nexa.restartLanguageServer", async () => {
      await stop();
      diagnostics.clear();
      start();
    }),
  );
  start();
}

async function deactivate() {
  await stop();
}

module.exports = { activate, deactivate };
