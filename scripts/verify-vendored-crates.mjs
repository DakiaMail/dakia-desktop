#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  readdirSync,
  readFileSync,
  realpathSync,
} from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";

const ROOT = resolve(dirname(new URL(import.meta.url).pathname), "..");
const MANIFEST_DIRECTORY = "testdata/vendored-crates";
const EXCLUDED_PROJECT_DIRECTORIES = new Set([
  ".git",
  "node_modules",
  "target",
  "vendor",
]);

function fail(message) {
  throw new Error(`Vendored crate integrity check failed: ${message}`);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function relativePath(root, path) {
  const value = relative(root, path);
  if (!value || value === ".." || value.startsWith(`..${sep}`)) {
    fail(`path escapes its declared root: ${path}`);
  }
  return value.split(sep).join("/");
}

function listRegularFiles(root) {
  const files = new Map();

  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const path = join(directory, entry.name);
      const info = lstatSync(path);
      if (info.isSymbolicLink()) fail(`vendor tree contains a symlink: ${path}`);
      if (info.isDirectory()) {
        visit(path);
      } else if (info.isFile()) {
        files.set(relativePath(root, path), sha256(path));
      } else {
        fail(`vendor tree contains a non-regular file: ${path}`);
      }
    }
  }

  visit(root);
  return files;
}

function parsePathPatches(cargoToml) {
  const patches = [];
  let patchSource = null;

  for (const rawLine of readFileSync(cargoToml, "utf8").split(/\r?\n/)) {
    const line = rawLine.trim();
    if (line.startsWith("[") && line.endsWith("]")) {
      const patchTable = line.match(/^\[patch\.(?:"([^"]+)"|([^\]]+))]$/);
      patchSource = patchTable?.[1] ?? patchTable?.[2] ?? null;
      continue;
    }
    if (!patchSource || !line || line.startsWith("#")) continue;

    const match = line.match(
      /^([A-Za-z0-9_-]+)\s*=\s*\{\s*path\s*=\s*"([^"]+)"\s*}\s*(?:#.*)?$/,
    );
    if (!match) {
      fail(
        `unsupported [patch.crates-io] declaration must use a single-line path mapping: ${rawLine}`,
      );
    }
    if (patches.some((patch) => patch.name === match[1])) {
      fail(`multiple path patches for ${match[1]} are not supported`);
    }
    patches.push({ name: match[1], path: match[2], source: patchSource });
  }

  return patches;
}

function findProjectCargoTomls(root) {
  const manifests = [];

  function visit(directory) {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      if (entry.isDirectory()) {
        if (!EXCLUDED_PROJECT_DIRECTORIES.has(entry.name)) visit(join(directory, entry.name));
      } else if (entry.isFile() && entry.name === "Cargo.toml") {
        manifests.push(join(directory, entry.name));
      }
    }
  }

  visit(root);
  return manifests.sort();
}

function readManifest(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(`cannot read provenance manifest ${path}: ${error.message}`);
  }
}

function requireString(value, field, manifestPath) {
  if (typeof value !== "string" || value.length === 0) {
    fail(`${manifestPath}: ${field} must be a non-empty string`);
  }
  return value;
}

function indexByPath(entries, field, manifestPath) {
  if (!Array.isArray(entries)) fail(`${manifestPath}: ${field} must be an array`);
  const indexed = new Map();
  for (const entry of entries) {
    const path = requireString(entry?.path, `${field}[].path`, manifestPath);
    if (path.includes("\\") || path.startsWith("/") || path.split("/").includes("..")) {
      fail(`${manifestPath}: ${field} contains an unsafe path ${path}`);
    }
    if (indexed.has(path)) fail(`${manifestPath}: duplicate ${field} path ${path}`);
    indexed.set(path, entry);
  }
  return indexed;
}

