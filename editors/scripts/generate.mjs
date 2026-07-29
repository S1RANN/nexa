import { spawnSync } from "node:child_process";
import path from "node:path";

import {
  grammarDirectory,
  vscodeDirectory,
  writeTextMateGrammars,
} from "./lib.mjs";

const result = spawnSync("tree-sitter", ["generate"], {
  cwd: grammarDirectory,
  encoding: "utf8",
  stdio: "inherit",
});

if (result.error) {
  throw result.error;
}
if (result.status !== 0) {
  process.exit(result.status ?? 1);
}

writeTextMateGrammars(path.join(vscodeDirectory, "syntaxes"));
