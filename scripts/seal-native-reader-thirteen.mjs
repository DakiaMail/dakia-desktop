#!/usr/bin/env node
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  readFile,
  readdir,
  readlink,
  realpath,
  writeFile,
} from "node:fs/promises";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { FIXTURES } from "./native-reader-thirteen-overlay.mjs";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const identityContract = "dakia-native-provider-reader-approved-identity-v1";
const approvalPreflightRelativePath =
  "docs/mail-safety-results/native-packaged-same-identity-preflight-2026-10-21.json";
const ownedRuntimeHelpers = [
  [
    "overlay",
    "native-reader-thirteen-overlay.mjs",
    "scripts/native-reader-thirteen-overlay.mjs",
  ],
  [
    "seeder",
    "seed-native-reader-thirteen-fixtures.mjs",
    "scripts/seed-native-reader-thirteen-fixtures.mjs",
  ],
];
const expectedUids = FIXTURES.map(([uid]) => uid);
const expectedIds = FIXTURES.map(([, id]) => id);
const uidToFixture = new Map(
  FIXTURES.map(([uid, id, , digest]) => [uid, { id, digest }]),
);
const expectedVisible = new Map([
  [4, ["Synthetic nested alternatives", "Visible HTML body."]],
  [
    5,
    [
      "Synthetic attached message",
      "The attached message is synthetic and fully redacted.",
    ],
  ],
  [6, ["Synthetic multipart attachment", "Visible parent body."]],
  [7, ["Synthetic empty MIME leaf", "Visible HTML after an empty plain part."]],
  [
    8,
    [
      "Synthetic truncated MIME",
      "The second leaf and closing boundary are deliberately missing.",
    ],
  ],
  [9, ["Synthetic charset matrix", "Zażółć"]],
  [10, ["Synthetic flowed text", "This is the HTML alternative."]],
  [11, ["Fictional 🌍 reader message", "مرحبا بالعالم"]],
  [12, ["Synthetic ambiguous inline identities", "Fictional resources"]],
  [
    13,
    [
      "CASE-12345, Example Street 1",
      "Redacted message content for a fictional insurance case.",
    ],
  ],
  [14, ["Synthetic filename parameters", "Synthetic filename coverage."]],
  [
    15,
    [
      "Redacted LinkedIn notification",
      "Redacted HTML=content with a foldedline.",
    ],
  ],
  [16, ["Synthetic unknown transfer encoding", "Visible safe body."]],
]);

async function requireOwnedFile(path, label, { within, modes = [0o600] } = {}) {
  const canonical = await realpath(path);
  if (canonical !== resolve(path))
    throw new Error(`${label} path is not canonical`);
  if (
    within &&
    !(canonical === within || canonical.startsWith(`${within}${sep}`))
  )
    throw new Error(`${label} is outside the owned run root`);
  const info = await lstat(canonical);
  if (
    !info.isFile() ||
    info.isSymbolicLink() ||
    !modes.includes(info.mode & 0o777) ||
    (process.getuid?.() !== undefined && info.uid !== process.getuid())
  )
    throw new Error(`${label} is not an owned regular file`);
  return { path: canonical, bytes: await readFile(canonical), info };
}

async function requireRunRoot(path) {
  const canonical = await realpath(path);
  if (
    canonical !== resolve(path) ||
    !/^dakia-native-provider-reader\.[A-Za-z0-9]{6}$/.test(basename(canonical))
  )
    throw new Error(
      "run root is outside the exact fictional harness namespace",
    );
  const info = await lstat(canonical);
  if (
    !info.isDirectory() ||
    info.isSymbolicLink() ||
    (info.mode & 0o777) !== 0o700 ||
    (process.getuid?.() !== undefined && info.uid !== process.getuid())
  )
    throw new Error("run root is not an owned directory");
  const marker = await requireOwnedFile(
    join(canonical, ".dakia-native-mail-safety-root"),
    "run root marker",
    { within: canonical },
  );
  if (marker.bytes.toString("utf8") !== "dakia-native-mail-safety-fixture-v1\n")
    throw new Error("run root marker is invalid");
  return canonical;
}

async function requireOwnedDirectory(path, label, { within } = {}) {
  const canonical = await realpath(path);
  const info = await lstat(canonical);
  if (
    canonical !== resolve(path) ||
    (within &&
      !(canonical === within || canonical.startsWith(`${within}${sep}`))) ||
    !info.isDirectory() ||
    info.isSymbolicLink() ||
    (process.getuid?.() !== undefined && info.uid !== process.getuid())
  )
    throw new Error(`${label} is not an owned canonical directory`);
  return canonical;
}

async function requireOwnedExecutable(path, label) {
  const canonical = await realpath(path);
  const info = await lstat(canonical);
  if (
    canonical !== resolve(path) ||
    !info.isFile() ||
    info.isSymbolicLink() ||
    (info.mode & 0o777) !== 0o755 ||
    (process.getuid?.() !== undefined && info.uid !== process.getuid())
  )
    throw new Error(`${label} is not an owned executable`);
  const bytes = await readFile(canonical);
  return { path: canonical, bytes, info };
}

