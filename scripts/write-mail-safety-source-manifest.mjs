#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";

import { SOURCE_SNAPSHOT_PATHS, sourceSnapshot } from "./run-mail-safety.mjs";

function fail(message) {
  throw new Error(message);
}

function sha256(contents) {
  return createHash("sha256").update(contents).digest("hex");
}

function isWithin(root, path) {
  const relativePath = relative(root, path);
  return (
    relativePath === "" ||
    (!relativePath.startsWith("..") && !isAbsolute(relativePath))
  );
}

function sourcePaths() {
  return [
    ...new Set(
      execFileSync(
        "git",
        [
          "ls-files",
          "--cached",
          "--others",
          "--exclude-standard",
          "-z",
          "--",
          ...SOURCE_SNAPSHOT_PATHS,
        ],
        { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 },
      )
        .split("\0")
        .filter(Boolean),
    ),
  ].sort();
}

function sourceManifest() {
  const files = sourcePaths().map((path) => {
    let stat;
    try {
      stat = lstatSync(path);
    } catch (error) {
      fail(
        `Governed source input is missing: ${path} (${error.code ?? "unknown error"})`,
      );
    }
    if (stat.isSymbolicLink())
      fail(`Governed source input is a symlink: ${path}`);
    if (!stat.isFile())
      fail(`Governed source input is not a regular file: ${path}`);
    const contents = readFileSync(path);
    return {
      path,
      bytes: contents.byteLength,
      sha256: sha256(contents),
      state: "present",
    };
  });
  const hash = createHash("sha256");
  for (const file of files) {
    hash.update(file.path).update("\0");
    hash.update(Buffer.from(file.sha256, "hex")).update("\0");
  }
  return {
    algorithm: "sha256-path-and-content-manifest",
    digest: hash.digest("hex"),
    fileCount: files.length,
    files,
  };
}

function outsideCheckout(path, checkout, label) {
  const resolved = resolve(path);
  const parent = dirname(resolved);
  let realParent;
  try {
    realParent = realpathSync(parent);
  } catch (error) {
    fail(
      `${label} parent is unavailable: ${parent} (${error.code ?? "unknown error"})`,
    );
  }
  const realOutput = resolve(realParent, relative(parent, resolved));
  if (isWithin(checkout, resolved) || isWithin(checkout, realOutput)) {
    fail(`${label} must be outside the source checkout`);
  }
  try {
    lstatSync(resolved);
    fail(`${label} already exists: ${resolved}`);
  } catch (error) {
    if (error instanceof Error && error.message.includes("already exists"))
      throw error;
    if (error?.code !== "ENOENT") throw error;
  }
  return resolved;
}

function exclusiveJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`, {
    encoding: "utf8",
    flag: "wx",
    mode: 0o600,
  });
  chmodSync(path, 0o600);
}

export function writeMailSafetySourceManifest({
  output,
  summaryOutput = null,
  snapshot = sourceSnapshot,
} = {}) {
  if (!output) fail("Missing --output");
  const checkout = realpathSync(process.cwd());
  const manifestPath = outsideCheckout(output, checkout, "Manifest output");
  const summaryPath = summaryOutput
    ? outsideCheckout(summaryOutput, checkout, "Summary output")
    : null;
  if (summaryPath === manifestPath)
    fail("Summary output must differ from manifest output");

  const before = snapshot();
  if (!before) fail("Unable to create the governed source snapshot");
  const manifest = sourceManifest();
  if (
    before.algorithm !== manifest.algorithm ||
    before.digest !== manifest.digest ||
    before.fileCount !== manifest.fileCount
  ) {
    fail("Governed source changed while the manifest was being computed");
  }
  const after = snapshot();
  if (
    !after ||
    after.algorithm !== manifest.algorithm ||
    after.digest !== manifest.digest ||
    after.fileCount !== manifest.fileCount
  ) {
    fail("Governed source changed before the manifest could be written");
  }

  exclusiveJson(manifestPath, manifest);
  if (summaryPath) {
    exclusiveJson(summaryPath, {
      sourceDigest: manifest.digest,
      sourceManifestSha256: sha256(readFileSync(manifestPath)),
      sourceFileCount: manifest.fileCount,
    });
  }
  return { manifestPath, manifest, summaryPath };
}

export function parseArgs(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    const value = args[index + 1];
    if (
      !["--output", "--summary-output"].includes(option) ||
      !value ||
      value.startsWith("--")
    ) {
      fail(
        `Use --output PATH [--summary-output PATH], got ${option ?? "<missing>"}`,
      );
    }
    options[option.slice(2).replaceAll("-", "_")] = value;
    index += 1;
  }
  if (!options.output) fail("Missing --output");
  return options;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const options = parseArgs(process.argv.slice(2));
    writeMailSafetySourceManifest({
      output: options.output,
      summaryOutput: options.summary_output,
    });
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
