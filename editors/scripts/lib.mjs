import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const scriptsDirectory = path.dirname(fileURLToPath(import.meta.url));

export const editorsDirectory = path.resolve(scriptsDirectory, "..");
export const repositoryDirectory = path.resolve(editorsDirectory, "..");
export const grammarDirectory = path.join(editorsDirectory, "tree-sitter-nexa");
export const idlGrammarDirectory = path.join(
  editorsDirectory,
  "tree-sitter-nexa-idl",
);
export const vscodeDirectory = path.join(editorsDirectory, "vscode");
export const zedDirectory = path.join(editorsDirectory, "zed");
export const artifactDirectory = path.join(
  repositoryDirectory,
  "target",
  "nexa-editor-support",
);
export const packageReportPath = path.join(
  artifactDirectory,
  "editor-package-report.json",
);

export function readSyntax() {
  return JSON.parse(
    fs.readFileSync(path.join(editorsDirectory, "language-syntax.json"), "utf8"),
  );
}

export function stableJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function wordPattern(words) {
  return `\\b(?:${words.map(escapeRegex).join("|")})\\b`;
}

function capture(name) {
  return { name };
}

export function textMateGrammars(syntax = readSyntax()) {
  const nexa = syntax.nexa;
  const nidl = syntax.nidl;
  const nexaKeywordGroups = [
    nexa.declarationKeywords,
    nexa.visibilityKeywords,
    nexa.effectKeywords,
    nexa.statementKeywords,
  ].flat();

  const nexaGrammar = {
    $schema:
      "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
    name: "Nexa",
    scopeName: "source.nexa",
    fileTypes: ["nexa"],
    patterns: [
      { include: "#comments" },
      { include: "#strings" },
      { include: "#runes" },
      { include: "#numbers" },
      { include: "#attributes" },
      { include: "#declarations" },
      { include: "#intrinsics" },
      { include: "#constructors" },
      { include: "#builtin-types" },
      { include: "#keywords" },
      { include: "#function-calls" },
      { include: "#properties" },
      { include: "#type-names" },
      { include: "#operators" },
      { include: "#punctuation" },
    ],
    repository: {
      comments: {
        patterns: [
          {
            name: "comment.line.documentation.nexa",
            match: "///.*$",
          },
          {
            name: "comment.line.double-slash.nexa",
            match: "//.*$",
          },
          {
            name: "comment.block.nexa",
            begin: "/\\*",
            end: "\\*/",
          },
        ],
      },
      strings: {
        name: "string.quoted.double.nexa",
        begin: '"',
        end: '"',
        patterns: [
          {
            name: "constant.character.escape.nexa",
            match: "\\\\(?:[nrt\\\\\"]|\\$\\{)",
          },
          {
            name: "meta.interpolation.nexa",
            begin: "\\$\\{",
            beginCaptures: {
              0: capture("punctuation.section.interpolation.begin.nexa"),
            },
            end: "\\}",
            endCaptures: {
              0: capture("punctuation.section.interpolation.end.nexa"),
            },
            patterns: [{ include: "$self" }],
          },
        ],
      },
      runes: {
        name: "string.quoted.single.nexa",
        begin: "'",
        end: "'",
        patterns: [
          {
            name: "constant.character.escape.nexa",
            match: "\\\\[nrt\\\\']",
          },
        ],
      },
      numbers: {
        patterns: [
          {
            name: "constant.numeric.float.nexa",
            match: "\\b[0-9]+\\.[0-9]+\\b",
          },
          {
            name: "constant.numeric.integer.nexa",
            match: "\\b[0-9]+\\b",
          },
        ],
      },
      attributes: {
        match: `(@)(${nexa.attributeKeywords.map(escapeRegex).join("|")})\\b`,
        captures: {
          1: capture("punctuation.definition.annotation.nexa"),
          2: capture("entity.name.tag.nexa"),
        },
      },
      declarations: {
        patterns: [
          {
            match:
              "\\b(use)\\s+([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+)(?:\\s+(as)\\s+([A-Za-z_][A-Za-z0-9_]*))?",
            captures: {
              1: capture("keyword.control.namespace.nexa"),
              2: capture("entity.name.namespace.nexa"),
              3: capture("keyword.control.namespace.nexa"),
              4: capture("entity.name.namespace.nexa"),
            },
          },
          {
            match:
              "\\b(struct|enum|class)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.type.nexa"),
              2: capture("entity.name.type.nexa"),
            },
          },
          {
            match:
              "\\b(?:(async)\\s+)?(fn)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.modifier.async.nexa"),
              2: capture("storage.type.function.nexa"),
              3: capture("entity.name.function.nexa"),
            },
          },
          {
            match:
              "\\b(const)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.modifier.nexa"),
              2: capture("constant.other.nexa"),
            },
          },
        ],
      },
      intrinsics: {
        name: "support.function.builtin.nexa",
        match: `\\b(?:${nexa.migrationIntrinsics
          .map(escapeRegex)
          .join("|")})\\b`,
      },
      constructors: {
        name: "entity.name.type.variant.nexa",
        match: wordPattern(nexa.constructors),
      },
      "builtin-types": {
        name: "support.type.builtin.nexa",
        match: wordPattern(nexa.builtinTypes),
      },
      keywords: {
        patterns: [
          {
            name: "keyword.control.nexa",
            match: wordPattern(nexaKeywordGroups),
          },
          {
            name: "constant.language.boolean.nexa",
            match: wordPattern(nexa.literalKeywords),
          },
        ],
      },
      "function-calls": {
        name: "entity.name.function.call.nexa",
        match:
          "\\b[A-Za-z_][A-Za-z0-9_]*(?:(?:::|\\.)[A-Za-z_][A-Za-z0-9_]*)*(?=\\s*(?:<[^;{}()]+>)?\\s*\\()",
      },
      properties: {
        name: "variable.other.property.nexa",
        match: "(?<=\\.)[A-Za-z_][A-Za-z0-9_]*",
      },
      "type-names": {
        name: "entity.name.type.nexa",
        match: "\\b[A-Z][A-Za-z0-9_]*\\b",
      },
      operators: {
        patterns: [
          {
            name: "keyword.operator.arrow.nexa",
            match: "->|=>",
          },
          {
            name: "keyword.operator.nexa",
            match: "::|&&|\\|\\||==|!=|<=|>=|\\.\\.|[+*/=!<>?:@-]",
          },
        ],
      },
      punctuation: {
        name: "punctuation.separator.nexa",
        match: "[{},();,.]",
      },
    },
  };

  const nidlGrammar = {
    $schema:
      "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json",
    name: "Nexa Contract",
    scopeName: "source.nexa-contract",
    fileTypes: ["contract.nexa"],
    patterns: [
      { include: "#comments" },
      { include: "#strings" },
      { include: "#attributes" },
      { include: "#declarations" },
      { include: "#policies" },
      { include: "#builtin-types" },
      { include: "#keywords" },
      { include: "#function-names" },
      { include: "#field-names" },
      { include: "#type-names" },
      { include: "#numbers" },
      { include: "#operators" },
      { include: "#punctuation" },
    ],
    repository: {
      comments: {
        patterns: [
          {
            name: "comment.line.documentation.nexa-contract",
            match: "///.*$",
          },
          {
            name: "comment.line.double-slash.nexa-contract",
            match: "//.*$",
          },
          {
            name: "comment.block.nexa-contract",
            begin: "/\\*",
            end: "\\*/",
          },
        ],
      },
      strings: {
        name: "string.quoted.double.nexa-contract",
        begin: "\"",
        end: "\"",
        patterns: [
          {
            name: "constant.character.escape.nexa-contract",
            match: "\\\\[nrt\\\\\"]",
          },
        ],
      },
      attributes: {
        match: `(@)(${nidl.attributeKeywords
          .map(escapeRegex)
          .join("|")})\\b`,
        captures: {
          1: capture("punctuation.definition.annotation.nexa-contract"),
          2: capture("entity.name.tag.nexa-contract"),
        },
      },
      declarations: {
        patterns: [
          {
            match:
              "\\b(contract|handle|struct|enum)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.type.nexa-contract"),
              2: capture("entity.name.type.nexa-contract"),
            },
          },
          {
            match:
              "\\b(?:(async)\\s+)?(fn)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.modifier.async.nexa-contract"),
              2: capture("storage.type.function.nexa-contract"),
              3: capture("entity.name.function.nexa-contract"),
            },
          },
        ],
      },
      policies: {
        name: "constant.language.policy.nexa-contract",
        match: wordPattern(nidl.policyKeywords),
      },
      "builtin-types": {
        name: "support.type.builtin.nexa-contract",
        match: wordPattern(nidl.builtinTypes),
      },
      keywords: {
        name: "keyword.control.nexa-contract",
        match: wordPattern([
          ...nidl.declarationKeywords,
          ...nidl.modeKeywords,
        ]),
      },
      "function-names": {
        name: "entity.name.function.call.nexa-contract",
        match: "\\b[A-Za-z_][A-Za-z0-9_]*(?=\\s*\\()",
      },
      "field-names": {
        name: "variable.other.property.nexa-contract",
        match: "\\b[A-Za-z_][A-Za-z0-9_]*(?=\\s*:)",
      },
      "type-names": {
        name: "entity.name.type.nexa-contract",
        match: "\\b[A-Z][A-Za-z0-9_]*\\b",
      },
      numbers: {
        name: "constant.numeric.integer.nexa-contract",
        match: "\\b[0-9]+\\b",
      },
      operators: {
        patterns: [
          {
            name: "keyword.operator.arrow.nexa-contract",
            match: "->",
          },
          {
            name: "keyword.operator.nexa-contract",
            match: "[=:<>]",
          },
        ],
      },
      punctuation: {
        name: "punctuation.separator.nexa-contract",
        match: "[{},();@]",
      },
    },
  };

  return new Map([
    ["nexa.tmLanguage.json", nexaGrammar],
    ["nexa-contract.tmLanguage.json", nidlGrammar],
  ]);
}

export function writeTextMateGrammars(directory) {
  fs.mkdirSync(directory, { recursive: true });
  for (const [name, grammar] of textMateGrammars()) {
    fs.writeFileSync(path.join(directory, name), stableJson(grammar));
  }
}

export function copyDirectory(source, destination) {
  fs.cpSync(source, destination, {
    recursive: true,
    filter: (entry) => path.basename(entry) !== ".DS_Store",
  });
}

export function renderZedManifest({
  grammarRepository = grammarDirectory,
  idlGrammarRepository = idlGrammarDirectory,
  grammarRevision = "local",
  idlGrammarRevision = "local",
} = {}) {
  const template = fs.readFileSync(
    path.join(zedDirectory, "extension.toml.in"),
    "utf8",
  );
  return template
    .replaceAll(
      "{{GRAMMAR_REPOSITORY}}",
      pathToFileURL(grammarRepository).href,
    )
    .replaceAll("{{GRAMMAR_REVISION}}", grammarRevision)
    .replaceAll(
      "{{IDL_GRAMMAR_REPOSITORY}}",
      pathToFileURL(idlGrammarRepository).href,
    )
    .replaceAll("{{IDL_GRAMMAR_REVISION}}", idlGrammarRevision);
}