function verifyManifest({
  root,
  patch,
  metadataPackages,
  manifestDirectory,
  workspaceManifest,
}) {
  const vendorRoot = resolve(dirname(workspaceManifest), patch.path);
  const manifestPath = join(
    root,
    manifestDirectory,
    `${patch.name}.json`,
  );
  const manifest = readManifest(manifestPath);

  if (manifest.schemaVersion !== 1) fail(`${manifestPath}: unsupported schemaVersion`);
  if (manifest.name !== patch.name) fail(`${manifestPath}: name does not match patch ${patch.name}`);
  requireString(manifest.version, "version", manifestPath);
  if (!/^[a-f0-9]{64}$/.test(manifest.registryChecksum ?? "")) {
    fail(`${manifestPath}: registryChecksum must be a lowercase SHA-256 digest`);
  }
  requireString(manifest.upstreamRepository, "upstreamRepository", manifestPath);
  if (!/^[a-f0-9]{40}$/.test(manifest.upstreamRevision ?? "")) {
    fail(`${manifestPath}: upstreamRevision must be a lowercase 40-hex commit`);
  }
  requireString(manifest.license, "license", manifestPath);
  if (!manifest.review || manifest.review.required !== "independent-review") {
    fail(`${manifestPath}: review.required must be independent-review`);
  }
  requireString(manifest.review.evidence, "review.evidence", manifestPath);

  const expectedVendorRoot = resolve(root, manifest.vendorPath ?? relative(root, vendorRoot));
  if (expectedVendorRoot !== vendorRoot) {
    fail(`${manifestPath}: vendorPath does not match Cargo patch path`);
  }

  const sameNamePackages = metadataPackages.filter(
    (pkg) => pkg.name === manifest.name,
  );
  if (
    sameNamePackages.length !== 1 ||
    sameNamePackages[0].version !== manifest.version ||
    sameNamePackages[0].source !== null
  ) {
    fail(
      `${manifestPath}: cargo metadata must not include a competing registry or version-resolved package for ${manifest.name}`,
    );
  }

  const matchingPackages = sameNamePackages.filter(
    (pkg) =>
      realpathSync(pkg.manifest_path) === join(vendorRoot, "Cargo.toml"),
  );
  if (matchingPackages.length !== 1) {
    fail(
      `${manifestPath}: cargo metadata must resolve exactly one path package at ${manifest.name}@${manifest.version}`,
    );
  }

  const upstream = indexByPath(manifest.upstreamFiles, "upstreamFiles", manifestPath);
  if (upstream.size === 0) fail(`${manifestPath}: upstreamFiles must not be empty`);
  const changes = indexByPath(manifest.allowedChanges ?? [], "allowedChanges", manifestPath);
  const vendorOnly = indexByPath(manifest.vendorOnlyFiles ?? [], "vendorOnlyFiles", manifestPath);
  const omitted = indexByPath(manifest.omittedUpstreamFiles ?? [], "omittedUpstreamFiles", manifestPath);
  const current = listRegularFiles(vendorRoot);

  for (const [path, upstreamEntry] of upstream) {
    const upstreamSha = requireString(upstreamEntry.sha256, `upstreamFiles[${path}].sha256`, manifestPath);
    const change = changes.get(path);
    const omittedEntry = omitted.get(path);
    if (change && omittedEntry) fail(`${manifestPath}: ${path} cannot be both changed and omitted`);
    if (omittedEntry) {
      if (current.has(path)) fail(`${manifestPath}: omitted upstream file still exists: ${path}`);
      if (requireString(omittedEntry.upstreamSha256, `omittedUpstreamFiles[${path}].upstreamSha256`, manifestPath) !== upstreamSha) {
        fail(`${manifestPath}: omitted upstream hash does not match the upstream manifest: ${path}`);
      }
      requireString(omittedEntry.reason, `omittedUpstreamFiles[${path}].reason`, manifestPath);
      continue;
    }
    if (!current.has(path)) fail(`${manifestPath}: upstream file is missing: ${path}`);
    const actual = current.get(path);
    if (!change) {
      if (actual !== upstreamSha) fail(`${manifestPath}: unapproved upstream file change: ${path}`);
      continue;
    }
    if (requireString(change.upstreamSha256, `allowedChanges[${path}].upstreamSha256`, manifestPath) !== upstreamSha) {
      fail(`${manifestPath}: allowed change upstream hash does not match upstream manifest: ${path}`);
    }
    if (requireString(change.patchedSha256, `allowedChanges[${path}].patchedSha256`, manifestPath) !== actual) {
      fail(`${manifestPath}: allowed change current hash does not match: ${path}`);
    }
    requireString(change.reason, `allowedChanges[${path}].reason`, manifestPath);
  }

  for (const [path, entry] of changes) {
    if (!upstream.has(path)) fail(`${manifestPath}: allowed change is not an upstream file: ${path}`);
    if (!current.has(path)) fail(`${manifestPath}: allowed change file is missing: ${path}`);
    if (!entry.upstreamSha256 || !entry.patchedSha256) fail(`${manifestPath}: incomplete allowed change: ${path}`);
  }
  for (const [path, entry] of omitted) {
    if (!upstream.has(path)) fail(`${manifestPath}: omitted file is not an upstream file: ${path}`);
    if (!entry.upstreamSha256 || !entry.reason) fail(`${manifestPath}: incomplete omitted file record: ${path}`);
  }
  for (const [path, actual] of current) {
    if (upstream.has(path)) continue;
    const entry = vendorOnly.get(path);
    if (!entry) fail(`${manifestPath}: unexpected vendor-only file: ${path}`);
    if (requireString(entry.sha256, `vendorOnlyFiles[${path}].sha256`, manifestPath) !== actual) {
      fail(`${manifestPath}: vendor-only file hash does not match: ${path}`);
    }
    requireString(entry.reason, `vendorOnlyFiles[${path}].reason`, manifestPath);
  }
  for (const [path, entry] of vendorOnly) {
    if (upstream.has(path)) fail(`${manifestPath}: vendor-only file is also upstream: ${path}`);
    if (!current.has(path)) fail(`${manifestPath}: vendor-only file is missing: ${path}`);
    if (!entry.sha256 || !entry.reason) fail(`${manifestPath}: incomplete vendor-only record: ${path}`);
  }

  return {
    name: manifest.name,
    version: manifest.version,
    manifestPath,
    workspaceManifest: relativePath(root, workspaceManifest),
  };
}

