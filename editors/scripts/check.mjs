import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

import TOML from "@iarna/toml";
import vscodeOniguruma from "vscode-oniguruma";
import vscodeTextmate from "vscode-textmate";

import {
  editorsDirectory,
  grammarDirectory,
  idlGrammarDirectory,
  readSyntax,
  renderZedManifest,
  repositoryDirectory,
  stableJson,
  textMateGrammars,
  vscodeDirectory,
  zedDirectory,
} from "./lib.mjs";

const generatedGrammarFiles = [
  "src/parser.c",
  "src/grammar.json",
  "src/node-types.json",
];

const require = createRequire(import.meta.url);
const { OnigScanner, OnigString, loadWASM } = vscodeOniguruma;
const { INITIAL, Registry, parseRawGrammar } = vscodeTextmate;

const nexaExamples = [
  "editors/fixtures/m4-language.nexa",
  "examples/add.nexa",
  "examples/combat-runtime/gameplay.nexa",
  "examples/combat-runtime/reload/activation_fault.nexa",
  "examples/combat-runtime/reload/invalid.nexa",
  "examples/combat-runtime/reload/v1.nexa",
  "examples/combat-runtime/reload/v2.nexa",
];
const nidlExamples = [
  "editors/fixtures/m4-language.nidl",
  "examples/combat-runtime/combat_api.nidl",
  "crates/nexa-idl/tests/fixtures/business_host/interface.nidl",
];
const invalidNidlExamples = [
  "editors/fixtures/nidl-comment-invalid.nidl",
  "editors/fixtures/nidl-enum-comment-invalid.nidl",
];

