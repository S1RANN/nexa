import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import {
  artifactDirectory,
  copyDirectory,
  grammarDirectory,
  idlGrammarDirectory,
  renderZedManifest,
  zedDirectory,
} from "./lib.mjs";

function runGit(args, cwd, environment = process.env) {
  const result = spawnSync("git", args, {
    cwd,
    env: environment,
    encoding: "utf8",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed\n${result.stdout}${result.stderr}`,
    );
  }
  return result.stdout.trim();
}

const output = path.join(artifactDirectory, "zed");
fs.rmSync(output, { recursive: true, force: true });
fs.mkdirSync(output, { recursive: true });
copyDirectory(path.join(zedDirectory, "languages"), path.join(output, "languages"));
fs.copyFileSync(
  path.join(zedDirectory, "Cargo.toml"),
  path.join(output, "Cargo.toml"),
);
fs.copyFileSync(
  path.join(zedDirectory, "Cargo.lock"),
  path.join(output, "Cargo.lock"),
);
copyDirectory(path.join(zedDirectory, "src"), path.join(output, "src"));

const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_DATE: "2000-01-01T00:00:00Z",
  GIT_AUTHOR_EMAIL: "nexa-editor-support@localhost",
  GIT_AUTHOR_NAME: "Nexa Editor Support",
  GIT_COMMITTER_DATE: "2000-01-01T00:00:00Z",
  GIT_COMMITTER_EMAIL: "nexa-editor-support@localhost",
  GIT_COMMITTER_NAME: "Nexa Editor Support",
};

function packageGrammar(source, name, commitMessage) {
  const destination = path.join(output, name);
  fs.mkdirSync(destination, { recursive: true });
  for (const entry of [
    "grammar.js",
    "package.json",
    "queries",
    "src",
    "tree-sitter.json",
  ]) {
    fs.cpSync(path.join(source, entry), path.join(destination, entry), {
      recursive: true,
    });
  }
  fs.copyFileSync(
    path.join(source, "..", "language-syntax.json"),
    path.join(destination, "language-syntax.json"),
  );
  const grammarSource = fs
    .readFileSync(path.join(destination, "grammar.js"), "utf8")
    .replace(
      'require("../language-syntax.json")',
      'require("./language-syntax.json")',
    );
  fs.writeFileSync(path.join(destination, "grammar.js"), grammarSource);

  const generatedGrammar = spawnSync("tree-sitter", ["generate"], {
    cwd: destination,
    encoding: "utf8",
  });
  if (generatedGrammar.error) {
    throw generatedGrammar.error;
  }
  if (generatedGrammar.status !== 0) {
    throw new Error(
      `packaged ${name} grammar generation failed\n${generatedGrammar.stdout}${generatedGrammar.stderr}`,
    );
  }

  runGit(
    ["init", "--quiet", "--initial-branch=main", "--object-format=sha1"],
    destination,
  );
  runGit(["add", "--all"], destination);
  runGit(
    [
      "-c",
      "commit.gpgsign=false",
      "commit",
      "--quiet",
      "--message",
      commitMessage,
    ],
    destination,
    gitEnvironment,
  );
  const revision = runGit(["rev-parse", "HEAD"], destination);
  const grammarCheck = spawnSync("tree-sitter", ["generate"], {
    cwd: destination,
    encoding: "utf8",
  });
  if (grammarCheck.error) {
    throw grammarCheck.error;
  }
  if (grammarCheck.status !== 0) {
    throw new Error(
      `packaged ${name} grammar generation failed\n${grammarCheck.stdout}${grammarCheck.stderr}`,
    );
  }
  runGit(
    [
      "diff",
      "--exit-code",
      "--",
      "src/parser.c",
      "src/grammar.json",
      "src/node-types.json",
    ],
    destination,
  );
  return { destination, revision };
}

const packagedGrammar = packageGrammar(
  grammarDirectory,
  "tree-sitter-nexa",
  "Package Nexa Tree-sitter grammar",
);
const packagedIdlGrammar = packageGrammar(
  idlGrammarDirectory,
  "tree-sitter-nexa-idl",
  "Package Nexa IDL Tree-sitter grammar",
);

fs.writeFileSync(
  path.join(output, "extension.toml"),
  renderZedManifest({
    grammarRepository: packagedGrammar.destination,
    grammarRevision: packagedGrammar.revision,
    idlGrammarRepository: packagedIdlGrammar.destination,
    idlGrammarRevision: packagedIdlGrammar.revision,
  }),
);

console.log(output);