function metadataForWorkspace(metadata, workspaceManifest, root) {
  if (Array.isArray(metadata?.packages)) return metadata;
  const key = relativePath(root, workspaceManifest);
  const selected = metadata?.[key];
  if (!Array.isArray(selected?.packages)) {
    fail(`cargo metadata package list is required for ${key}`);
  }
  return selected;
}

export function verifyVendoredCrates({
  root = ROOT,
  metadata,
  manifestDirectory = MANIFEST_DIRECTORY,
} = {}) {
  const resolvedRoot = realpathSync(root);
  const verified = [];
  for (const workspaceManifest of findProjectCargoTomls(resolvedRoot)) {
    const patches = parsePathPatches(workspaceManifest);
    if (patches.length === 0) continue;
    const metadataPackages = metadataForWorkspace(
      metadata,
      workspaceManifest,
      resolvedRoot,
    ).packages;
    for (const patch of patches) {
      verified.push(
        verifyManifest({
          root: resolvedRoot,
          patch,
          metadataPackages,
          manifestDirectory,
          workspaceManifest,
        }),
      );
    }
  }
  return verified;
}

function main() {
  const metadata = {};
  for (const workspaceManifest of findProjectCargoTomls(ROOT)) {
    const manifestPath = relativePath(ROOT, workspaceManifest);
    if (parsePathPatches(workspaceManifest).length === 0) continue;
    const args = ["metadata", "--locked", "--offline", "--format-version", "1"];
    if (manifestPath !== "Cargo.toml") args.splice(1, 0, "--manifest-path", workspaceManifest);
    const output = execFileSync("cargo", args, {
      cwd: ROOT,
      encoding: "utf8",
      maxBuffer: 10 * 1024 * 1024,
    });
    metadata[manifestPath] = JSON.parse(output);
  }
  const verified = verifyVendoredCrates({ metadata });
  for (const crate of verified) {
    process.stdout.write(
      `Verified vendored ${crate.name}@${crate.version} provenance and exact approved diff for ${crate.workspaceManifest}. cargo-audit separately evaluates the upstream package version against RustSec advisories.\n`,
    );
  }
}

if (import.meta.url === new URL(process.argv[1], "file:").href) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}
