import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

import TOML from "@iarna/toml";

import {
  editorsDirectory,
  grammarDirectory,
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

const exampleFiles = [
  "examples/add.nexa",
  "examples/combat-runtime/gameplay.nexa",
  "examples/combat-runtime/reload/activation_fault.nexa",
  "examples/combat-runtime/reload/invalid.nexa",
  "examples/combat-runtime/reload/v1.nexa",
  "examples/combat-runtime/reload/v2.nexa",
  "examples/combat-runtime/combat_api.nidl",
  "crates/nexa-idl/tests/fixtures/business_host/interface.nidl",
];

const queryChecks = [
  [
    "tree-sitter-nexa/queries/highlights.scm",
    "examples/combat-runtime/gameplay.nexa",
  ],
  [
    "zed/languages/nexa/highlights.scm",
    "examples/combat-runtime/gameplay.nexa",
  ],
  [
    "zed/languages/nexa/indents.scm",
    "examples/combat-runtime/gameplay.nexa",
  ],
  [
    "zed/languages/nexa/brackets.scm",
    "examples/combat-runtime/gameplay.nexa",
  ],
  [
    "zed/languages/nexa/outline.scm",
    "examples/combat-runtime/gameplay.nexa",
  ],
  [
    "zed/languages/nexa-idl/highlights.scm",
    "examples/combat-runtime/combat_api.nidl",
  ],
  [
    "zed/languages/nexa-idl/indents.scm",
    "examples/combat-runtime/combat_api.nidl",
  ],
  [
    "zed/languages/nexa-idl/brackets.scm",
    "examples/combat-runtime/combat_api.nidl",
  ],
  [
    "zed/languages/nexa-idl/outline.scm",
    "examples/combat-runtime/combat_api.nidl",
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

function validateContributions() {
  const extension = parseJson(path.join(vscodeDirectory, "package.json"));
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

  for (const language of ["nexa", "nexa-idl"]) {
    const config = parseJson(
      path.join(vscodeDirectory, "language-configuration", `${language}.json`),
    );
    assert(!("comments" in config), `${language} must not declare comments`);
  }
}

function validateZedFiles() {
  const template = read(path.join(zedDirectory, "extension.toml.in"));
  TOML.parse(template);

  for (const directory of ["nexa", "nexa-idl"]) {
    const configFile = path.join(
      zedDirectory,
      "languages",
      directory,
      "config.toml",
    );
    const source = read(configFile);
    const config = TOML.parse(source);
    assert(config.grammar === "nexa", `${directory} must use grammar nexa`);
    assert(
      !Object.hasOwn(config, "line_comments") && !source.includes("line_comments"),
      `${directory} must not declare line comments`,
    );
  }

  const manifestSource = renderZedManifest();
  const manifest = TOML.parse(manifestSource);
  const expectedUrl = pathToFileURL(grammarDirectory).href;
  assert(
    manifest.grammars.nexa.repository === expectedUrl,
    "Zed grammar URL is not the canonical absolute file URL",
  );
  assert(
    manifest.grammars.nexa.rev === "local",
    "Zed local grammar revision is invalid",
  );
}

function validateSyntaxContract() {
  const syntax = readSyntax();
  const grammar = parseJson(path.join(grammarDirectory, "src", "grammar.json"));
  assert(!grammar.rules.comment, "Tree-sitter grammar must not define comments");
  assert(
    syntax.nexa.migrationIntrinsics.length > 0,
    "Nexa migration intrinsic list must not be empty",
  );
  assert(
    syntax.nidl.builtinTypes.includes("void"),
    "NIDL builtin types must include void",
  );

  const nexaTextMate = parseJson(
    path.join(vscodeDirectory, "syntaxes", "nexa.tmLanguage.json"),
  );
  assert(
    nexaTextMate.repository.operators.match.includes("/"),
    "TextMate grammar must highlight / as an operator",
  );
  assert(
    !Object.hasOwn(nexaTextMate.repository, "comments"),
    "TextMate grammar must not define comments",
  );
}

function checkGeneratedFiles(temporaryDirectory) {
  const temporaryGrammar = path.join(temporaryDirectory, "tree-sitter-nexa");
  fs.mkdirSync(temporaryGrammar, { recursive: true });
  fs.copyFileSync(
    path.join(editorsDirectory, "language-syntax.json"),
    path.join(temporaryDirectory, "language-syntax.json"),
  );
  for (const file of ["grammar.js", "package.json", "tree-sitter.json"]) {
    fs.copyFileSync(
      path.join(grammarDirectory, file),
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
      path.join(grammarDirectory, file),
      path.join(temporaryGrammar, file),
      file,
    );
  }

  for (const [file, grammar] of textMateGrammars()) {
    const actual = read(path.join(vscodeDirectory, "syntaxes", file));
    assert(
      actual === stableJson(grammar),
      `vscode/syntaxes/${file} is out of date`,
    );
  }
}

function validateExamplesAndQueries(temporaryDirectory) {
  const environment = {
    ...process.env,
    XDG_CACHE_HOME: path.join(temporaryDirectory, "cache"),
  };
  const parse = run(
    "tree-sitter",
    [
      "parse",
      "--grammar-path",
      grammarDirectory,
      "--json-summary",
      ...exampleFiles,
    ],
    { env: environment },
  );
  const summary = JSON.parse(parse.stdout);
  assert(
    summary.source_count === exampleFiles.length,
    "Tree-sitter did not parse every required example",
  );
  assert(
    summary.parse_summaries.every((item) => item.successful),
    "A required example contains ERROR or MISSING nodes",
  );

  for (const [query, example] of queryChecks) {
    run(
      "tree-sitter",
      [
        "query",
        "--grammar-path",
        grammarDirectory,
        "--quiet",
        path.join(editorsDirectory, query),
        example,
      ],
      { env: environment },
    );
  }
}

const temporaryDirectory = fs.mkdtempSync(
  path.join(os.tmpdir(), "nexa-editor-check-"),
);
try {
  validateJsonFiles();
  validateContributions();
  validateZedFiles();
  validateSyntaxContract();
  checkGeneratedFiles(temporaryDirectory);
  validateExamplesAndQueries(temporaryDirectory);
  console.log(
    `Nexa editor support check passed (${exampleFiles.length} examples, ${queryChecks.length} queries).`,
  );
} finally {
  fs.rmSync(temporaryDirectory, { recursive: true, force: true });
}
