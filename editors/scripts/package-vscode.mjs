import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import {
  artifactDirectory,
  vscodeDirectory,
} from "./lib.mjs";

const outputDirectory = path.join(artifactDirectory, "vscode");
const output = path.join(
  outputDirectory,
  "nexa-language-support-0.1.0.vsix",
);
fs.mkdirSync(outputDirectory, { recursive: true });

const result = spawnSync(
  "vsce",
  ["package", "--out", output, "--no-dependencies"],
  {
    cwd: vscodeDirectory,
    encoding: "utf8",
    stdio: "inherit",
  },
);

if (result.error) {
  throw result.error;
}
if (result.status !== 0) {
  process.exit(result.status ?? 1);
}

console.log(output);
