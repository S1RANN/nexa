import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import {
  artifactDirectory,
  vscodeDirectory,
} from "./lib.mjs";

const outputDirectory = path.join(artifactDirectory, "vscode");
const extensionPackage = JSON.parse(
  fs.readFileSync(path.join(vscodeDirectory, "package.json"), "utf8"),
);
const output = path.join(
  outputDirectory,
  `nexa-language-support-${extensionPackage.version}.vsix`,
);
fs.rmSync(outputDirectory, { recursive: true, force: true });
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

const archive = fs.readFileSync(output);
if (
  archive.length < 1024 ||
  !archive.subarray(0, 4).equals(Buffer.from([0x50, 0x4b, 0x03, 0x04]))
) {
  throw new Error(`vsce did not produce a valid non-empty VSIX archive at ${output}`);
}

console.log(output);
