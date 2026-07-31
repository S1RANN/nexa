import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

import { artifactDirectory } from "./lib.mjs";

const target = "wasm32-wasip2";
const extensionDirectory = path.join(artifactDirectory, "zed");
const manifest = path.join(extensionDirectory, "Cargo.toml");
const cargoTargetDirectory = path.join(artifactDirectory, "zed-cargo-target");
const builtExtension = path.join(
  cargoTargetDirectory,
  target,
  "release",
  "nexa_zed_extension.wasm",
);
const packagedExtension = path.join(extensionDirectory, "extension.wasm");

if (!fs.existsSync(path.join(extensionDirectory, "extension.toml"))) {
  throw new Error(
    "the Zed extension has not been prepared; run `pnpm run prepare:zed` first",
  );
}

const result = spawnSync(
  "cargo",
  [
    "build",
    "--manifest-path",
    manifest,
    "--locked",
    "--release",
    "--target",
    target,
    "--target-dir",
    cargoTargetDirectory,
  ],
  {
    cwd: extensionDirectory,
    encoding: "utf8",
    stdio: "inherit",
  },
);

if (result.error) {
  throw result.error;
}
if (result.status !== 0) {
  throw new Error(
    `Zed WebAssembly build failed for ${target}; install it with ` +
      "`rustup target add wasm32-wasip2`",
  );
}
if (!fs.existsSync(builtExtension)) {
  throw new Error(
    `cargo reported success but did not produce ${builtExtension}`,
  );
}

fs.copyFileSync(builtExtension, packagedExtension);
const wasm = fs.readFileSync(packagedExtension);
if (
  wasm.length < 1024 ||
  !wasm.subarray(0, 4).equals(Buffer.from([0x00, 0x61, 0x73, 0x6d]))
) {
  fs.rmSync(packagedExtension, { force: true });
  throw new Error(
    `cargo did not produce a valid non-empty WebAssembly extension at ${packagedExtension}`,
  );
}

console.log(packagedExtension);