function exactObject(value, keys, label) {
  if (
    !value ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    JSON.stringify(Object.keys(value).sort()) !==
      JSON.stringify([...keys].sort())
  )
    throw new Error(`${label} contract is invalid`);
}

function parseJson(bytes, label) {
  try {
    return JSON.parse(bytes);
  } catch {
    throw new Error(`${label} is not valid JSON`);
  }
}

async function requireSourceRoot(path) {
  const canonical = await realpath(path);
  const info = await lstat(canonical);
  if (
    canonical !== resolve(path) ||
    !info.isDirectory() ||
    info.isSymbolicLink() ||
    (process.getuid?.() !== undefined && info.uid !== process.getuid())
  )
    throw new Error("source root is not an owned canonical directory");
  return canonical;
}

export async function validateLiveSourceManifest(manifest, sourceRoot) {
  const paths = new Set();
  const digest = createHash("sha256");
  for (const item of manifest.files) {
    if (
      !item ||
      Object.keys(item).sort().join(",") !== "bytes,path,sha256,state" ||
      item.state !== "present" ||
      typeof item.path !== "string" ||
      item.path.startsWith("/") ||
      item.path.split("/").includes("..") ||
      !/^[a-f0-9]{64}$/.test(item.sha256) ||
      !Number.isInteger(item.bytes) ||
      item.bytes < 0 ||
      paths.has(item.path)
    )
      throw new Error("source manifest entry is invalid");
    paths.add(item.path);
    const path = join(sourceRoot, item.path);
    const info = await lstat(path);
    if (
      !info.isFile() ||
      info.isSymbolicLink() ||
      info.size !== item.bytes ||
      sha256(await readFile(path)) !== item.sha256
    )
      throw new Error(`live source differs from frozen manifest: ${item.path}`);
    digest
      .update(item.path)
      .update("\0")
      .update(Buffer.from(item.sha256, "hex"))
      .update("\0");
  }
  if (JSON.stringify([...paths]) !== JSON.stringify([...paths].sort()))
    throw new Error("source manifest paths are not sorted");
  if (digest.digest("hex") !== manifest.digest)
    throw new Error("source manifest digest does not match live source");
}

function validateApprovedIdentityPreflight({
  preflight,
  preflightBytes,
  approvedRecord,
  approvedRecordFile,
  approvedRoot,
  approvedApp,
  expectedApprovalRecordSha256,
}) {
  const approvedIdentity = preflight?.approvedIdentity;
  exactObject(
    approvedIdentity,
    [
      "record",
      "recordSha256",
      "contract",
      "root",
      "app",
      "displayName",
      "bundleIdentifier",
      "desktopSha256",
      "cliSha256",
      "inventory",
      "filesystem",
      "bootstrapResult",
    ],
    "approved identity preflight",
  );
  exactObject(
    approvedIdentity.inventory,
    ["sha256", "entries"],
    "approved identity preflight inventory",
  );
  if (
    preflight?.schemaVersion !== 1 ||
    approvedIdentity.record !== approvedRecordFile.path ||
    approvedIdentity.recordSha256 !== sha256(approvedRecordFile.bytes) ||
    approvedIdentity.recordSha256 !== expectedApprovalRecordSha256 ||
    approvedIdentity.contract !== identityContract ||
    approvedIdentity.root !== approvedRoot ||
    approvedIdentity.app !== approvedApp ||
    approvedIdentity.displayName !== approvedRecord.displayName ||
    approvedIdentity.bundleIdentifier !== approvedRecord.bundleIdentifier ||
    approvedIdentity.desktopSha256 !== approvedRecord.desktopSha256 ||
    approvedIdentity.cliSha256 !== approvedRecord.cliSha256 ||
    approvedIdentity.inventory.sha256 !== approvedRecord.inventory?.digest ||
    approvedIdentity.inventory.entries !==
      approvedRecord.inventory?.entries?.length ||
    approvedIdentity.filesystem !==
      "record is an owned mode-0600 regular file; root is an owned mode-0700 canonical directory" ||
    approvedIdentity.bootstrapResult !==
      "passed before this documentation turn; the original application bundle remained intact and no application was launched by bootstrap"
  )
    throw new Error("approved identity preflight does not match record");
  return { preflight, sha256: sha256(preflightBytes) };
}

export async function inventory(root) {
  const entries = [];
  async function walk(path) {
    const info = await lstat(path);
    if (process.getuid?.() !== undefined && info.uid !== process.getuid())
      throw new Error(`application inventory entry is not owned: ${path}`);
    const entry = {
      path: path === root ? "." : relative(root, path),
      mode: info.mode & 0o7777,
      uid: info.uid,
      gid: info.gid,
    };
    if (info.isDirectory()) {
      entry.type = "directory";
      entries.push(entry);
      for (const name of (await readdir(path)).sort())
        await walk(join(path, name));
    } else if (info.isFile()) {
      entry.type = "file";
      entry.bytes = info.size;
      entry.links = info.nlink;
      entry.sha256 = sha256(await readFile(path));
      entries.push(entry);
    } else if (info.isSymbolicLink()) {
      entry.type = "symlink";
      entry.target = await readlink(path);
      entries.push(entry);
    } else {
      throw new Error(`application inventory has unsupported entry: ${path}`);
    }
  }
  await walk(root);
  return {
    digest: createHash("sha256").update(JSON.stringify(entries)).digest("hex"),
    entries,
  };
}