const queryChecks = [
  [
    grammarDirectory,
    "tree-sitter-nexa/queries/highlights.scm",
    "editors/fixtures/m4-language.nexa",
    ["keyword", "function", "attribute", "comment.documentation"],
  ],
  [
    grammarDirectory,
    "zed/languages/nexa/highlights.scm",
    "editors/fixtures/m4-language.nexa",
    ["keyword", "function", "attribute", "comment.documentation"],
  ],
  [
    grammarDirectory,
    "zed/languages/nexa/indents.scm",
    "editors/fixtures/m4-language.nexa",
    ["indent", "end"],
  ],
  [
    grammarDirectory,
    "zed/languages/nexa/brackets.scm",
    "editors/fixtures/m4-language.nexa",
    ["open", "close"],
  ],
  [
    grammarDirectory,
    "zed/languages/nexa/outline.scm",
    "editors/fixtures/m4-language.nexa",
    ["item", "name", "context"],
  ],
  [
    idlGrammarDirectory,
    "tree-sitter-nexa-idl/queries/highlights.scm",
    "editors/fixtures/m4-language.nidl",
    ["keyword", "function", "type", "type.builtin"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-idl/highlights.scm",
    "editors/fixtures/m4-language.nidl",
    ["keyword", "function", "type", "type.builtin"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-idl/indents.scm",
    "editors/fixtures/m4-language.nidl",
    ["indent", "end"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-idl/brackets.scm",
    "editors/fixtures/m4-language.nidl",
    ["open", "close"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-idl/outline.scm",
    "editors/fixtures/m4-language.nidl",
    ["item", "name", "context"],
  ],
];

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function read(file) {
  return fs.readFileSync(file, "utf8");
}

function parseJson(file) {
  return JSON.parse(read(file));
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: repositoryDirectory,
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
    ...options,
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    const output = [result.stdout, result.stderr].filter(Boolean).join("\n");
    throw new Error(`${command} ${args.join(" ")} failed\n${output}`);
  }
  return result;
}

function assertFileEquals(actual, expected, label) {
  assert(read(actual) === read(expected), `${label} is out of date`);
}

function validateJsonFiles() {
  const files = [
    "language-syntax.json",
    "package.json",
    "tree-sitter-nexa/package.json",
    "tree-sitter-nexa/tree-sitter.json",
    "tree-sitter-nexa/src/grammar.json",
    "tree-sitter-nexa/src/node-types.json",
    "tree-sitter-nexa-idl/package.json",
    "tree-sitter-nexa-idl/tree-sitter.json",
    "tree-sitter-nexa-idl/src/grammar.json",
    "tree-sitter-nexa-idl/src/node-types.json",
    "vscode/package.json",
    "vscode/language-configuration/nexa.json",
    "vscode/language-configuration/nexa-idl.json",
    "vscode/syntaxes/nexa.tmLanguage.json",
    "vscode/syntaxes/nexa-idl.tmLanguage.json",
  ];
  for (const file of files) {
    parseJson(path.join(editorsDirectory, file));
  }
}

function validatePackageScripts() {
  const packageJson = parseJson(path.join(editorsDirectory, "package.json"));
  const scripts = packageJson.scripts ?? {};
  assert(
    scripts.generate === "node scripts/generate.mjs" &&
      scripts.check === "node scripts/check.mjs" &&
      scripts["package:vscode"] === "node scripts/package-vscode.mjs" &&
      scripts["prepare:zed"] === "node scripts/prepare-zed.mjs" &&
      scripts["build:zed"] === "node scripts/build-zed.mjs" &&
      scripts["verify:package"] === "node scripts/verify-package.mjs",
    "editor package scripts must use the checked-in generation and packaging pipeline",
  );
  assert(
    scripts.package ===
      "pnpm run generate && pnpm run check && pnpm run package:vscode && pnpm run prepare:zed && pnpm run build:zed && pnpm run verify:package",
    "editor package must generate, check, package VS Code, prepare/build Zed, and verify both artifacts",
  );
}

function validateContributions() {
  const extension = parseJson(path.join(vscodeDirectory, "package.json"));
  const extensionSource = read(path.join(vscodeDirectory, "extension.js"));
  assert(
    extension.main === "./extension.js",
    "VS Code must activate the Nexa language server client",
  );
  assert(
    extensionSource.includes('cp.spawn(executable, ["lsp"]'),
    "VS Code must launch `nexa lsp`",
  );
  for (const required of [
    "BUILD_INPUT_GLOB",
    "*.nexa",
    "*.nidl",
    "package.toml",
    "nexa.lock",
    "nexa.dev.toml",
    "createFileSystemWatcher",
    "workspace/didChangeWatchedFiles",
    "onDidRenameFiles",
    "workspace/didChangeWorkspaceFolders",
    "onDidChangeWorkspaceFolders",
    "workspaceFolders.map(workspaceFolder)",
    "return isBuildInputUri(document.uri)",
    "dynamicRegistration: false",
  ]) {
    assert(
      extensionSource.includes(required),
      `VS Code build-input synchronization is missing ${required}`,
    );
  }

  const languages = new Map(
    extension.contributes.languages.map((language) => [language.id, language]),
  );
  assert(languages.size === 2, "VS Code must contribute exactly two languages");
  assert(
    languages.get("nexa")?.extensions?.includes(".nexa"),
    "VS Code Nexa language contribution is invalid",
  );
  assert(
    languages.get("nexa-idl")?.extensions?.includes(".nidl"),
    "VS Code Nexa IDL language contribution is invalid",
  );

  const grammars = new Map(
    extension.contributes.grammars.map((grammar) => [
      grammar.language,
      grammar.scopeName,
    ]),
  );
  assert(
    grammars.get("nexa") === "source.nexa",
    "VS Code Nexa scope is invalid",
  );
  assert(
    grammars.get("nexa-idl") === "source.nexa-idl",
    "VS Code Nexa IDL scope is invalid",
  );

  const nexaConfig = parseJson(
    path.join(vscodeDirectory, "language-configuration", "nexa.json"),
  );
  assert(
    nexaConfig.comments?.lineComment === "//" &&
      nexaConfig.comments?.blockComment?.[0] === "/*" &&
      nexaConfig.comments?.blockComment?.[1] === "*/",
    "Nexa must register its line and block comments",
  );
  const nidlConfig = parseJson(
    path.join(vscodeDirectory, "language-configuration", "nexa-idl.json"),
  );
  assert(
    !Object.hasOwn(nidlConfig, "comments"),
    "NIDL must not register comments",
  );

  for (const [language, config] of [
    ["nexa", nexaConfig],
    ["nexa-idl", nidlConfig],
  ]) {
    const configuredPairs = [
      ...config.brackets,
      ...config.autoClosingPairs.map((pair) => [pair.open, pair.close]),
      ...config.surroundingPairs,
    ];
    assert(
      !configuredPairs.some(([open, close]) => open === "<" && close === ">"),
      `${language} must not register angle brackets that split arrow operators`,
    );
  }
}

function validateZedFiles() {
  TOML.parse(read(path.join(zedDirectory, "extension.toml.in")));

  const nexaSource = read(
    path.join(zedDirectory, "languages", "nexa", "config.toml"),
  );
  const nexaConfig = TOML.parse(nexaSource);
  assert(nexaConfig.grammar === "nexa", "Nexa must use grammar nexa");
  assert(
    nexaConfig.line_comments?.includes("// "),
    "Nexa must register line comments in Zed",
  );

  const idlSource = read(
    path.join(zedDirectory, "languages", "nexa-idl", "config.toml"),
  );
  const idlConfig = TOML.parse(idlSource);
  assert(
    idlConfig.grammar === "nexa_idl",
    "Nexa IDL must use grammar nexa_idl",
  );
  assert(
    !Object.hasOwn(idlConfig, "line_comments") &&
      !idlSource.includes("line_comments"),
    "Nexa IDL must not register line comments",
  );

  const manifest = TOML.parse(renderZedManifest());
  assert(
    manifest.grammars.nexa.repository === pathToFileURL(grammarDirectory).href,
    "Zed Nexa grammar URL is not the canonical absolute file URL",
  );
  assert(
    manifest.grammars.nexa_idl.repository ===
      pathToFileURL(idlGrammarDirectory).href,
    "Zed NIDL grammar URL is not the canonical absolute file URL",
  );
  assert(
    manifest.grammars.nexa.rev === "local" &&
      manifest.grammars.nexa_idl.rev === "local",
    "Zed local grammar revisions are invalid",
  );
  assert(
    manifest.language_servers?.nexa?.languages?.includes("Nexa") &&
      manifest.language_servers?.nexa?.languages?.includes("Nexa IDL"),
    "Zed must attach the Nexa language server to both languages",
  );
  assert(
    read(path.join(zedDirectory, "src", "lib.rs")).includes(
      'args: vec!["lsp".to_owned()]',
    ),
    "Zed must launch `nexa lsp`",
  );
}

function validateSyntaxContract() {
  const syntax = readSyntax();
  const nexaGrammar = parseJson(
    path.join(grammarDirectory, "src", "grammar.json"),
  );
  const idlGrammar = parseJson(
    path.join(idlGrammarDirectory, "src", "grammar.json"),
  );
  assert(
    nexaGrammar.rules.line_comment &&
      nexaGrammar.rules.block_comment &&
      nexaGrammar.rules.doc_comment,
    "Nexa Tree-sitter grammar must define all M4 comments",
  );
  assert(
    !idlGrammar.rules.line_comment &&
      !idlGrammar.rules.block_comment &&
      !idlGrammar.rules.doc_comment,
    "NIDL Tree-sitter grammar must reject comments",
  );
  assert(
    syntax.nexa.attributeKeywords.includes("stable") &&
      syntax.nexa.attributeKeywords.includes("test"),
    "M4 Nexa attributes are missing",
  );
  assert(
    syntax.nexa.statementKeywords.includes("break") &&
      syntax.nexa.statementKeywords.includes("continue"),
    "M4 loop control keywords are missing",
  );
  assert(
    syntax.nidl.builtinTypes.includes("void"),
    "NIDL builtin types must include void",
  );

  const nexaTextMate = parseJson(
    path.join(vscodeDirectory, "syntaxes", "nexa.tmLanguage.json"),
  );
  const idlTextMate = parseJson(
    path.join(vscodeDirectory, "syntaxes", "nexa-idl.tmLanguage.json"),
  );
  assert(
    Object.hasOwn(nexaTextMate.repository, "comments"),
    "Nexa TextMate grammar must define comments",
  );
  assert(
    !Object.hasOwn(idlTextMate.repository, "comments"),
    "NIDL TextMate grammar must reject comments",
  );
  const operatorPatterns = nexaTextMate.repository.operators.patterns.map(
    (pattern) => pattern.match,
  );
  assert(
    operatorPatterns.some((pattern) => pattern.includes("/")),
    "TextMate grammar must highlight / as an operator",
  );
}

async function validateTextMateTokenization() {
  const wasm = fs.readFileSync(
    require.resolve("vscode-oniguruma/release/onig.wasm"),
  );
  await loadWASM(
    wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength),
  );
  const grammarFiles = new Map([
    [
      "source.nexa",
      path.join(vscodeDirectory, "syntaxes", "nexa.tmLanguage.json"),
    ],
    [
      "source.nexa-idl",
      path.join(vscodeDirectory, "syntaxes", "nexa-idl.tmLanguage.json"),
    ],
  ]);
  const registry = new Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (patterns) => new OnigScanner(patterns),
      createOnigString: (value) => new OnigString(value),
    }),
    loadGrammar: async (scopeName) => {
      const grammarFile = grammarFiles.get(scopeName);
      return grammarFile
        ? parseRawGrammar(read(grammarFile), grammarFile)
        : null;
    },
  });

  const cases = [
    {
      scopeName: "source.nexa",
      line: "pub fn add(a: i32, b: i32) -> i32 {",
      operators: [["->", "keyword.operator.arrow.nexa"]],
    },
    {
      scopeName: "source.nexa",
      line: "Some(found) => found, None => 0,",
      operators: [
        ["=>", "keyword.operator.arrow.nexa"],
        ["=>", "keyword.operator.arrow.nexa"],
      ],
    },
    {
      scopeName: "source.nexa-idl",
      line: "sync fn log(message: string) -> i32;",
      operators: [["->", "keyword.operator.arrow.nexa-idl"]],
    },
  ];

  for (const testCase of cases) {
    const grammar = await registry.loadGrammar(testCase.scopeName);
    assert(grammar, `failed to load ${testCase.scopeName}`);
    const tokens = grammar.tokenizeLine(testCase.line, INITIAL).tokens;
    let searchOffset = 0;
    for (const [operator, expectedScope] of testCase.operators) {
      const start = testCase.line.indexOf(operator, searchOffset);
      const end = start + operator.length;
      const matches = tokens.filter(
        (token) => token.startIndex < end && token.endIndex > start,
      );
      assert(
        matches.length === 1 &&
          matches[0].startIndex === start &&
          matches[0].endIndex === end &&
          matches[0].scopes.includes(expectedScope),
        `${testCase.scopeName} must tokenize ${operator} as one operator`,
      );
      searchOffset = end;
    }
  }
}

