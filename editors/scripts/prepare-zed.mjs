import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import {
  artifactDirectory,
  copyDirectory,
  grammarDirectory,
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

const packagedGrammar = path.join(output, "tree-sitter-nexa");
fs.mkdirSync(packagedGrammar, { recursive: true });
for (const entry of [
  "grammar.js",
  "package.json",
  "queries",
  "src",
  "tree-sitter.json",
]) {
  fs.cpSync(
    path.join(grammarDirectory, entry),
    path.join(packagedGrammar, entry),
    { recursive: true },
  );
}
fs.copyFileSync(
  path.join(grammarDirectory, "..", "language-syntax.json"),
  path.join(packagedGrammar, "language-syntax.json"),
);
const packagedGrammarSource = fs
  .readFileSync(path.join(packagedGrammar, "grammar.js"), "utf8")
  .replace(
    'require("../language-syntax.json")',
    'require("./language-syntax.json")',
  );
fs.writeFileSync(
  path.join(packagedGrammar, "grammar.js"),
  packagedGrammarSource,
);

runGit(["init", "--quiet", "--initial-branch=main"], packagedGrammar);
runGit(["add", "--all"], packagedGrammar);
const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_DATE: "2000-01-01T00:00:00Z",
  GIT_AUTHOR_EMAIL: "nexa-editor-support@localhost",
  GIT_AUTHOR_NAME: "Nexa Editor Support",
  GIT_COMMITTER_DATE: "2000-01-01T00:00:00Z",
  GIT_COMMITTER_EMAIL: "nexa-editor-support@localhost",
  GIT_COMMITTER_NAME: "Nexa Editor Support",
};
runGit(
  [
    "-c",
    "commit.gpgsign=false",
    "commit",
    "--quiet",
    "--message",
    "Package Nexa Tree-sitter grammar",
  ],
  packagedGrammar,
  gitEnvironment,
);
const grammarRevision = runGit(["rev-parse", "HEAD"], packagedGrammar);
const grammarCheck = spawnSync("tree-sitter", ["generate"], {
  cwd: packagedGrammar,
  encoding: "utf8",
});
if (grammarCheck.error) {
  throw grammarCheck.error;
}
if (grammarCheck.status !== 0) {
  throw new Error(
    `packaged Zed grammar generation failed\n${grammarCheck.stdout}${grammarCheck.stderr}`,
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
  packagedGrammar,
);

fs.writeFileSync(
  path.join(output, "extension.toml"),
  renderZedManifest({
    grammarRepository: packagedGrammar,
    grammarRevision,
  }),
);

console.log(output);