async function validateRuntimeHelperSource({
  sourceRoot,
  manifest,
  runtimeHelperPath,
}) {
  if (!runtimeHelperPath) return;
  const path = await realpath(runtimeHelperPath);
  const sourcePath = "scripts/seal-native-reader-thirteen.mjs";
  if (path !== join(sourceRoot, sourcePath))
    throw new Error(
      "native Reader sealer is outside the frozen source checkout",
    );
  const entry = manifest.files.find((item) => item.path === sourcePath);
  if (!entry || sha256(await readFile(path)) !== entry.sha256)
    throw new Error("native Reader sealer differs from frozen source");
}

export function validatePng(bytes) {
  const signature = Buffer.from([
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
  ]);
  if (bytes.length < 45 || !bytes.subarray(0, 8).equals(signature))
    throw new Error("screenshot is not a PNG");
  let offset = 8;
  let dimensions;
  while (offset + 12 <= bytes.length) {
    const length = bytes.readUInt32BE(offset);
    const type = bytes.subarray(offset + 4, offset + 8).toString("ascii");
    const next = offset + 12 + length;
    if (
      length > 16 * 1024 * 1024 ||
      !/^[A-Za-z]{4}$/.test(type) ||
      next > bytes.length
    )
      throw new Error("screenshot PNG structure is invalid");
    if (type === "IHDR") {
      if (offset !== 8 || length !== 13 || dimensions)
        throw new Error("screenshot PNG header is invalid");
      const width = bytes.readUInt32BE(offset + 8);
      const height = bytes.readUInt32BE(offset + 12);
      if (
        width === 0 ||
        height === 0 ||
        width > 32_768 ||
        height > 32_768 ||
        width * height > 100_000_000
      )
        throw new Error("screenshot PNG dimensions are unsafe");
      dimensions = { width, height };
    }
    if (type === "IEND") {
      if (!dimensions || length !== 0 || next !== bytes.length)
        throw new Error("screenshot PNG termination is invalid");
      return dimensions;
    }
    offset = next;
  }
  throw new Error("screenshot PNG is missing IEND");
}

