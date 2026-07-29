import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const scriptsDirectory = path.dirname(fileURLToPath(import.meta.url));

export const editorsDirectory = path.resolve(scriptsDirectory, "..");
export const repositoryDirectory = path.resolve(editorsDirectory, "..");
export const grammarDirectory = path.join(editorsDirectory, "tree-sitter-nexa");
export const vscodeDirectory = path.join(editorsDirectory, "vscode");
export const zedDirectory = path.join(editorsDirectory, "zed");
export const artifactDirectory = path.join(
  repositoryDirectory,
  "target",
  "nexa-editor-support",
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
      strings: {
        name: "string.quoted.double.nexa",
        begin: '"',
        end: '"',
        patterns: [
          {
            name: "constant.character.escape.nexa",
            match: "\\\\[nrt\\\\\"]",
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
              "\\b(module|import)\\s+([A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*)",
            captures: {
              1: capture("keyword.control.module.nexa"),
              2: capture("entity.name.namespace.nexa"),
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
              "\\b(fn)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.type.function.nexa"),
              2: capture("entity.name.function.nexa"),
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
          "\\b[A-Za-z_][A-Za-z0-9_]*(?:\\.[A-Za-z_][A-Za-z0-9_]*)*(?=\\s*(?:<[^;{}()]+>)?\\s*\\()",
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
        name: "keyword.operator.nexa",
        match: "==|=>|->|\\.\\.|[+*/=<>?:@-]",
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
    name: "Nexa IDL",
    scopeName: "source.nexa-idl",
    fileTypes: ["nidl"],
    patterns: [
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
      declarations: {
        patterns: [
          {
            match:
              "\\b(interface|opaque|struct|enum)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.type.nexa-idl"),
              2: capture("entity.name.type.nexa-idl"),
            },
          },
          {
            match:
              "\\b(fn|export)\\s+([A-Za-z_][A-Za-z0-9_]*)",
            captures: {
              1: capture("storage.type.function.nexa-idl"),
              2: capture("entity.name.function.nexa-idl"),
            },
          },
        ],
      },
      policies: {
        name: "constant.language.policy.nexa-idl",
        match: wordPattern(nidl.policyKeywords),
      },
      "builtin-types": {
        name: "support.type.builtin.nexa-idl",
        match: wordPattern(nidl.builtinTypes),
      },
      keywords: {
        name: "keyword.control.nexa-idl",
        match: wordPattern([
          ...nidl.declarationKeywords,
          ...nidl.modeKeywords,
        ]),
      },
      "function-names": {
        name: "entity.name.function.call.nexa-idl",
        match: "\\b[A-Za-z_][A-Za-z0-9_]*(?=\\s*\\()",
      },
      "field-names": {
        name: "variable.other.property.nexa-idl",
        match: "\\b[A-Za-z_][A-Za-z0-9_]*(?=\\s*:)",
      },
      "type-names": {
        name: "entity.name.type.nexa-idl",
        match: "\\b[A-Z][A-Za-z0-9_]*\\b",
      },
      numbers: {
        name: "constant.numeric.integer.nexa-idl",
        match: "\\b[0-9]+\\b",
      },
      operators: {
        name: "keyword.operator.nexa-idl",
        match: "->|[:<>]",
      },
      punctuation: {
        name: "punctuation.separator.nexa-idl",
        match: "[{},();]",
      },
    },
  };

  return new Map([
    ["nexa.tmLanguage.json", nexaGrammar],
    ["nexa-idl.tmLanguage.json", nidlGrammar],
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
  grammarRevision = "local",
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
    .replaceAll("{{GRAMMAR_REVISION}}", grammarRevision);
}
