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

const versionedGeneratedGrammarFiles = [
  "src/grammar.json",
  "src/node-types.json",
];
const generatedParser = "src/parser.c";

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
  "examples/hello-runtime/hello.nexa",
  "examples/language-scale/packages/app/src/language_scale/app.nexa",
  "examples/language-scale/packages/app/src/language_scale/flow.nexa",
  "examples/language-scale/packages/app/src/language_scale/rules.nexa",
  "examples/language-scale/packages/app/src/language_scale/text.nexa",
  "examples/language-scale/packages/app/tests/basic/scoring.nexa",
  "examples/language-scale/packages/snake-common/src/math.nexa",
  "examples/snake-game/packages/builtin/classic-hud/src/snake/classic_hud.nexa",
  "examples/snake-game/packages/builtin/classic-rules/src/snake/classic_rules.nexa",
  "examples/snake-game/packages/builtin/classic-spawn/src/snake/classic_spawn.nexa",
  "examples/snake-game/packages/builtin/default-skin/src/snake/default_skin.nexa",
  "examples/snake-game/packages/dlc/food-chaos/src/snake/food_chaos.nexa",
  "examples/snake-game/packages/mods/corner-spawn/src/snake/corner_spawn.nexa",
  "examples/snake-game/packages/mods/neon-skin/src/snake/neon_skin.nexa",
  "examples/snake-game/packages/mods/score-overlay/src/snake/score_overlay.nexa",
  "examples/snake-game/packages/mods/weird-foods/src/snake/weird_foods.nexa",
];
const contractExamples = [
  "editors/fixtures/m4-language.contract.nexa",
  "editors/fixtures/nexa-contract-comment-invalid.contract.nexa",
  "editors/fixtures/nexa-contract-enum-comment-invalid.contract.nexa",
  "editors/fixtures/nexa-contract-header-attribute.contract.nexa",
  "examples/combat-runtime/combat_api.contract.nexa",
  "examples/game.contract.nexa",
  "examples/hello-runtime/hello_api.contract.nexa",
  "examples/language-scale/language_scale.contract.nexa",
  "examples/snake-game/snake_api.contract.nexa",
  "crates/nexa-contract/tests/fixtures/business_host/contract.nidl",
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
    "tree-sitter-nexa-contract/queries/highlights.scm",
    "editors/fixtures/m4-language.contract.nexa",
    ["keyword", "function", "type", "type.builtin", "attribute"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-contract/highlights.scm",
    "editors/fixtures/m4-language.contract.nexa",
    ["keyword", "function", "type", "type.builtin", "attribute"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-contract/indents.scm",
    "editors/fixtures/m4-language.contract.nexa",
    ["indent", "end"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-contract/brackets.scm",
    "editors/fixtures/m4-language.contract.nexa",
    ["open", "close"],
  ],
  [
    idlGrammarDirectory,
    "zed/languages/nexa-contract/outline.scm",
    "editors/fixtures/m4-language.contract.nexa",
    ["item", "name", "context"],
  ],
  [
    idlGrammarDirectory,
    "tree-sitter-nexa-contract/queries/highlights.scm",
    "editors/fixtures/nexa-contract-header-attribute.contract.nexa",
    ["keyword", "type", "attribute", "comment.documentation"],
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
    "tree-sitter-nexa-contract/package.json",
    "tree-sitter-nexa-contract/tree-sitter.json",
    "tree-sitter-nexa-contract/src/grammar.json",
    "tree-sitter-nexa-contract/src/node-types.json",
    "vscode/package.json",
    "vscode/language-configuration/nexa.json",
    "vscode/language-configuration/nexa-contract.json",
    "vscode/syntaxes/nexa.tmLanguage.json",
    "vscode/syntaxes/nexa-contract.tmLanguage.json",
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
    "*.contract.nexa",
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
    "registerDocumentSymbolProvider",
    "textDocument/documentSymbol",
    "settlePendingSymbolRequests",
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
    languages.get("nexa-contract")?.extensions?.includes(".contract.nexa"),
    "VS Code Nexa Contract language contribution is invalid",
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
    grammars.get("nexa-contract") === "source.nexa-contract",
    "VS Code Nexa Contract scope is invalid",
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
  const contractConfig = parseJson(
    path.join(vscodeDirectory, "language-configuration", "nexa-contract.json"),
  );
  assert(
    contractConfig.comments?.lineComment === "//" &&
      contractConfig.comments?.blockComment?.[0] === "/*" &&
      contractConfig.comments?.blockComment?.[1] === "*/",
    "Contract must register its line, block, and documentation comments",
  );

  for (const [language, config] of [
    ["nexa", nexaConfig],
    ["nexa-contract", contractConfig],
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
    path.join(zedDirectory, "languages", "nexa-contract", "config.toml"),
  );
  const idlConfig = TOML.parse(idlSource);
  assert(
    idlConfig.grammar === "nexa_contract",
    "Nexa Contract must use grammar nexa_contract",
  );
  assert(
    idlConfig.line_comments?.includes("// "),
    "Nexa Contract must register line and documentation comments",
  );

  const manifest = TOML.parse(renderZedManifest());
  assert(
    manifest.grammars.nexa.repository === pathToFileURL(grammarDirectory).href,
    "Zed Nexa grammar URL is not the canonical absolute file URL",
  );
  assert(
    manifest.grammars.nexa_contract.repository ===
      pathToFileURL(idlGrammarDirectory).href,
    "Zed Contract grammar URL is not the canonical absolute file URL",
  );
  assert(
    manifest.grammars.nexa.rev === "local" &&
      manifest.grammars.nexa_contract.rev === "local",
    "Zed local grammar revisions are invalid",
  );
  assert(
    manifest.language_servers?.nexa?.languages?.includes("Nexa") &&
      manifest.language_servers?.nexa?.languages?.includes("Nexa Contract"),
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
    "Nexa Tree-sitter grammar must define all v2 comments",
  );
  for (const rule of [
    "use_declaration",
    "namespace_path",
    "field_declaration",
    "postfix_expression",
    "await_suffix",
    "attribute",
  ]) {
    assert(
      nexaGrammar.rules[rule],
      `Nexa v2 Tree-sitter grammar is missing ${rule}`,
    );
  }
  for (const rule of [
    "module_declaration",
    "import_declaration",
    "await_expression",
    "with_expression",
  ]) {
    assert(
      !nexaGrammar.rules[rule],
      `legacy Nexa Tree-sitter rule ${rule} remains active`,
    );
  }
  assert(
    idlGrammar.rules.line_comment &&
      idlGrammar.rules.block_comment &&
      idlGrammar.rules.doc_comment,
    "Contract Tree-sitter grammar must define v2 comments",
  );
  for (const rule of [
    "nidl_document",
    "handle_declaration",
    "host_block",
    "nexa_block",
    "host_function_declaration",
    "nexa_function_declaration",
    "nidl_attribute",
  ]) {
    assert(
      idlGrammar.rules[rule],
      `Contract Tree-sitter grammar is missing ${rule}`,
    );
  }
  for (const rule of [
    "opaque_declaration",
    "export_declaration",
    "request_policy",
    "void_type",
  ]) {
    assert(
      !idlGrammar.rules[rule],
      `legacy Contract Tree-sitter rule ${rule} remains active`,
    );
  }
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
    ["mut", "use", "async"].every((keyword) =>
      [
        ...syntax.nexa.declarationKeywords,
        ...syntax.nexa.effectKeywords,
        ...syntax.nexa.statementKeywords,
      ].includes(keyword),
    ),
    "Nexa v2 surface keywords are missing",
  );
  assert(
    ["var", "module", "import", "task", "immediate", "migration", "activation", "cleanup", "stateful", "with"]
      .filter((keyword) => !syntax.nexa.attributeKeywords.includes(keyword))
      .every(
        (keyword) =>
          ![
            ...syntax.nexa.declarationKeywords,
            ...syntax.nexa.effectKeywords,
            ...syntax.nexa.statementKeywords,
          ].includes(keyword),
      ),
    "legacy Nexa surface keywords remain active",
  );
  assert(
    ["contract", "host", "nexa", "handle", "async", "fn"].every((keyword) =>
      [
        ...syntax.contract.declarationKeywords,
        ...syntax.contract.modeKeywords,
      ].includes(keyword),
    ) &&
      ["void", "request", "host_request", "array", "buffer"].every(
        (type) => !syntax.contract.builtinTypes.includes(type),
      ),
    "Contract v3 keywords or generic type spelling are invalid",
  );

  const nexaTextMate = parseJson(
    path.join(vscodeDirectory, "syntaxes", "nexa.tmLanguage.json"),
  );
  const idlTextMate = parseJson(
    path.join(vscodeDirectory, "syntaxes", "nexa-contract.tmLanguage.json"),
  );
  assert(
    Object.hasOwn(nexaTextMate.repository, "comments"),
    "Nexa TextMate grammar must define comments",
  );
  assert(
    Object.hasOwn(idlTextMate.repository, "comments"),
    "Contract TextMate grammar must define comments",
  );
  assert(
    read(
      path.join(idlGrammarDirectory, "queries", "highlights.scm"),
    ).includes("(doc_comment) @comment.documentation") &&
      read(
        path.join(
          zedDirectory,
          "languages",
          "nexa-contract",
          "highlights.scm",
        ),
      ).includes("(doc_comment) @comment.documentation"),
    "Contract documentation comments must be highlighted in Tree-sitter and Zed",
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
      "source.nexa-contract",
      path.join(vscodeDirectory, "syntaxes", "nexa-contract.tmLanguage.json"),
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
      scopeName: "source.nexa-contract",
      line: "async fn load_profile(id: string) -> Result<Profile, LoadError>;",
      operators: [["->", "keyword.operator.arrow.nexa-contract"]],
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

function generateAndCompare(source, temporaryDirectory, versionedParser) {
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
  for (const file of versionedGeneratedGrammarFiles) {
    assertFileEquals(
      path.join(source, file),
      path.join(temporaryGrammar, file),
      `${name}/${file}`,
    );
  }
  const generatedParserPath = path.join(temporaryGrammar, generatedParser);
  assert(
    fs.statSync(generatedParserPath).size > 0,
    `${name}/${generatedParser} was not generated`,
  );
  const sourceParserPath = path.join(source, generatedParser);
  if (versionedParser || fs.existsSync(sourceParserPath)) {
    assertFileEquals(
      sourceParserPath,
      generatedParserPath,
      `${name}/${generatedParser}`,
    );
  }
  return temporaryGrammar;
}

function checkGeneratedFiles(temporaryDirectory) {
  fs.copyFileSync(
    path.join(editorsDirectory, "language-syntax.json"),
    path.join(temporaryDirectory, "language-syntax.json"),
  );
  const generatedGrammars = {
    nexa: generateAndCompare(grammarDirectory, temporaryDirectory, false),
    contract: generateAndCompare(idlGrammarDirectory, temporaryDirectory, true),
  };

  for (const [file, grammar] of textMateGrammars()) {
    const actual = read(path.join(vscodeDirectory, "syntaxes", file));
    assert(
      actual === stableJson(grammar),
      `vscode/syntaxes/${file} is out of date`,
    );
  }
  return generatedGrammars;
}

function parseExamples(grammar, files, environment) {
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
  const allSuccessful = summary.parse_summaries.every(
    (item) => item.successful,
  );
  assert(
    allSuccessful,
    "A required example contains ERROR or MISSING nodes",
  );
}

function validateExamplesAndQueries(temporaryDirectory, generatedGrammars) {
  const environment = {
    ...process.env,
    XDG_CACHE_HOME: path.join(temporaryDirectory, "cache"),
  };
  parseExamples(generatedGrammars.nexa, nexaExamples, environment);
  parseExamples(generatedGrammars.contract, contractExamples, environment);

  for (const [grammar, query, example, expectedCaptures] of queryChecks) {
    const generatedGrammar =
      grammar === grammarDirectory
        ? generatedGrammars.nexa
        : generatedGrammars.contract;
    const result = run(
      "tree-sitter",
      [
        "query",
        "--grammar-path",
        generatedGrammar,
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
  const generatedGrammars = checkGeneratedFiles(temporaryDirectory);
  validateExamplesAndQueries(temporaryDirectory, generatedGrammars);
  console.log(
    `Nexa editor support check passed (${nexaExamples.length} Nexa examples, ${contractExamples.length} Contract examples including comments, ${queryChecks.length} queries).`,
  );
} finally {
  fs.rmSync(temporaryDirectory, { recursive: true, force: true });
}