function generateAndCompare(source, temporaryDirectory) {
  const name = path.basename(source);
  const temporaryGrammar = path.join(temporaryDirectory, name);
  fs.mkdirSync(temporaryGrammar, { recursive: true });
  for (const file of ["grammar.js", "package.json", "tree-sitter.json"]) {
    fs.copyFileSync(
      path.join(source, file),
      path.join(temporaryGrammar, file),
    );
  }
  run("tree-sitter", ["generate"], {
    cwd: temporaryGrammar,
    env: {
      ...process.env,
      XDG_CACHE_HOME: path.join(temporaryDirectory, "cache"),
    },
  });
  for (const file of generatedGrammarFiles) {
    assertFileEquals(
      path.join(source, file),
      path.join(temporaryGrammar, file),
      `${name}/${file}`,
    );
  }
}

function checkGeneratedFiles(temporaryDirectory) {
  fs.copyFileSync(
    path.join(editorsDirectory, "language-syntax.json"),
    path.join(temporaryDirectory, "language-syntax.json"),
  );
  generateAndCompare(grammarDirectory, temporaryDirectory);
  generateAndCompare(idlGrammarDirectory, temporaryDirectory);

  for (const [file, grammar] of textMateGrammars()) {
    const actual = read(path.join(vscodeDirectory, "syntaxes", file));
    assert(
      actual === stableJson(grammar),
      `vscode/syntaxes/${file} is out of date`,
    );
  }
}

