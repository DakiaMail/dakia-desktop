#!/usr/bin/env node
/**
 * Prepares and compares private, fictional native profile fixtures.
 *
 * This deliberately has no knowledge of application support, app bundles, or
 * application lifecycle.  It only copies a sealed, caller-owned profile into
 * a new private temporary directory and records byte-level provenance.
 */
import { createHash } from "node:crypto";
import { open } from "node:fs/promises";
import {
  chmod,
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";

const CONTRACT = "dakia-native-profile-fixture-v1";
const MAX_FILES = 32;
const MAX_FILE_BYTES = 512 * 1024 * 1024;
const MAX_TOTAL_BYTES = 768 * 1024 * 1024;
const CORE_FILES = new Set([
  "dakia.db",
  "dakia.db-wal",
  "dakia.db-shm",
  "vault.key",
]);
const MARKER = /^\.dakia-[A-Za-z0-9][A-Za-z0-9._-]{0,120}$/;
const SHA256 = /^[a-f0-9]{64}$/;
const uid = process.getuid?.();

function fail(message) {
  throw new Error(message);
}

function isWithin(parent, candidate) {
  return candidate === parent || candidate.startsWith(`${parent}${sep}`);
}

async function canonicalInputPath(path) {
  const resolved = resolve(path);
  const parent = await realpath(dirname(resolved));
  return join(parent, resolved.split(sep).at(-1));
}

function hash(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function exactObject(value, keys, label) {
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify([...keys].sort())
  )
    fail(`${label} has an invalid contract`);
}

function validateName(name, markers) {
  return CORE_FILES.has(name) || (markers.has(name) && MARKER.test(name));
}

async function ownedDirectory(path, label, { mode, modes } = {}) {
  const canonical = await realpath(path);
  if (canonical !== (await canonicalInputPath(path)))
    fail(`${label} path is not canonical`);
  const info = await lstat(canonical);
  if (!info.isDirectory() || info.isSymbolicLink())
    fail(`${label} is not a real directory`);
  if (uid !== undefined && info.uid !== uid)
    fail(`${label} is not owned by this user`);
  if (mode !== undefined && (info.mode & 0o777) !== mode) {
    fail(`${label} mode is not ${mode.toString(8)}`);
  }
  if (modes && !modes.includes(info.mode & 0o777))
    fail(`${label} mode is invalid`);
  return canonical;
}

async function ownedRegular(
  path,
  label,
  { modes, maxBytes = MAX_FILE_BYTES } = {},
) {
  const canonical = await realpath(path);
  if (canonical !== (await canonicalInputPath(path)))
    fail(`${label} path is not canonical`);
  const info = await lstat(canonical);
  if (!info.isFile() || info.isSymbolicLink() || info.nlink !== 1) {
    fail(`${label} is not an unlinked regular file`);
  }
  if (uid !== undefined && info.uid !== uid)
    fail(`${label} is not owned by this user`);
  if (modes && !modes.includes(info.mode & 0o777))
    fail(`${label} mode is invalid`);
  if (info.size < 0 || info.size > maxBytes) fail(`${label} size is invalid`);
  return { path: canonical, info };
}

async function readManifest(path) {
  const file = await ownedRegular(path, "manifest", {
    modes: [0o600],
    maxBytes: 128 * 1024,
  });
  let value;
  try {
    value = JSON.parse(await readFile(file.path, "utf8"));
  } catch {
    fail("manifest is not valid JSON");
  }
  exactObject(
    value,
    [
      "contract",
      "expectedAbsentFiles",
      "files",
      "ownedMarkers",
      "schemaVersion",
    ],
    "manifest",
  );
  if (value.contract !== CONTRACT || value.schemaVersion !== 1)
    fail("manifest contract is invalid");
  if (
    !Array.isArray(value.ownedMarkers) ||
    !Array.isArray(value.expectedAbsentFiles) ||
    !Array.isArray(value.files)
  ) {
    fail("manifest arrays are invalid");
  }
  if (value.files.length < 2 || value.files.length > MAX_FILES)
    fail("manifest file count is invalid");
  const markers = new Set();
  for (const name of value.ownedMarkers) {
    if (typeof name !== "string" || !MARKER.test(name) || markers.has(name))
      fail("manifest marker is invalid");
    markers.add(name);
  }
  const expectedAbsent = new Set();
  for (const name of value.expectedAbsentFiles) {
    if (
      typeof name !== "string" ||
      !validateName(name, markers) ||
      expectedAbsent.has(name)
    ) {
      fail("manifest expected-absent file is invalid");
    }
    expectedAbsent.add(name);
  }
  const files = [];
  const names = new Set();
  let total = 0;
  for (const item of value.files) {
    exactObject(item, ["bytes", "mode", "path", "sha256"], "manifest file");
    if (
      typeof item.path !== "string" ||
      !validateName(item.path, markers) ||
      names.has(item.path) ||
      expectedAbsent.has(item.path) ||
      !Number.isInteger(item.bytes) ||
      item.bytes < 0 ||
      item.bytes > MAX_FILE_BYTES ||
      !SHA256.test(item.sha256) ||
      ![0o400, 0o444, 0o600].includes(item.mode)
    )
      fail("manifest file entry is invalid");
    names.add(item.path);
    total += item.bytes;
    if (total > MAX_TOTAL_BYTES) fail("manifest total size is invalid");
    files.push({ ...item });
  }
  if (!names.has("dakia.db") || !names.has("vault.key"))
    fail("manifest lacks required profile files");
  return {
    path: file.path,
    files,
    expectedAbsentFiles: [...expectedAbsent].sort(),
    ownedMarkers: [...markers].sort(),
  };
}

async function inventoryProfile(source, manifest, { requireManifestMatch }) {
  const root = await ownedDirectory(source, "sealed source directory", {
    modes: [0o500, 0o700],
  });
  const expected = new Map(manifest.files.map((item) => [item.path, item]));
  const listed = await readdir(root);
  if (listed.length !== expected.size)
    fail("sealed source directory inventory differs from manifest");
  const entries = [];
  for (const name of listed.sort()) {
    if (!expected.has(name))
      fail("sealed source directory has an unmanifested file");
    const path = join(root, name);
    const file = await ownedRegular(path, `profile file ${name}`, {
      modes: [expected.get(name).mode],
    });
    if (relative(root, file.path) !== name)
      fail("profile file escapes the sealed source directory");
    const bytes = await readFile(file.path);
    const entry = {
      path: name,
      bytes: bytes.length,
      mode: file.info.mode & 0o777,
      sha256: hash(bytes),
    };
    if (requireManifestMatch) {
      const declared = expected.get(name);
      if (
        entry.bytes !== declared.bytes ||
        entry.mode !== declared.mode ||
        entry.sha256 !== declared.sha256
      )
        fail(`profile file ${name} differs from manifest`);
    }
    entries.push(entry);
  }
  for (const name of manifest.expectedAbsentFiles) {
    try {
      await lstat(join(root, name));
      fail(`expected-absent profile file ${name} exists`);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
  }
  return {
    root,
    entries: entries.sort((a, b) => a.path.localeCompare(b.path)),
  };
}

async function requireNewTemporaryOutput(path) {
  const target = resolve(path);
  const temporary = await realpath(tmpdir());
  try {
    await lstat(target);
    fail("output root already exists");
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
  }
  const parent = dirname(target);
  const canonicalParent = await ownedDirectory(parent, "output root parent");
  const canonicalTarget = join(canonicalParent, target.split(sep).at(-1));
  if (!isWithin(temporary, canonicalTarget) || canonicalTarget === temporary) {
    fail("output root is not under the OS temporary directory");
  }
  await mkdir(target, { mode: 0o700 });
  const root = await ownedDirectory(target, "new output root", { mode: 0o700 });
  if (root !== canonicalTarget) fail("new output root path is not canonical");
  return root;
}

async function writeExclusive(path, bytes, mode = 0o600) {
  const handle = await open(path, "wx", mode);
  try {
    await handle.writeFile(bytes);
    await handle.sync();
  } finally {
    await handle.close();
  }
  await chmod(path, mode);
  const info = await lstat(path);
  if (
    !info.isFile() ||
    info.isSymbolicLink() ||
    info.nlink !== 1 ||
    (info.mode & 0o777) !== mode
  ) {
    fail("exclusive output file changed during creation");
  }
}

async function copyProfile(source, manifest, outputRoot) {
  const before = await inventoryProfile(source, manifest, {
    requireManifestMatch: true,
  });
  const profile = join(outputRoot, "profile");
  await mkdir(profile, { mode: 0o700 });
  await ownedDirectory(profile, "copied profile directory", { mode: 0o700 });
  const copied = [];
  for (const entry of before.entries) {
    const sourcePath = join(before.root, entry.path);
    const bytes = await readFile(sourcePath);
    if (hash(bytes) !== entry.sha256 || bytes.length !== entry.bytes)
      fail(`source file ${entry.path} changed before copy`);
    const destination = join(profile, entry.path);
    await writeExclusive(destination, bytes, 0o600);
    const observed = await ownedRegular(
      destination,
      `copied profile file ${entry.path}`,
      { modes: [0o600] },
    );
    const copiedBytes = await readFile(observed.path);
    const copyEntry = {
      path: entry.path,
      bytes: copiedBytes.length,
      mode: observed.info.mode & 0o777,
      sha256: hash(copiedBytes),
    };
    if (copyEntry.bytes !== entry.bytes || copyEntry.sha256 !== entry.sha256)
      fail(`copied profile file ${entry.path} is not faithful`);
    copied.push(copyEntry);
  }
  const after = await inventoryProfile(source, manifest, {
    requireManifestMatch: true,
  });
  if (JSON.stringify(before.entries) !== JSON.stringify(after.entries))
    fail("sealed source changed while it was copied");
  return {
    before: before.entries,
    after: after.entries,
    copied: copied.sort((a, b) => a.path.localeCompare(b.path)),
  };
}

export async function prepareFixture({
  source,
  manifest: manifestPath,
  outputRoot,
}) {
  const manifest = await readManifest(manifestPath);
  const output = await requireNewTemporaryOutput(outputRoot);
  const result = await copyProfile(source, manifest, output);
  const receipt = {
    schemaVersion: 1,
    contract: CONTRACT,
    operation: "prepare",
    expectedAbsentFiles: manifest.expectedAbsentFiles,
    sourceBefore: result.before,
    sourceAfter: result.after,
    copied: result.copied,
    sourceUnchangedAfterCopy:
      JSON.stringify(result.before) === JSON.stringify(result.after),
    applicationLifecycleTouched: false,
    applicationSupportTouched: false,
  };
  await writeExclusive(
    join(output, "receipt.json"),
    `${JSON.stringify(receipt, null, 2)}\n`,
  );
  return receipt;
}

async function inventoryExistingProfile(path, manifest) {
  const root = await ownedDirectory(path, "comparison directory");
  const expected = new Map(manifest.files.map((item) => [item.path, item]));
  const entries = [];
  const names = await readdir(root);
  if (names.length > MAX_FILES)
    fail("comparison directory file count is invalid");
  let total = 0;
  for (const name of names.sort()) {
    const file = await ownedRegular(
      join(root, name),
      `comparison file ${name}`,
      { modes: [0o400, 0o444, 0o600] },
    );
    total += file.info.size;
    if (total > MAX_TOTAL_BYTES)
      fail("comparison directory total size is invalid");
    const bytes = await readFile(file.path);
    entries.push({
      path: name,
      bytes: bytes.length,
      mode: file.info.mode & 0o777,
      sha256: hash(bytes),
      recognized: expected.has(name),
    });
  }
  return { root, entries };
}

export async function compareFixture({
  baseline: baselinePath,
  after,
  policy,
}) {
  const manifest = await readManifest(baselinePath);
  if (policy !== "generic" && policy !== "future-profile-durable")
    fail("comparison policy is invalid");
  const observed = await inventoryExistingProfile(after, manifest);
  const report = {
    schemaVersion: 1,
    contract: CONTRACT,
    operation: "compare",
    policy,
    inventory: observed.entries,
    result: policy === "generic" ? "inventory-only" : "passed",
  };
  if (policy === "generic") return report;

  const baseline = new Map(manifest.files.map((item) => [item.path, item]));
  const afterByPath = new Map(
    observed.entries.map((item) => [item.path, item]),
  );
  const durable = ["dakia.db", "vault.key", ...manifest.ownedMarkers];
  for (const name of durable) {
    const initial = baseline.get(name);
    const later = afterByPath.get(name);
    if (
      !initial ||
      !later ||
      initial.bytes !== later.bytes ||
      initial.sha256 !== later.sha256
    ) {
      fail(`future-profile durable file changed: ${name}`);
    }
  }
  if (!manifest.expectedAbsentFiles.includes("dakia.db-wal")) {
    fail(
      "future-profile durable policy requires dakia.db-wal to be declared absent",
    );
  }
  try {
    await lstat(join(observed.root, "dakia.db-wal"));
    fail(
      "future-profile durable policy requires dakia.db-wal to remain absent",
    );
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
  }
  report.durableFiles = durable.sort();
  return report;
}

function parseArguments(argv) {
  const [command, ...rest] = argv;
  const values = new Map();
  for (let index = 0; index < rest.length; index += 2) {
    const key = rest[index];
    const value = rest[index + 1];
    if (!key?.startsWith("--") || value === undefined || values.has(key))
      fail("arguments are invalid");
    values.set(key, value);
  }
  if (
    command === "prepare" &&
    [...values.keys()].sort().join(",") === "--manifest,--output-root,--source"
  ) {
    return {
      command,
      source: values.get("--source"),
      manifest: values.get("--manifest"),
      outputRoot: values.get("--output-root"),
    };
  }
  if (
    command === "compare" &&
    [...values.keys()].sort().join(",") === "--after,--baseline,--policy"
  ) {
    return {
      command,
      baseline: values.get("--baseline"),
      after: values.get("--after"),
      policy: values.get("--policy"),
    };
  }
  fail(
    "usage: native-profile-fixture.mjs prepare --source DIR --manifest FILE --output-root NEW_TEMP_DIR | compare --baseline FILE --after DIR --policy generic|future-profile-durable",
  );
}

async function main() {
  const input = parseArguments(process.argv.slice(2));
  const result =
    input.command === "prepare"
      ? await prepareFixture(input)
      : await compareFixture(input);
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    process.stderr.write(`native-profile-fixture: ${error.message}\n`);
    process.exitCode = 1;
  });
}