export async function verifyInputs({
  root: rootArg,
  sourceRoot: sourceRootArg,
  sourceManifestPath,
  sourceManifestSha256,
  observationPath,
  expectedApprovalRecordSha256,
  runtimeHelperPath,
}) {
  const root = await requireRunRoot(rootArg);
  const sourceRoot = await requireSourceRoot(sourceRootArg);
  if (!/^[a-f0-9]{64}$/.test(sourceManifestSha256))
    throw new Error("source manifest digest argument is invalid");
  if (!/^[a-f0-9]{64}$/.test(expectedApprovalRecordSha256))
    throw new Error("expected approved identity record digest is invalid");
  const manifestFile = await requireOwnedFile(
    sourceManifestPath,
    "source manifest",
    { modes: [0o600, 0o644] },
  );
  if (sha256(manifestFile.bytes) !== sourceManifestSha256)
    throw new Error("source manifest digest changed");
  const manifest = parseJson(manifestFile.bytes, "source manifest");
  if (
    Object.keys(manifest).sort().join(",") !==
      "algorithm,digest,fileCount,files" ||
    manifest.algorithm !== "sha256-path-and-content-manifest" ||
    !/^[a-f0-9]{64}$/.test(manifest.digest) ||
    !Number.isInteger(manifest.fileCount) ||
    manifest.files?.length !== manifest.fileCount
  )
    throw new Error("source manifest contract is invalid");
  await validateLiveSourceManifest(manifest, sourceRoot);
  await validateRuntimeHelperSource({
    sourceRoot,
    manifest,
    runtimeHelperPath,
  });

  const exitFile = await requireOwnedFile(
    join(root, "app-exit-status.txt"),
    "app exit status",
    { within: root },
  );
  if (exitFile.bytes.toString("utf8") !== "0\n")
    throw new Error("native app did not exit cleanly");
  const sourceBeforeFile = await requireOwnedFile(
    join(root, "source-before-build.json"),
    "source before build",
    { within: root },
  );
  const sourceAfterFile = await requireOwnedFile(
    join(root, "source-before-launch-services-registration.json"),
    "source before launch services registration",
    { within: root },
  );
  const sourceBefore = parseJson(sourceBeforeFile.bytes, "source before build");
  const sourceAfter = parseJson(
    sourceAfterFile.bytes,
    "source before launch services registration",
  );
  for (const [value, label] of [
    [sourceBefore, "source before build"],
    [sourceAfter?.current, "source before launch services registration"],
  ]) {
    if (
      value?.algorithm !== manifest.algorithm ||
      value?.digest !== manifest.digest ||
      value?.fileCount !== manifest.fileCount
    )
      throw new Error(`${label} does not match the frozen source`);
  }
  if (sourceAfter.status !== "passed")
    throw new Error("source before launch services registration did not pass");

  const launchFile = await requireOwnedFile(
    join(root, "sleep-wake-fixture.json"),
    "launch metadata",
    { within: root },
  );
  const launch = parseJson(launchFile.bytes, "launch metadata");
  if (
    !Number.isInteger(launch.pid) ||
    launch.pid < 1 ||
    typeof launch.appExecutable !== "string" ||
    !/^[a-f0-9]{64}$/.test(launch.appSha256) ||
    launch.approvedIdentityReuse !== true ||
    launch.packaged !== true ||
    typeof launch.approvedIdentityRecord !== "string" ||
    launch.expectedMainWebviewUrl !== "tauri://localhost/" ||
    typeof launch.cli !== "string" ||
    !/^[a-f0-9]{64}$/.test(launch.cliSha256)
  )
    throw new Error("launch metadata identity is invalid");

  const handoffFile = await requireOwnedFile(
    join(root, "post-exit-acceptance-handoff.json"),
    "post-exit acceptance handoff",
    { within: root },
  );
  const handoff = parseJson(handoffFile.bytes, "post-exit acceptance handoff");
  exactObject(
    handoff,
    [
      "schemaVersion",
      "status",
      "runRoot",
      "app",
      "record",
      "journal",
      "desktopSha256",
      "cliSha256",
      "release",
      "expectedReleaseContent",
    ],
    "post-exit acceptance handoff",
  );
  const approvedRoot = dirname(handoff.app);
  if (
    handoff.schemaVersion !== 1 ||
    handoff.status !== "waiting-for-post-exit-acceptance" ||
    handoff.runRoot !== root ||
    basename(handoff.app) !== "Dakia Fictional Provider Reader.app" ||
    handoff.record !== join(approvedRoot, "approved-native-identity.json") ||
    handoff.journal !==
      join(approvedRoot, ".approved-native-identity-swap.json") ||
    handoff.release !==
      join(root, ".native-provider-reader-post-exit-complete") ||
    handoff.expectedReleaseContent !==
      "dakia-native-provider-reader-post-exit-complete-v1\n" ||
    launch.approvedIdentityRecord !== handoff.record ||
    launch.appExecutable !==
      join(handoff.app, "Contents/MacOS/dakia-desktop") ||
    launch.cli !== join(handoff.app, "Contents/MacOS/dakia") ||
    launch.appSha256 !== handoff.desktopSha256 ||
    launch.cliSha256 !== handoff.cliSha256
  )
    throw new Error("held approved candidate identity is invalid");
  try {
    await lstat(handoff.release);
    throw new Error("post-exit release was already accepted");
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
  }
  await requireOwnedDirectory(approvedRoot, "approved identity root");
  await requireOwnedDirectory(handoff.app, "held approved application");
  const liveInventory = await inventory(handoff.app);
  const approvedMarker = await requireOwnedFile(
    join(approvedRoot, ".dakia-native-mail-safety-root"),
    "approved identity marker",
    { within: approvedRoot },
  );
  if (
    approvedMarker.bytes.toString("utf8") !==
    "dakia-native-mail-safety-fixture-v1\n"
  )
    throw new Error("approved identity marker is invalid");
  const approvedRecordFile = await requireOwnedFile(
    handoff.record,
    "approved identity record",
    { within: approvedRoot },
  );
  const approvedJournalFile = await requireOwnedFile(
    handoff.journal,
    "approved identity transaction journal",
    { within: approvedRoot },
  );
  const approvedRecord = parseJson(
    approvedRecordFile.bytes,
    "approved identity record",
  );
  const approvedJournal = parseJson(
    approvedJournalFile.bytes,
    "approved identity transaction journal",
  );
  exactObject(
    approvedRecord,
    [
      "schemaVersion",
      "contract",
      "sealedAt",
      "approvedRoot",
      "approvedApp",
      "displayName",
      "bundleIdentifier",
      "desktopSha256",
      "cliSha256",
      "inventory",
    ],
    "approved identity record",
  );
  exactObject(
    approvedRecord.inventory,
    ["digest", "entries"],
    "approved identity inventory",
  );
  exactObject(
    approvedJournal,
    [
      "schemaVersion",
      "contract",
      "transactionId",
      "stage",
      "createdAt",
      "approvedRoot",
      "approvedApp",
      "runRoot",
      "candidateApp",
      "backupApp",
      "testedCandidateApp",
      "originalInventoryDigest",
      "candidateInventoryDigest",
      "candidateDesktopSha256",
      "candidateCliSha256",
    ],
    "approved identity transaction journal",
  );
  if (
    approvedRecord.schemaVersion !== 1 ||
    approvedRecord.contract !== identityContract ||
    typeof approvedRecord.sealedAt !== "string" ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(
      approvedRecord.sealedAt,
    ) ||
    Number.isNaN(Date.parse(approvedRecord.sealedAt)) ||
    approvedRecord.approvedRoot !== approvedRoot ||
    approvedRecord.approvedApp !== handoff.app ||
    approvedRecord.displayName !== "Dakia Fictional Provider Reader" ||
    typeof approvedRecord.bundleIdentifier !== "string" ||
    !/^dev\.dakia\.fictional-provider-reader\.run-[A-Za-z0-9]+$/.test(
      approvedRecord.bundleIdentifier,
    ) ||
    !/^[a-f0-9]{64}$/.test(approvedRecord.desktopSha256) ||
    !/^[a-f0-9]{64}$/.test(approvedRecord.cliSha256) ||
    !/^[a-f0-9]{64}$/.test(approvedRecord.inventory?.digest) ||
    !Array.isArray(approvedRecord.inventory.entries) ||
    approvedJournal.schemaVersion !== 1 ||
    approvedJournal.contract !== identityContract ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      approvedJournal.transactionId,
    ) ||
    approvedJournal.stage !== "candidate-installed" ||
    approvedJournal.approvedRoot !== approvedRoot ||
    approvedJournal.approvedApp !== handoff.app ||
    approvedJournal.runRoot !== root ||
    approvedJournal.candidateApp !==
      join(root, "Dakia Fictional Provider Reader.app") ||
    approvedJournal.backupApp !== join(root, "sealed-approved-original.app") ||
    approvedJournal.testedCandidateApp !==
      join(root, "tested-approved-candidate.app") ||
    approvedJournal.originalInventoryDigest !==
      approvedRecord.inventory.digest ||
    approvedJournal.candidateDesktopSha256 !== handoff.desktopSha256 ||
    approvedJournal.candidateCliSha256 !== handoff.cliSha256 ||
    approvedJournal.candidateInventoryDigest !== liveInventory.digest
  )
    throw new Error("approved identity transaction is invalid");
  const approvalPreflightFile = await requireOwnedFile(
    join(sourceRoot, approvalPreflightRelativePath),
    "approved identity preflight",
    { within: sourceRoot, modes: [0o600, 0o644] },
  );
  const approvalPreflight = validateApprovedIdentityPreflight({
    preflight: parseJson(
      approvalPreflightFile.bytes,
      "approved identity preflight",
    ),
    preflightBytes: approvalPreflightFile.bytes,
    approvedRecord,
    approvedRecordFile,
    approvedRoot,
    approvedApp: handoff.app,
    expectedApprovalRecordSha256,
  });
  let sealedOriginal;
  try {
    sealedOriginal = await requireOwnedDirectory(
      approvedJournal.backupApp,
      "retained sealed original application",
      { within: root },
    );
  } catch {
    throw new Error("retained sealed original is unavailable");
  }
  const sealedOriginalInventory = await inventory(sealedOriginal);
  if (
    sealedOriginalInventory.digest !== approvedRecord.inventory.digest ||
    JSON.stringify(sealedOriginalInventory.entries) !==
      JSON.stringify(approvedRecord.inventory.entries)
  )
    throw new Error("retained sealed original differs from approved inventory");
  for (const path of [
    approvedJournal.candidateApp,
    approvedJournal.testedCandidateApp,
  ]) {
    try {
      await lstat(path);
      throw new Error("candidate-installed transaction topology is invalid");
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
  }
  const heldDesktop = await requireOwnedExecutable(
    launch.appExecutable,
    "held desktop executable",
  );
  const heldCli = await requireOwnedExecutable(
    launch.cli,
    "held CLI executable",
  );
  if (
    sha256(heldDesktop.bytes) !== handoff.desktopSha256 ||
    sha256(heldCli.bytes) !== handoff.cliSha256
  )
    throw new Error("held approved candidate executable changed");

  const overlayFile = await requireOwnedFile(
    join(root, "native-reader-thirteen-overlay.json"),
    "overlay metadata",
    { within: root },
  );
  const overlay = parseJson(overlayFile.bytes, "overlay metadata");
  const ownedInputsFile = await requireOwnedFile(
    join(root, "native-reader-thirteen-owned-inputs.json"),
    "owned input receipt",
    { within: root },
  );
  const ownedInputs = parseJson(ownedInputsFile.bytes, "owned input receipt");
  exactObject(
    ownedInputs,
    ["schemaVersion", "overlay", "seeder"],
    "owned input receipt",
  );
  if (ownedInputs.schemaVersion !== 1)
    throw new Error("owned input receipt version is invalid");
  const ownedInputCopies = [];
  for (const [label, expectedName, sourcePath] of ownedRuntimeHelpers) {
    exactObject(
      ownedInputs[label],
      ["path", "sourcePath", "sha256"],
      `owned ${label} receipt`,
    );
    if (
      ownedInputs[label].path !== join(root, expectedName) ||
      ownedInputs[label].sourcePath !== sourcePath ||
      !/^[a-f0-9]{64}$/.test(ownedInputs[label].sha256)
    )
      throw new Error(`owned ${label} receipt identity is invalid`);
    const sourceEntry = manifest.files.find((item) => item.path === sourcePath);
    if (sourceEntry?.sha256 !== ownedInputs[label].sha256)
      throw new Error(`owned ${label} copy does not match frozen source`);
    const file = await requireOwnedFile(
      ownedInputs[label].path,
      `owned ${label} copy`,
      { within: root },
    );
    if (sha256(file.bytes) !== ownedInputs[label].sha256)
      throw new Error(`owned ${label} copy changed`);
    ownedInputCopies.push({ label: `owned-${label}`, ...file });
  }
  if (
    overlay.schemaVersion !== 1 ||
    overlay.mode !== "native-reader-thirteen-inner-tls" ||
    JSON.stringify(overlay.fixtures?.map(({ uid }) => uid)) !==
      JSON.stringify(expectedUids) ||
    JSON.stringify(overlay.fixtures?.map(({ id }) => id)) !==
      JSON.stringify(expectedIds)
  )
    throw new Error("overlay metadata fixture identity is invalid");
  for (const item of overlay.fixtures) {
    const expected = uidToFixture.get(item.uid);
    if (item.sha256 !== expected.digest)
      throw new Error(
        `overlay metadata fixture digest changed for UID ${item.uid}`,
      );
  }

  const transcriptFile = await requireOwnedFile(
    join(root, "native-reader-thirteen-imap-transcript.jsonl"),
    "IMAP transcript",
    { within: root },
  );
  const records = transcriptFile.bytes
    .toString("utf8")
    .trimEnd()
    .split("\n")
    .filter(Boolean)
    .map((line, index) => {
      try {
        return JSON.parse(line);
      } catch {
        throw new Error(`IMAP transcript line ${index + 1} is invalid`);
      }
    });
  for (const uid of expectedUids) {
    const expected = uidToFixture.get(uid);
    const actions = records.filter(
      (record) =>
        record.uid === uid &&
        record.fixtureId === expected.id &&
        record.fixtureSha256 === expected.digest &&
        record.fixtureAccount === true &&
        record.mailbox === "INBOX",
    );
    for (const action of ["metadata", "mime", "raw"]) {
      if (!actions.some((record) => record.action === action))
        throw new Error(`UID ${uid} lacks bound production ${action} evidence`);
    }
    const firstMetadata = actions.findIndex(
      ({ action }) => action === "metadata",
    );
    const firstMime = actions.findIndex(({ action }) => action === "mime");
    const firstRaw = actions.findIndex(({ action }) => action === "raw");
    if (!(
      firstMetadata >= 0 &&
      firstMime > firstMetadata &&
      firstRaw > firstMime
    ))
      throw new Error(
        `UID ${uid} lacks an ordered metadata to MIME to raw fallback sequence`,
      );
  }

  const observationFile = await requireOwnedFile(
    observationPath,
    "UI observation",
    { within: root },
  );
  if (
    observationFile.path !==
    join(root, "native-reader-thirteen-ui-observation.json")
  )
    throw new Error("UI observation path is not canonical for this run");
  const observation = parseJson(observationFile.bytes, "UI observation");
  exactObject(
    observation,
    [
      "schemaVersion",
      "status",
      "sequence",
      "identity",
      "noAttachmentSaveInvoked",
      "fixtures",
      "oracles",
    ],
    "UI observation",
  );
  if (
    observation.schemaVersion !== 1 ||
    observation.status !== "observed" ||
    observation.noAttachmentSaveInvoked !== true ||
    JSON.stringify(observation.sequence) !== JSON.stringify(expectedUids)
  )
    throw new Error("UI observation top-level contract is invalid");
  exactObject(
    observation.identity,
    ["observedProcessPath", "observedWebviewUrl", "observedExecutableSha256"],
    "UI identity observation",
  );
  if (
    observation.identity.observedProcessPath !== launch.appExecutable ||
    observation.identity.observedWebviewUrl !== launch.expectedMainWebviewUrl ||
    observation.identity.observedExecutableSha256 !== launch.appSha256
  )
    throw new Error("UI identity observation does not match launch metadata");
  if (
    !Array.isArray(observation.fixtures) ||
    observation.fixtures.length !== 13
  )
    throw new Error("UI fixture observations are incomplete");

  const evidenceFiles = [];
  for (let index = 0; index < observation.fixtures.length; index += 1) {
    const result = observation.fixtures[index];
    const [uid, id] = FIXTURES[index];
    const [expectedSubject, expectedText] = expectedVisible.get(uid);
    exactObject(
      result,
      [
        "uid",
        "fixtureId",
        "visibleSubject",
        "requiredText",
        "requiredTextVisible",
        "decodeErrorVisible",
        "retryVisible",
        "ax",
        "screenshot",
      ],
      `UI fixture ${uid}`,
    );
    if (
      result.uid !== uid ||
      result.fixtureId !== id ||
      result.visibleSubject !== expectedSubject ||
      result.requiredText !== expectedText ||
      result.requiredTextVisible !== true ||
      result.decodeErrorVisible !== false ||
      result.retryVisible !== false
    )
      throw new Error(`UI fixture ${uid} semantic observation is invalid`);
    for (const [label, evidence] of [
      ["AX", result.ax],
      ["screenshot", result.screenshot],
    ]) {
      exactObject(evidence, ["path", "sha256"], `UI fixture ${uid} ${label}`);
      if (!/^[a-f0-9]{64}$/.test(evidence.sha256))
        throw new Error(`UI fixture ${uid} ${label} digest is invalid`);
      const file = await requireOwnedFile(
        evidence.path,
        `UI fixture ${uid} ${label}`,
        { within: root },
      );
      if (
        !file.path.startsWith(`${join(root, "native-ui-evidence")}${sep}`) ||
        sha256(file.bytes) !== evidence.sha256
      )
        throw new Error(`UI fixture ${uid} ${label} evidence changed`);
      evidenceFiles.push({
        label: `uid-${uid}-${label.toLowerCase()}`,
        ...file,
      });
      if (label === "screenshot") {
        if (!file.path.endsWith(".png"))
          throw new Error(`UI fixture ${uid} screenshot filename is invalid`);
        validatePng(file.bytes);
      }
      if (label === "AX") {
        const ax = file.bytes.toString("utf8");
        for (const required of [expectedSubject, expectedText]) {
          if (!ax.includes(required))
            throw new Error(
              `UI fixture ${uid} AX evidence lacks ${JSON.stringify(required)}`,
            );
        }
        if (
          ax.includes("The contents of this message cannot be decoded.") ||
          /\bRetry\b/.test(ax)
        )
          throw new Error(
            `UI fixture ${uid} AX evidence contains an error control`,
          );
        const warning =
          "This file may run software. Only open it if you trust the sender.";
        if (uid === 13) {
          for (const required of [
            "claim-documents.pdf",
            "Save claim-documents.pdf to Downloads",
          ])
            if (!ax.includes(required))
              throw new Error(`provider fixture AX lacks ${required}`);
          if (ax.includes("Save image001.png to Downloads"))
            throw new Error("provider inline image appeared as an attachment");
        } else if (uid === 14) {
          for (const filename of [
            "quarterly report final.pdf",
            "report-test.txt",
            "Prügimaja plaan vaated 31.08.pdf",
            "Prügimaja_31.08.pdf",
            "Prügimaja.pdf",
            "invoice;final.pdf",
            "invoicegpj.exe",
            "broken%ZZname.pdf",
          ]) {
            if (
              !ax.includes(filename) ||
              !ax.includes(`Save ${filename} to Downloads`)
            )
              throw new Error(
                `filename fixture AX lacks controls for ${filename}`,
              );
          }
          if (ax.split(warning).length - 1 !== 1)
            throw new Error("filename fixture AX warning count is not one");
        } else if (uid === 15) {
          if (/Save .* to Downloads/.test(ax))
            throw new Error(
              "LinkedIn inline fixture exposed an attachment control",
            );
        } else if (uid === 16) {
          for (const required of [
            "Save invoice.exe to Downloads",
            "Save attachment to Downloads",
          ])
            if (!ax.includes(required))
              throw new Error(`unknown encoding fixture AX lacks ${required}`);
          if (ax.split(warning).length - 1 !== 1)
            throw new Error(
              "unknown encoding fixture AX warning count is not one",
            );
        }
      }
    }
  }

  const expectedOracles = {
    providerSignatureInline: {
      inlineImgElementPresent: true,
      cidRewrittenToFixtureDataUrl: true,
      decodedPixelRenderingClaimed: false,
      attachmentFilenames: ["claim-documents.pdf"],
      saveControlCount: 1,
      image001AttachmentRowAbsent: true,
    },
    filenameParametersAndEncodings: {
      visibleFilenames: [
        "quarterly report final.pdf",
        "report-test.txt",
        "Prügimaja plaan vaated 31.08.pdf",
        "Prügimaja_31.08.pdf",
        "Prügimaja.pdf",
        "invoice;final.pdf",
        "invoicegpj.exe",
        "broken%ZZname.pdf",
      ],
      saveControlCount: 8,
      warningVisibleOnlyInRow: "invoicegpj.exe",
    },
    linkedinInlineContentId: {
      htmlBodyVisible: true,
      attachmentRowCount: 0,
      saveControlCount: 0,
    },
    standardsUnknownTransferEncoding: {
      safeBodyVisible: true,
      htmlDocumentSurfaceAbsent: true,
      saveControlNames: [
        "Save invoice.exe to Downloads",
        "Save attachment to Downloads",
      ],
      warningVisibleInRow: "invoice.exe",
      warningAbsentFromRow: "attachment",
    },
  };
  if (JSON.stringify(observation.oracles) !== JSON.stringify(expectedOracles))
    throw new Error(
      "UI security oracle observations are incomplete or changed",
    );

  return {
    root,
    manifestFile,
    manifest,
    exitFile,
    sourceBeforeFile,
    sourceAfterFile,
    launchFile,
    launch,
    handoffFile,
    handoff,
    approvedRecordFile,
    approvedRecord,
    approvedJournalFile,
    approvedJournal,
    approvalPreflightFile,
    approvalPreflight,
    sealedOriginal,
    sealedOriginalInventory,
    heldDesktop,
    heldCli,
    liveInventory,
    overlayFile,
    ownedInputsFile,
    ownedInputCopies,
    transcriptFile,
    observationFile,
    evidenceFiles,
    transcriptRecordCount: records.length,
  };
}