function parseExamples(grammar, files, environment, expectedSuccess) {
  const result = spawnSync(
    "tree-sitter",
    [
      "parse",
      "--grammar-path",
      grammar,
      "--json-summary",
      ...files,
    ],
    {
      cwd: repositoryDirectory,
      encoding: "utf8",
      maxBuffer: 16 * 1024 * 1024,
      env: environment,
    },
  );
  if (result.error) {
    throw result.error;
  }
  const jsonStart = result.stdout.indexOf('{\n  "parse_summaries"');
  assert(jsonStart >= 0, "Tree-sitter did not emit a JSON parse summary");
  const summary = JSON.parse(result.stdout.slice(jsonStart));
  assert(
    summary.source_count === files.length,
    "Tree-sitter did not parse every required example",
  );
  const matchesExpectation = summary.parse_summaries.every(
    (item) => item.successful === expectedSuccess,
  );
  assert(
    matchesExpectation,
    expectedSuccess
      ? "A required example contains ERROR or MISSING nodes"
      : "NIDL unexpectedly accepted comment syntax in at least one invalid fixture",
  );
}

function validateExamplesAndQueries(temporaryDirectory) {
  const environment = {
    ...process.env,
    XDG_CACHE_HOME: path.join(temporaryDirectory, "cache"),
  };
  parseExamples(grammarDirectory, nexaExamples, environment, true);
  parseExamples(idlGrammarDirectory, nidlExamples, environment, true);
  parseExamples(
    idlGrammarDirectory,
    invalidNidlExamples,
    environment,
    false,
  );

  for (const [grammar, query, example, expectedCaptures] of queryChecks) {
    const result = run(
      "tree-sitter",
      [
        "query",
        "--grammar-path",
        grammar,
        path.join(editorsDirectory, query),
        example,
      ],
      { env: environment },
    );
    const observedCaptures = new Set(
      [...result.stdout.matchAll(/^\s*capture:\s+\d+\s+-\s+([\w.-]+),/gm)].map(
        (match) => match[1],
      ),
    );
    for (const capture of expectedCaptures) {
      assert(
        observedCaptures.has(capture),
        `${query} did not emit expected @${capture} capture for ${example}`,
      );
    }
  }
}

const temporaryDirectory = fs.mkdtempSync(
  path.join(os.tmpdir(), "nexa-editor-check-"),
);
try {
  validateJsonFiles();
  validatePackageScripts();
  validateContributions();
  validateZedFiles();
  validateSyntaxContract();
  await validateTextMateTokenization();
  checkGeneratedFiles(temporaryDirectory);
  validateExamplesAndQueries(temporaryDirectory);
  console.log(
    `Nexa editor support check passed (${nexaExamples.length} Nexa examples, ${nidlExamples.length} NIDL examples, ${invalidNidlExamples.length} rejected NIDL comment fixtures, ${queryChecks.length} queries).`,
  );
} finally {
  fs.rmSync(temporaryDirectory, { recursive: true, force: true });
}
