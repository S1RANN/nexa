import fs from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";

import {
  grammarDirectory,
  idlGrammarDirectory,
  packageReportPath,
  vscodeDirectory,
  writeTextMateGrammars,
} from "./lib.mjs";

fs.rmSync(packageReportPath, { force: true });

for (const directory of [grammarDirectory, idlGrammarDirectory]) {
  const result = spawnSync("tree-sitter", ["generate"], {
    cwd: directory,
    encoding: "utf8",
    stdio: "inherit",
  });

  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

writeTextMateGrammars(path.join(vscodeDirectory, "syntaxes"));