async function copyExclusive(source, destination) {
  await copyFile(source, destination, constants.COPYFILE_EXCL);
  await chmod(destination, 0o600);
  return {
    path: basename(destination),
    sha256: sha256(await readFile(destination)),
  };
}

async function main() {
  const args = new Map();
  for (let index = 2; index < process.argv.length; index += 2)
    args.set(process.argv[index], process.argv[index + 1]);
  for (const key of [
    "--root",
    "--source-root",
    "--source-manifest",
    "--source-manifest-sha256",
    "--observation",
    "--expected-approval-record-sha256",
  ])
    if (!args.get(key)) throw new Error(`missing ${key}`);
  const verified = await verifyInputs({
    root: args.get("--root"),
    sourceRoot: args.get("--source-root"),
    sourceManifestPath: args.get("--source-manifest"),
    sourceManifestSha256: args.get("--source-manifest-sha256"),
    observationPath: args.get("--observation"),
    expectedApprovalRecordSha256: args.get("--expected-approval-record-sha256"),
    runtimeHelperPath: new URL(import.meta.url).pathname,
  });
  const snapshot = join(verified.root, "native-reader-thirteen-phase-snapshot");
  await mkdir(snapshot, { mode: 0o700 });
  await chmod(snapshot, 0o700);
  const inputs = [
    [verified.manifestFile.path, "source-manifest.json"],
    [verified.exitFile.path, "app-exit-status.txt"],
    [verified.sourceBeforeFile.path, "source-before-build.json"],
    [
      verified.sourceAfterFile.path,
      "source-before-launch-services-registration.json",
    ],
    [verified.launchFile.path, "sleep-wake-fixture.json"],
    [verified.handoffFile.path, "post-exit-acceptance-handoff.json"],
    [verified.approvedRecordFile.path, "approved-native-identity.json"],
    [verified.approvedJournalFile.path, ".approved-native-identity-swap.json"],
    [
      verified.approvalPreflightFile.path,
      "native-packaged-same-identity-preflight-2026-10-21.json",
    ],
    [verified.overlayFile.path, "native-reader-thirteen-overlay.json"],
    [verified.ownedInputsFile.path, "native-reader-thirteen-owned-inputs.json"],
    ...verified.ownedInputCopies.map((file) => [
      file.path,
      basename(file.path),
    ]),
    [
      verified.transcriptFile.path,
      "native-reader-thirteen-imap-transcript.jsonl",
    ],
    [
      verified.observationFile.path,
      "native-reader-thirteen-ui-observation.json",
    ],
    ...verified.evidenceFiles.map((file) => [
      file.path,
      `${file.label}-${basename(file.path)}`,
    ]),
  ];
  const artifacts = [];
  for (const [source, name] of inputs)
    artifacts.push(await copyExclusive(source, join(snapshot, name)));
  const report = {
    schemaVersion: 1,
    kind: "dakia-native-reader-thirteen-fixture-snapshot",
    status: "passed",
    snapshotId: randomUUID(),
    source: {
      algorithm: verified.manifest.algorithm,
      digest: verified.manifest.digest,
      fileCount: verified.manifest.fileCount,
      manifestSha256: sha256(verified.manifestFile.bytes),
    },
    app: {
      executable: verified.launch.appExecutable,
      sha256: verified.launch.appSha256,
      cli: verified.launch.cli,
      cliSha256: verified.launch.cliSha256,
      expectedMainWebviewUrl: verified.launch.expectedMainWebviewUrl,
      heldCandidateVerifiedBeforeRelease: true,
    },
    approvedIdentity: {
      preflightSha256: verified.approvalPreflight.sha256,
      originalRecordSha256: sha256(verified.approvedRecordFile.bytes),
      candidateTransactionId: verified.approvedJournal.transactionId,
      candidateInventoryDigest: verified.liveInventory.digest,
    },
    fixtureUids: expectedUids,
    fixtureCount: 13,
    transcriptRecordCount: verified.transcriptRecordCount,
    artifacts,
    limitations: [
      "The provider-signature fixture proves CID rewrite and inline element placement, not decoded PNG pixels.",
      "Attachment save controls were observed but were not activated in this rendering run.",
      "Inline-image placement and warning-row placement that AX cannot structurally distinguish remain manual screenshot judgments.",
    ],
  };
  const reportPath = join(snapshot, "native-reader-thirteen-acceptance.json");
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
  console.log(
    JSON.stringify({
      snapshot,
      report: reportPath,
      reportSha256: sha256(await readFile(reportPath)),
    }),
  );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
)
  main().catch((error) => {
    console.error(error.stack || error);
    process.exit(1);
  });
