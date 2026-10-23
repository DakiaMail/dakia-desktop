#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { FIXTURES } from "./native-reader-thirteen-overlay.mjs";
import { inventory, verifyInputs } from "./seal-native-reader-thirteen.mjs";

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const tinyPng = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL/7wAAAABJRU5ErkJggg==",
  "base64",
);

function sourceManifest(files) {
  const digest = createHash("sha256");
  for (const file of files)
    digest
      .update(file.path)
      .update("\0")
      .update(Buffer.from(file.sha256, "hex"))
      .update("\0");
  return {
    algorithm: "sha256-path-and-content-manifest",
    digest: digest.digest("hex"),
    fileCount: files.length,
    files,
  };
}
const visible = [
  ["Synthetic nested alternatives", "Visible HTML body."],
  [
    "Synthetic attached message",
    "The attached message is synthetic and fully redacted.",
  ],
  ["Synthetic multipart attachment", "Visible parent body."],
  ["Synthetic empty MIME leaf", "Visible HTML after an empty plain part."],
  [
    "Synthetic truncated MIME",
    "The second leaf and closing boundary are deliberately missing.",
  ],
  ["Synthetic charset matrix", "Zażółć"],
  ["Synthetic flowed text", "This is the HTML alternative."],
  ["Fictional 🌍 reader message", "مرحبا بالعالم"],
  ["Synthetic ambiguous inline identities", "Fictional resources"],
  [
    "CASE-12345, Example Street 1",
    "Redacted message content for a fictional insurance case.",
  ],
  ["Synthetic filename parameters", "Synthetic filename coverage."],
  [
    "Redacted LinkedIn notification",
    "Redacted HTML=content with a foldedline.",
  ],
  ["Synthetic unknown transfer encoding", "Visible safe body."],
];
const oracles = {
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

async function ownedWrite(path, value) {
  await writeFile(path, value, { flag: "wx", mode: 0o600 });
  await chmod(path, 0o600);
}

async function executableWrite(path, value) {
  await writeFile(path, value, { flag: "wx", mode: 0o755 });
  await chmod(path, 0o755);
}

async function fixtureRoot(mutate = async () => {}) {
  const root = await mkdtemp(
    join(await realpath(tmpdir()), "dakia-native-provider-reader."),
  );
  await chmod(root, 0o700);
  await ownedWrite(
    join(root, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
  );
  const sourceRoot = join(root, "source-root");
  await mkdir(sourceRoot, { mode: 0o700 });
  const sourceBytes = Buffer.from("x");
  await ownedWrite(join(sourceRoot, "synthetic"), sourceBytes);
  const overlayBytes = await readFile(
    new URL("./native-reader-thirteen-overlay.mjs", import.meta.url),
  );
  const seederBytes = await readFile(
    new URL("./seed-native-reader-thirteen-fixtures.mjs", import.meta.url),
  );
  const sealerBytes = await readFile(
    new URL("./seal-native-reader-thirteen.mjs", import.meta.url),
  );
  await mkdir(join(sourceRoot, "scripts"), { mode: 0o700 });
  await ownedWrite(
    join(sourceRoot, "scripts/native-reader-thirteen-overlay.mjs"),
    overlayBytes,
  );
  await ownedWrite(
    join(sourceRoot, "scripts/seed-native-reader-thirteen-fixtures.mjs"),
    seederBytes,
  );
  await ownedWrite(
    join(sourceRoot, "scripts/seal-native-reader-thirteen.mjs"),
    sealerBytes,
  );
  await ownedWrite(join(root, "app-exit-status.txt"), "0\n");
  const approvedRoot = join(root, "approved-identity");
  const app = join(approvedRoot, "Dakia Fictional Provider Reader.app");
  const macos = join(app, "Contents", "MacOS");
  await mkdir(macos, { recursive: true, mode: 0o700 });
  await chmod(approvedRoot, 0o700);
  await ownedWrite(
    join(approvedRoot, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
  );
  const appExecutable = join(macos, "dakia-desktop");
  const cli = join(macos, "dakia");
  await executableWrite(appExecutable, "fictional desktop executable\n");
  await executableWrite(cli, "fictional cli executable\n");
  const appSha256 = sha256(await readFile(appExecutable));
  const cliSha256 = sha256(await readFile(cli));
  const appInventory = await inventory(app);
  const backupApp = join(root, "sealed-approved-original.app");
  const backupMacos = join(backupApp, "Contents", "MacOS");
  await mkdir(backupMacos, { recursive: true, mode: 0o700 });
  await executableWrite(
    join(backupMacos, "dakia-desktop"),
    "sealed original desktop executable\n",
  );
  await executableWrite(join(backupMacos, "dakia"), "sealed original cli\n");
  const originalInventory = await inventory(backupApp);
  const record = join(approvedRoot, "approved-native-identity.json");
  const journal = join(approvedRoot, ".approved-native-identity-swap.json");
  const originalRecord = {
    schemaVersion: 1,
    contract: "dakia-native-provider-reader-approved-identity-v1",
    sealedAt: "2026-10-21T00:00:00.000Z",
    approvedRoot,
    approvedApp: app,
    displayName: "Dakia Fictional Provider Reader",
    bundleIdentifier: "dev.dakia.fictional-provider-reader.run-test",
    desktopSha256: sha256(await readFile(join(backupMacos, "dakia-desktop"))),
    cliSha256: sha256(await readFile(join(backupMacos, "dakia"))),
    inventory: originalInventory,
  };
  await ownedWrite(record, `${JSON.stringify(originalRecord)}\n`);
  const approvalRecordSha256 = sha256(await readFile(record));
  await ownedWrite(
    journal,
    `${JSON.stringify({ schemaVersion: 1, contract: "dakia-native-provider-reader-approved-identity-v1", transactionId: "123e4567-e89b-42d3-a456-426614174000", stage: "candidate-installed", createdAt: "2026-10-22T00:00:00.000Z", approvedRoot, approvedApp: app, runRoot: root, candidateApp: join(root, "Dakia Fictional Provider Reader.app"), backupApp, testedCandidateApp: join(root, "tested-approved-candidate.app"), originalInventoryDigest: originalInventory.digest, candidateInventoryDigest: appInventory.digest, candidateDesktopSha256: appSha256, candidateCliSha256: cliSha256 })}\n`,
  );
  const approvalPreflightPath = join(
    sourceRoot,
    "docs/mail-safety-results/native-packaged-same-identity-preflight-2026-10-21.json",
  );
  await mkdir(join(sourceRoot, "docs/mail-safety-results"), {
    recursive: true,
    mode: 0o700,
  });
  await ownedWrite(
    approvalPreflightPath,
    `${JSON.stringify({ schemaVersion: 1, approvedIdentity: { record, recordSha256: approvalRecordSha256, contract: originalRecord.contract, root: approvedRoot, app, displayName: originalRecord.displayName, bundleIdentifier: originalRecord.bundleIdentifier, desktopSha256: originalRecord.desktopSha256, cliSha256: originalRecord.cliSha256, inventory: { sha256: originalInventory.digest, entries: originalInventory.entries.length }, filesystem: "record is an owned mode-0600 regular file; root is an owned mode-0700 canonical directory", bootstrapResult: "passed before this documentation turn; the original application bundle remained intact and no application was launched by bootstrap" } })}\n`,
  );
  const manifest = sourceManifest(
    [
      {
        path: "scripts/native-reader-thirteen-overlay.mjs",
        state: "present",
        bytes: overlayBytes.length,
        sha256: sha256(overlayBytes),
      },
      {
        path: "scripts/seed-native-reader-thirteen-fixtures.mjs",
        state: "present",
        bytes: seederBytes.length,
        sha256: sha256(seederBytes),
      },
      {
        path: "scripts/seal-native-reader-thirteen.mjs",
        state: "present",
        bytes: sealerBytes.length,
        sha256: sha256(sealerBytes),
      },
      {
        path: "synthetic",
        state: "present",
        bytes: sourceBytes.length,
        sha256: sha256(sourceBytes),
      },
    ].sort((left, right) => left.path.localeCompare(right.path)),
  );
  const manifestPath = join(root, "frozen-source-manifest.json");
  await ownedWrite(manifestPath, `${JSON.stringify(manifest)}\n`);
  const snapshot = {
    algorithm: manifest.algorithm,
    digest: manifest.digest,
    fileCount: manifest.fileCount,
  };
  await ownedWrite(
    join(root, "source-before-build.json"),
    `${JSON.stringify(snapshot)}\n`,
  );
  await ownedWrite(
    join(root, "source-before-launch-services-registration.json"),
    `${JSON.stringify({ status: "passed", baseline: snapshot, current: snapshot })}\n`,
  );
  await ownedWrite(
    join(root, "sleep-wake-fixture.json"),
    `${JSON.stringify({ pid: 12345, appExecutable, appSha256, cli, cliSha256, approvedIdentityReuse: true, approvedIdentityRecord: record, packaged: true, expectedMainWebviewUrl: "tauri://localhost/" })}\n`,
  );
  await ownedWrite(
    join(root, "post-exit-acceptance-handoff.json"),
    `${JSON.stringify({ schemaVersion: 1, status: "waiting-for-post-exit-acceptance", runRoot: root, app, record, journal, desktopSha256: appSha256, cliSha256, release: join(root, ".native-provider-reader-post-exit-complete"), expectedReleaseContent: "dakia-native-provider-reader-post-exit-complete-v1\n" })}\n`,
  );
  const overlayCopy = join(root, "native-reader-thirteen-overlay.mjs");
  const seederCopy = join(root, "seed-native-reader-thirteen-fixtures.mjs");
  await ownedWrite(overlayCopy, overlayBytes);
  await ownedWrite(seederCopy, seederBytes);
  await ownedWrite(
    join(root, "native-reader-thirteen-owned-inputs.json"),
    `${JSON.stringify({ schemaVersion: 1, overlay: { path: overlayCopy, sourcePath: "scripts/native-reader-thirteen-overlay.mjs", sha256: sha256(overlayBytes) }, seeder: { path: seederCopy, sourcePath: "scripts/seed-native-reader-thirteen-fixtures.mjs", sha256: sha256(seederBytes) } })}\n`,
  );
  await ownedWrite(
    join(root, "native-reader-thirteen-overlay.json"),
    `${JSON.stringify({ schemaVersion: 1, mode: "native-reader-thirteen-inner-tls", fixtures: FIXTURES.map(([uid, id, relativePath, hash]) => ({ uid, id, relativePath, sha256: hash })) })}\n`,
  );
  const transcript = FIXTURES.flatMap(([uid, id, , hash]) =>
    ["metadata", "mime", "raw"].map((action) => ({
      at: "2026-10-22T00:00:00Z",
      direction: "client",
      action,
      uid,
      fixtureId: id,
      fixtureSha256: hash,
      fixtureAccount: true,
      mailbox: "INBOX",
    })),
  );
  await ownedWrite(
    join(root, "native-reader-thirteen-imap-transcript.jsonl"),
    `${transcript.map((record) => JSON.stringify(record)).join("\n")}\n`,
  );
  const evidenceDir = join(root, "native-ui-evidence");
  await (await import("node:fs/promises")).mkdir(evidenceDir, { mode: 0o700 });
  const observations = [];
  for (let index = 0; index < FIXTURES.length; index += 1) {
    const [uid, id] = FIXTURES[index];
    const axPath = join(evidenceDir, `uid-${uid}.ax.txt`);
    const screenshotPath = join(evidenceDir, `uid-${uid}.png`);
    const axParts = [visible[index][0], visible[index][1]];
    const warning =
      "This file may run software. Only open it if you trust the sender.";
    if (uid === 13)
      axParts.push(
        "claim-documents.pdf",
        "Save claim-documents.pdf to Downloads",
      );
    if (uid === 14) {
      for (const filename of oracles.filenameParametersAndEncodings
        .visibleFilenames)
        axParts.push(filename, `Save ${filename} to Downloads`);
      axParts.push(warning);
    }
    if (uid === 16)
      axParts.push(
        ...oracles.standardsUnknownTransferEncoding.saveControlNames,
        warning,
      );
    const axBytes = Buffer.from(`${axParts.join("\n")}\n`);
    const screenshotBytes = tinyPng;
    await ownedWrite(axPath, axBytes);
    await ownedWrite(screenshotPath, screenshotBytes);
    observations.push({
      uid,
      fixtureId: id,
      visibleSubject: visible[index][0],
      requiredText: visible[index][1],
      requiredTextVisible: true,
      decodeErrorVisible: false,
      retryVisible: false,
      ax: { path: axPath, sha256: sha256(axBytes) },
      screenshot: { path: screenshotPath, sha256: sha256(screenshotBytes) },
    });
  }
  const observationPath = join(
    root,
    "native-reader-thirteen-ui-observation.json",
  );
  const observation = {
    schemaVersion: 1,
    status: "observed",
    sequence: FIXTURES.map(([uid]) => uid),
    identity: {
      observedProcessPath: appExecutable,
      observedWebviewUrl: "tauri://localhost/",
      observedExecutableSha256: appSha256,
    },
    noAttachmentSaveInvoked: true,
    fixtures: observations,
    oracles,
  };
  await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  const context = {
    root,
    sourceRoot,
    manifestPath,
    manifestSha256: sha256(await readFile(manifestPath)),
    approvalRecordSha256,
    observationPath,
    transcript,
    observation,
  };
  await mutate(context);
  return context;
}

async function rejects(label, mutate, pattern) {
  const context = await fixtureRoot(mutate);
  try {
    await assert.rejects(
      verifyInputs({
        root: context.root,
        sourceRoot: context.sourceRoot,
        sourceManifestPath: context.manifestPath,
        sourceManifestSha256: context.manifestSha256,
        observationPath: context.observationPath,
        expectedApprovalRecordSha256: context.approvalRecordSha256,
      }),
      pattern,
      label,
    );
  } finally {
    await rm(context.root, { recursive: true, force: true });
  }
}

const positive = await fixtureRoot();
try {
  const verified = await verifyInputs({
    root: positive.root,
    sourceRoot: positive.sourceRoot,
    sourceManifestPath: positive.manifestPath,
    sourceManifestSha256: positive.manifestSha256,
    observationPath: positive.observationPath,
    expectedApprovalRecordSha256: positive.approvalRecordSha256,
  });
  assert.equal(verified.transcriptRecordCount, 39);
  const run = spawnSync(
    process.execPath,
    [
      join(positive.sourceRoot, "scripts/seal-native-reader-thirteen.mjs"),
      "--root",
      positive.root,
      "--source-root",
      positive.sourceRoot,
      "--source-manifest",
      positive.manifestPath,
      "--source-manifest-sha256",
      positive.manifestSha256,
      "--observation",
      positive.observationPath,
      "--expected-approval-record-sha256",
      positive.approvalRecordSha256,
    ],
    { encoding: "utf8" },
  );
  assert.equal(run.status, 0, run.stderr);
  const result = JSON.parse(run.stdout);
  assert.match(result.reportSha256, /^[a-f0-9]{64}$/);
  const report = JSON.parse(await readFile(result.report, "utf8"));
  assert.equal(report.status, "passed");
  assert.equal(report.fixtureCount, 13);
  assert.equal(report.transcriptRecordCount, 39);
} finally {
  await rm(positive.root, { recursive: true, force: true });
}

await rejects(
  "missing raw",
  async ({ root, transcript }) => {
    await rm(join(root, "native-reader-thirteen-imap-transcript.jsonl"));
    const mutated = transcript.filter(
      (record) => !(record.uid === 16 && record.action === "raw"),
    );
    await ownedWrite(
      join(root, "native-reader-thirteen-imap-transcript.jsonl"),
      `${mutated.map((record) => JSON.stringify(record)).join("\n")}\n`,
    );
  },
  /UID 16 lacks bound production raw evidence/,
);
await rejects(
  "changed screenshot",
  async ({ observation }) => {
    await writeFile(observation.fixtures[0].screenshot.path, "changed", {
      flag: "a",
    });
  },
  /screenshot evidence changed/,
);
await rejects(
  "wrong app identity",
  async ({ observationPath, observation }) => {
    await rm(observationPath);
    observation.identity.observedExecutableSha256 = "d".repeat(64);
    await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  },
  /identity observation does not match/,
);
await rejects(
  "source drift",
  async ({ root }) => {
    const path = join(root, "source-before-launch-services-registration.json");
    await rm(path);
    await ownedWrite(
      path,
      `${JSON.stringify({ status: "passed", current: { algorithm: "sha256-path-and-content-manifest", digest: "e".repeat(64), fileCount: 1 } })}\n`,
    );
  },
  /source before launch services registration does not match/,
);
await rejects(
  "live source drift",
  async ({ sourceRoot }) => {
    await writeFile(join(sourceRoot, "synthetic"), "changed");
  },
  /live source differs from frozen manifest: synthetic/,
);
const mismatchedApprovalDigest = await fixtureRoot();
try {
  await assert.rejects(
    verifyInputs({
      root: mismatchedApprovalDigest.root,
      sourceRoot: mismatchedApprovalDigest.sourceRoot,
      sourceManifestPath: mismatchedApprovalDigest.manifestPath,
      sourceManifestSha256: mismatchedApprovalDigest.manifestSha256,
      observationPath: mismatchedApprovalDigest.observationPath,
      expectedApprovalRecordSha256: "d".repeat(64),
    }),
    /approved identity preflight does not match record/,
    "expected approved identity record digest must bind the governed preflight",
  );
} finally {
  await rm(mismatchedApprovalDigest.root, { recursive: true, force: true });
}
const crossCheckoutHelper = await fixtureRoot();
try {
  await assert.rejects(
    verifyInputs({
      root: crossCheckoutHelper.root,
      sourceRoot: crossCheckoutHelper.sourceRoot,
      sourceManifestPath: crossCheckoutHelper.manifestPath,
      sourceManifestSha256: crossCheckoutHelper.manifestSha256,
      observationPath: crossCheckoutHelper.observationPath,
      expectedApprovalRecordSha256: crossCheckoutHelper.approvalRecordSha256,
      runtimeHelperPath: new URL(
        "./seal-native-reader-thirteen.mjs",
        import.meta.url,
      ).pathname,
    }),
    /native Reader sealer is outside the frozen source checkout/,
    "the sealer must execute from the checkout bound by its frozen manifest",
  );
} finally {
  await rm(crossCheckoutHelper.root, { recursive: true, force: true });
}
await rejects(
  "original approved record substitution",
  async ({ root }) => {
    const path = join(
      root,
      "approved-identity",
      "approved-native-identity.json",
    );
    const record = JSON.parse(await readFile(path, "utf8"));
    record.desktopSha256 = "d".repeat(64);
    await rm(path);
    await ownedWrite(path, `${JSON.stringify(record)}\n`);
  },
  /approved identity preflight does not match record/,
);
await rejects(
  "sealed original record requires sealing time",
  async ({ root }) => {
    const path = join(
      root,
      "approved-identity",
      "approved-native-identity.json",
    );
    const record = JSON.parse(await readFile(path, "utf8"));
    delete record.sealedAt;
    await rm(path);
    await ownedWrite(path, `${JSON.stringify(record)}\n`);
  },
  /approved identity record contract is invalid/,
);
await rejects(
  "candidate inventory journal binding",
  async ({ root }) => {
    const app = join(
      root,
      "approved-identity",
      "Dakia Fictional Provider Reader.app",
    );
    await ownedWrite(join(app, "unexpected-fixture-entry"), "unexpected");
  },
  /approved identity transaction is invalid/,
);
await rejects(
  "missing sealed original backup",
  async ({ root }) => {
    await rm(join(root, "sealed-approved-original.app"), {
      recursive: true,
      force: true,
    });
  },
  /retained sealed original is unavailable/,
);
await rejects(
  "tampered sealed original backup",
  async ({ root }) => {
    await writeFile(
      join(
        root,
        "sealed-approved-original.app",
        "Contents",
        "MacOS",
        "dakia-desktop",
      ),
      "tampered",
      { flag: "a" },
    );
  },
  /retained sealed original differs from approved inventory/,
);
await rejects(
  "candidate-installed topology has no source candidate",
  async ({ root }) => {
    await mkdir(join(root, "Dakia Fictional Provider Reader.app"), {
      mode: 0o700,
    });
  },
  /candidate-installed transaction topology is invalid/,
);
await rejects(
  "candidate bytes binding",
  async ({ root }) => {
    const launch = JSON.parse(
      await readFile(join(root, "sleep-wake-fixture.json"), "utf8"),
    );
    await writeFile(launch.appExecutable, "changed", { flag: "a" });
  },
  /approved identity transaction is invalid/,
);
await rejects(
  "owned helper cannot be rebound to another checkout",
  async ({ root }) => {
    const receiptPath = join(root, "native-reader-thirteen-owned-inputs.json");
    const receipt = JSON.parse(await readFile(receiptPath, "utf8"));
    const overlay = receipt.overlay;
    await writeFile(overlay.path, "\n// another checkout\n", { flag: "a" });
    overlay.sha256 = sha256(await readFile(overlay.path));
    await rm(receiptPath);
    await ownedWrite(receiptPath, `${JSON.stringify(receipt)}\n`);
  },
  /owned overlay copy does not match frozen source/,
);
await rejects(
  "candidate journal hash mismatch",
  async ({ root }) => {
    const path = join(
      root,
      "approved-identity",
      ".approved-native-identity-swap.json",
    );
    const journal = JSON.parse(await readFile(path, "utf8"));
    journal.candidateDesktopSha256 = "d".repeat(64);
    await rm(path);
    await ownedWrite(path, `${JSON.stringify(journal)}\n`);
  },
  /approved identity transaction is invalid/,
);
await rejects(
  "screenshot must be a PNG",
  async ({ observationPath, observation }) => {
    const screenshot = observation.fixtures[0].screenshot;
    const bytes = Buffer.from("not a PNG");
    await rm(screenshot.path);
    await ownedWrite(screenshot.path, bytes);
    screenshot.sha256 = sha256(bytes);
    await rm(observationPath);
    await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  },
  /screenshot is not a PNG/,
);
await rejects(
  "screenshot dimensions are bounded",
  async ({ observationPath, observation }) => {
    const screenshot = observation.fixtures[0].screenshot;
    const bytes = Buffer.from(tinyPng);
    bytes.writeUInt32BE(32_769, 16);
    await rm(screenshot.path);
    await ownedWrite(screenshot.path, bytes);
    screenshot.sha256 = sha256(bytes);
    await rm(observationPath);
    await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  },
  /screenshot PNG dimensions are unsafe/,
);
await rejects(
  "wrong account route",
  async ({ root, transcript }) => {
    await rm(join(root, "native-reader-thirteen-imap-transcript.jsonl"));
    transcript[0].fixtureAccount = false;
    await ownedWrite(
      join(root, "native-reader-thirteen-imap-transcript.jsonl"),
      `${transcript.map((record) => JSON.stringify(record)).join("\n")}\n`,
    );
  },
  /UID 4 lacks bound production metadata evidence/,
);
await rejects(
  "unbound decoys cannot establish bound order",
  async ({ root, transcript }) => {
    await rm(join(root, "native-reader-thirteen-imap-transcript.jsonl"));
    const boundUid4 = transcript.filter((record) => record.uid === 4).reverse();
    const remaining = transcript.filter((record) => record.uid !== 4);
    const decoys = ["metadata", "mime", "raw"].map((action) => ({
      ...boundUid4[0],
      action,
      fixtureAccount: false,
    }));
    const mutated = [...decoys, ...boundUid4, ...remaining];
    await ownedWrite(
      join(root, "native-reader-thirteen-imap-transcript.jsonl"),
      `${mutated.map((record) => JSON.stringify(record)).join("\n")}\n`,
    );
  },
  /UID 4 lacks an ordered metadata to MIME to raw fallback sequence/,
);
await rejects(
  "AX lacks visible subject",
  async ({ observationPath, observation }) => {
    const fixture = observation.fixtures[0];
    const changed = Buffer.from(`${fixture.requiredText}\n`);
    await rm(fixture.ax.path);
    await ownedWrite(fixture.ax.path, changed);
    fixture.ax.sha256 = sha256(changed);
    await rm(observationPath);
    await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  },
  /AX evidence lacks "Synthetic nested alternatives"/,
);
await rejects(
  "bad exit",
  async ({ root }) => {
    const path = join(root, "app-exit-status.txt");
    await rm(path);
    await ownedWrite(path, "1\n");
  },
  /did not exit cleanly/,
);
await rejects(
  "pixel overclaim",
  async ({ observationPath, observation }) => {
    await rm(observationPath);
    observation.oracles.providerSignatureInline.decodedPixelRenderingClaimed = true;
    await ownedWrite(observationPath, `${JSON.stringify(observation)}\n`);
  },
  /security oracle observations are incomplete or changed/,
);

console.log(
  "native Reader thirteen-fixture sealer: immutable snapshot plus missing raw, changed screenshot, identity, source, exit, and pixel-overclaim sensitivity passed",
);
