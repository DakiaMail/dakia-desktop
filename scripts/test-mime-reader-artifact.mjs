import { createHash, randomUUID } from "node:crypto";
import { execFileSync, spawn } from "node:child_process";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sourceSnapshot } from "./run-mail-safety.mjs";

const EXPECTED_FIXTURE_IDS = [
  "competing-nested-alternatives",
  "attached-message-rfc822",
  "multipart-attachment-container",
  "empty-alternative-leaf",
  "truncated-multipart-lf",
  "charset-matrix",
  "format-flowed-delsp",
  "international-reader-semantics",
  "ambiguous-inline-identities",
  "provider-signature-inline",
  "filename-parameters-and-encodings",
  "linkedin-inline-content-id",
  "standards-unknown-transfer-encoding",
];
const EXPECTED_FIXTURE_COUNT = EXPECTED_FIXTURE_IDS.length;

function run(command, args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", env });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0 && !signal) resolve();
      else
        reject(
          new Error(
            `${command} ${args.join(" ")} failed with code ${code} signal ${signal}`,
          ),
        );
    });
  });
}

async function sha256(path) {
  return createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
}

async function durableProfileSnapshot(root) {
  const names = ["mime-reader-profile.sqlite", "vault.key"];
  let walBytes = 0;
  try {
    const wal = await stat(join(root, "mime-reader-profile.sqlite-wal"));
    if (!wal.isFile()) throw new Error("SQLite WAL path is not a regular file");
    walBytes = wal.size;
    if (walBytes > 0) names.push("mime-reader-profile.sqlite-wal");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  const files = [];
  const aggregate = createHash("sha256");
  for (const name of names.sort()) {
    const path = join(root, name);
    const metadata = await stat(path);
    if (!metadata.isFile() || metadata.size === 0) {
      throw new Error(`durable profile file is absent or empty: ${name}`);
    }
    const digest = await sha256(path);
    files.push({ path: name, bytes: metadata.size, sha256: digest });
    aggregate.update(name).update("\0").update(digest).update("\0");
  }
  return { files, sha256: aggregate.digest("hex"), walBytes };
}

function validateCoreArtifact(core, nonce) {
  if (
    core.schemaVersion !== 1 ||
    core.nonce !== nonce ||
    core.database !== "mime-reader-profile.sqlite" ||
    core.producer !==
      "MailService::hydrate_message_with_imap_endpoint_for_test" ||
    core.profileState !== "closed-and-reopened-after-provider-hydration" ||
    core.messages?.length !== EXPECTED_FIXTURE_COUNT
  ) {
    throw new Error(
      "core MIME profile artifact has the wrong identity or producer",
    );
  }
  const fixtureIds = new Set(core.messages.map((item) => item.fixtureId));
  const messageIds = new Set(core.messages.map((item) => item.messageId));
  if (
    fixtureIds.size !== EXPECTED_FIXTURE_COUNT ||
    messageIds.size !== EXPECTED_FIXTURE_COUNT ||
    EXPECTED_FIXTURE_IDS.some((fixtureId) => !fixtureIds.has(fixtureId)) ||
    core.messages.some((item) => item.expectedUidValidity !== 7707)
  ) {
    throw new Error(
      "core MIME profile artifact has duplicate or stale message identities",
    );
  }
}

const root = await mkdtemp(join(tmpdir(), "dakia-mime-reader-artifact-"));
const nonce = randomUUID();
const profile = join(root, "mime-reader-profile.sqlite");
const coreManifest = join(root, "core-manifest.json");
const tauriArtifact = join(root, "tauri-message-content.json");
const frontendResults = join(root, "frontend-results.json");
const pipelineReceipt = join(root, "pipeline-receipt.json");
const sourceManifest = join(root, "source-binding.json");
const nodeMajor = Number(process.versions.node.split(".")[0]);
const vitestNodeOptions = [
  process.env.NODE_OPTIONS,
  nodeMajor >= 22 ? "--no-experimental-webstorage" : null,
]
  .filter(Boolean)
  .join(" ");
const sourceBefore = sourceSnapshot();
if (!sourceBefore) {
  throw new Error("MIME Reader artifact lane requires a source snapshot");
}
const toolchain = {
  node: process.version,
  cargo: execFileSync("cargo", ["--version"], { encoding: "utf8" }).trim(),
  rustc: execFileSync("rustc", ["--version"], { encoding: "utf8" }).trim(),
};
const env = {
  ...process.env,
  DAKIA_MIME_READER_ARTIFACT_NONCE: nonce,
  DAKIA_MIME_READER_CORE_ARTIFACT_ROOT: root,
  DAKIA_MIME_READER_TAURI_ARTIFACT: tauriArtifact,
};

try {
  await writeFile(join(root, ".dakia-mime-reader-artifact"), `${nonce}\n`, {
    mode: 0o600,
  });
  await writeFile(
    sourceManifest,
    `${JSON.stringify(sourceBefore, null, 2)}\n`,
    {
      mode: 0o600,
    },
  );
  const sourceManifestSha256 = await sha256(sourceManifest);
  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-core",
      "--lib",
      "--locked",
      "mail::tests::mime_reader_handoff_tests::public_full_and_selective_reader_fetches_preserve_every_governed_reader_fixture",
      "--",
      "--exact",
    ],
    env,
  );
  const core = JSON.parse(await readFile(coreManifest, "utf8"));
  validateCoreArtifact(core, nonce);
  const profileMetadata = await stat(profile);
  if (!profileMetadata.isFile() || profileMetadata.size === 0) {
    throw new Error("provider-hydrated MIME profile is absent or empty");
  }
  const profileBeforeTauri = await durableProfileSnapshot(root);
  if (profileBeforeTauri.walBytes !== 0) {
    throw new Error("core handoff did not checkpoint the hydrated profile");
  }
  const profileIdentity = {
    device: profileMetadata.dev.toString(),
    inode: profileMetadata.ino.toString(),
  };
  const coreManifestSha256 = await sha256(coreManifest);

  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-desktop",
      "--lib",
      "--locked",
      "mime_reader_artifact_tests::real_mime_profile_reaches_reader_through_the_message_content_tauri_command",
      "--",
      "--exact",
    ],
    env,
  );
  const profileAfterTauri = await durableProfileSnapshot(root);
  const profileMetadataAfterTauri = await stat(profile);
  if (
    profileMetadataAfterTauri.dev.toString() !== profileIdentity.device ||
    profileMetadataAfterTauri.ino.toString() !== profileIdentity.inode
  ) {
    throw new Error("Tauri replaced the provider-hydrated profile");
  }
  const tauri = JSON.parse(await readFile(tauriArtifact, "utf8"));
  if (
    tauri.nonce !== nonce ||
    tauri.messages?.length !== EXPECTED_FIXTURE_COUNT
  ) {
    throw new Error(
      "Tauri MIME payload artifact has the wrong nonce or fixture count",
    );
  }

  await run(
    "npx",
    [
      "vitest",
      "run",
      "apps/desktop/src/components/mimeReaderArtifact.test.tsx",
      "--maxWorkers=1",
      "--reporter=json",
      "--outputFile",
      frontendResults,
    ],
    {
      ...env,
      ...(vitestNodeOptions ? { NODE_OPTIONS: vitestNodeOptions } : {}),
      VITE_DAKIA_MIME_READER_ARTIFACT_NONCE: nonce,
      VITE_DAKIA_MIME_READER_TAURI_ARTIFACT: Buffer.from(
        JSON.stringify(tauri),
      ).toString("base64"),
    },
  );
  const frontend = JSON.parse(await readFile(frontendResults, "utf8"));
  if (
    frontend.numTotalTests !== EXPECTED_FIXTURE_COUNT ||
    frontend.numPassedTests !== EXPECTED_FIXTURE_COUNT ||
    frontend.numPendingTests !== 0
  ) {
    throw new Error(
      "the Reader artifact lane did not execute every passing, non-skipped fixture test",
    );
  }
  const profileAfterReact = await durableProfileSnapshot(root);
  if (profileAfterReact.sha256 !== profileAfterTauri.sha256) {
    throw new Error("React changed the durable profile after Tauri handoff");
  }
  const sourceAfter = sourceSnapshot();
  if (
    !sourceAfter ||
    sourceAfter.digest !== sourceBefore.digest ||
    sourceAfter.fileCount !== sourceBefore.fileCount
  ) {
    throw new Error("source changed during the MIME Reader artifact lane");
  }
  await writeFile(
    pipelineReceipt,
    `${JSON.stringify(
      {
        schemaVersion: 1,
        nonce,
        producer: core.producer,
        profileState: core.profileState,
        fixtureCount: EXPECTED_FIXTURE_COUNT,
        source: {
          before: sourceBefore,
          after: sourceAfter,
          manifest: "source-binding.json",
          manifestSha256: sourceManifestSha256,
        },
        toolchain,
        commands: [
          { stage: "core", status: "passed" },
          { stage: "tauri", status: "passed" },
          {
            stage: "react",
            status: "passed",
            tests: EXPECTED_FIXTURE_COUNT,
            skipped: 0,
          },
        ],
        profile: {
          path: core.database,
          identity: profileIdentity,
          beforeTauri: profileBeforeTauri,
          afterTauri: profileAfterTauri,
          afterReact: profileAfterReact,
          excludedCoordinationFile: "mime-reader-profile.sqlite-shm",
          checkpointedWalRequiredEmptyBeforeTauri: true,
          expectedTauriWrite:
            "message_content may update cache last_accessed in the same profile",
        },
        artifacts: {
          coreManifestSha256,
          tauriArtifactSha256: await sha256(tauriArtifact),
          frontendResultsSha256: await sha256(frontendResults),
        },
        stages: [
          "production MailService full fetch",
          "production MailService selective fetch",
          "production MailService hydration",
          "SQLite close and reopen",
          "Tauri message_content command",
          "React Reader",
        ],
      },
      null,
      2,
    )}\n`,
    { mode: 0o600 },
  );
} finally {
  if (process.env.DAKIA_KEEP_MIME_READER_ARTIFACTS === "1") {
    process.stderr.write(`Retained MIME Reader artifacts: ${root}\n`);
  } else {
    await rm(root, { recursive: true, force: true });
  }
}
