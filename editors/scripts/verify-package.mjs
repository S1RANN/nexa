import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import {
  artifactDirectory,
  packageReportPath,
  repositoryDirectory,
  vscodeDirectory,
} from "./lib.mjs";

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

function relativeArtifactPath(file) {
  return path.relative(repositoryDirectory, file).split(path.sep).join("/");
}

function sha256(bytes) {
  return crypto.createHash("sha256").update(bytes).digest("hex");
}

function git(args, cwd) {
  const result = spawnSync("git", args, {
    cwd,
    encoding: "utf8",
  });
  if (result.error) {
    throw result.error;
  }
  if (result.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed in ${cwd}\n${result.stdout}${result.stderr}`,
    );
  }
  return result.stdout.trim();
}

function inspectArtifact(file, magic, label) {
  const bytes = fs.readFileSync(file);
  assert(bytes.length >= 1024, `${label} is unexpectedly small`);
  assert(
    bytes.subarray(0, magic.length).equals(magic),
    `${label} has an invalid binary signature`,
  );
  return {
    path: relativeArtifactPath(file),
    bytes: bytes.length,
    sha256: sha256(bytes),
  };
}

fs.rmSync(packageReportPath, { force: true });

const vscodePackage = JSON.parse(
  fs.readFileSync(path.join(vscodeDirectory, "package.json"), "utf8"),
);
const expectedVsixName = `nexa-language-support-${vscodePackage.version}.vsix`;
const vscodeOutputDirectory = path.join(artifactDirectory, "vscode");
const vsixEntries = fs
  .readdirSync(vscodeOutputDirectory)
  .filter((entry) => entry.endsWith(".vsix"));
assert(
  vsixEntries.length === 1 && vsixEntries[0] === expectedVsixName,
  `expected exactly ${expectedVsixName} in ${vscodeOutputDirectory}`,
);
const vscodeArtifact = inspectArtifact(
  path.join(vscodeOutputDirectory, expectedVsixName),
  Buffer.from([0x50, 0x4b, 0x03, 0x04]),
  "VSIX artifact",
);

const zedDirectory = path.join(artifactDirectory, "zed");
const zedArtifact = inspectArtifact(
  path.join(zedDirectory, "extension.wasm"),
  Buffer.from([0x00, 0x61, 0x73, 0x6d]),
  "Zed WebAssembly artifact",
);
const grammarDirectories = {
  nexa: path.join(zedDirectory, "tree-sitter-nexa"),
  nexa_idl: path.join(zedDirectory, "tree-sitter-nexa-idl"),
};
const grammarRevisions = Object.fromEntries(
  Object.entries(grammarDirectories).map(([name, directory]) => {
    assert(
      git(["status", "--porcelain"], directory) === "",
      `${name} packaged grammar repository is dirty`,
    );
    return [name, git(["rev-parse", "HEAD"], directory)];
  }),
);

const extensionManifest = path.join(zedDirectory, "extension.toml");
const manifestSource = fs.readFileSync(extensionManifest, "utf8");
for (const revision of Object.values(grammarRevisions)) {
  assert(
    manifestSource.includes(`rev = "${revision}"`),
    `Zed manifest does not pin packaged grammar revision ${revision}`,
  );
}

const report = {
  schema: 1,
  status: "PASS",
  vscode: vscodeArtifact,
  zed: {
    ...zedArtifact,
    target: "wasm32-wasip2",
    manifest: relativeArtifactPath(extensionManifest),
    grammar_revisions: grammarRevisions,
  },
};
fs.mkdirSync(path.dirname(packageReportPath), { recursive: true });
const temporaryReport = `${packageReportPath}.tmp`;
fs.writeFileSync(temporaryReport, `${JSON.stringify(report, null, 2)}\n`);
fs.renameSync(temporaryReport, packageReportPath);

console.log(packageReportPath);
