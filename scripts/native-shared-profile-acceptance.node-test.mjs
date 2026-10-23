import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  activeImapConnection,
  parseArguments,
  parseActiveImapConnectionResult,
  parseApprovedBundleInfoPlist,
  readOnlyWavePlan,
  validateApprovedIdentityDocuments,
  validateAccounts,
  validatePrimaryCachedBody,
  validateReadOnlyPlan,
  validateRuntimeManifest,
  validateSearch,
} from "./native-shared-profile-acceptance.mjs";

const primary = "62a9320c-04f5-47c2-b4fa-d4952f0b6121";
const secondary = "62a9320c-04f5-47c2-b4fa-d4952f0b6122";
const shaA = "a".repeat(64);
const shaB = "b".repeat(64);
const EXPECTED_BUNDLE_IDENTIFIER =
  "dev.dakia.fictional-provider-reader.run-REDACTED";
const EXPECTED_BUNDLE_NAME = "Dakia Fictional Provider Reader";
const GOLDEN_INFO_PLIST = fileURLToPath(
  new URL(
    "../testdata/native-mail-safety/packaged-info.plist",
    import.meta.url,
  ),
);

async function plistFile(contents, name = "Info.plist") {
  const root = await mkdtemp("/private/tmp/dakia-plist-test-");
  const path = join(root, name);
  await writeFile(path, contents);
  return { root, path };
}

function plistXml({
  identifier = EXPECTED_BUNDLE_IDENTIFIER,
  name = EXPECTED_BUNDLE_NAME,
} = {}) {
  return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>${identifier}</string><key>CFBundleName</key><string>${name}</string></dict></plist>`;
}

async function assertInvalidPlist(path) {
  await assert.rejects(
    () =>
      parseApprovedBundleInfoPlist(
        path,
        EXPECTED_BUNDLE_IDENTIFIER,
        EXPECTED_BUNDLE_NAME,
      ),
    /invalid or ambiguous/,
  );
}

function completeArguments() {
  return [
    "--root",
    "/private/tmp/fictional",
    "--app-pid",
    "42",
    "--app-executable",
    "/private/tmp/fictional/Dakia Fictional Provider Reader.app/Contents/MacOS/dakia-desktop",
    "--cli",
    "/private/tmp/fictional/Dakia Fictional Provider Reader.app/Contents/MacOS/dakia",
    "--profile",
    "/private/tmp/fictional/profile",
    "--imap-port",
    "3993",
    "--imap-proxy-pid",
    "43",
    "--expected-app-sha256",
    shaA,
    "--expected-cli-sha256",
    shaB,
    "--report",
    "/private/tmp/fictional/native-shared-profile-acceptance.json",
  ];
}

test("shared-profile arguments require explicit artifact identities and owned endpoints", () => {
  const parsed = parseArguments(completeArguments());
  assert.equal(parsed.appPid, 42);
  assert.equal(parsed.imapPort, 3993);
  assert.equal(parsed.imapProxyPid, 43);
  assert.equal(parsed["expected-app-sha256"], shaA);
  assert.equal(parsed["expected-cli-sha256"], shaB);

  assert.throws(
    () => parseArguments(completeArguments().slice(0, -2)),
    /missing --report/,
  );
  const invalidHash = completeArguments();
  invalidHash[invalidHash.indexOf(shaA)] = "not-a-hash";
  assert.throws(() => parseArguments(invalidHash), /not a lowercase SHA-256/);
});

test("approved wrapper plist parser accepts the faithful formatted Tauri plist", async () => {
  assert.deepEqual(
    await parseApprovedBundleInfoPlist(
      GOLDEN_INFO_PLIST,
      EXPECTED_BUNDLE_IDENTIFIER,
      EXPECTED_BUNDLE_NAME,
    ),
    {
      bundleIdentifier: EXPECTED_BUNDLE_IDENTIFIER,
      bundleName: EXPECTED_BUNDLE_NAME,
    },
  );
});

test("approved wrapper plist parser accepts a binary plist", async () => {
  const binary = execFileSync(
    "/usr/bin/python3",
    [
      "-c",
      "import plistlib, sys; sys.stdout.buffer.write(plistlib.dumps({'CFBundleIdentifier': sys.argv[1], 'CFBundleName': sys.argv[2]}, fmt=plistlib.FMT_BINARY))",
      EXPECTED_BUNDLE_IDENTIFIER,
      EXPECTED_BUNDLE_NAME,
    ],
    { maxBuffer: 4_096 },
  );
  const { path } = await plistFile(binary);
  assert.equal(
    (
      await parseApprovedBundleInfoPlist(
        path,
        EXPECTED_BUNDLE_IDENTIFIER,
        EXPECTED_BUNDLE_NAME,
      )
    ).bundleName,
    EXPECTED_BUNDLE_NAME,
  );
});

test("approved wrapper plist parser rejects ambiguous or forged identity documents", async () => {
  const duplicate = await plistFile(
    `<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>${EXPECTED_BUNDLE_IDENTIFIER}</string><key>CFBundleIdentifier</key><string>${EXPECTED_BUNDLE_IDENTIFIER}</string><key>CFBundleName</key><string>${EXPECTED_BUNDLE_NAME}</string></dict></plist>`,
  );
  await assertInvalidPlist(duplicate.path);

  const nestedAndComment = await plistFile(
    `<?xml version="1.0"?><plist version="1.0"><!-- <key>CFBundleIdentifier</key><string>${EXPECTED_BUNDLE_IDENTIFIER}</string> --><dict><key>CFBundleIdentifier</key><string>wrong.example</string><key>CFBundleName</key><string>Wrong</string><key>Nested</key><dict><key>CFBundleIdentifier</key><string>${EXPECTED_BUNDLE_IDENTIFIER}</string><key>CFBundleName</key><string>${EXPECTED_BUNDLE_NAME}</string></dict></dict></plist>`,
  );
  await assertInvalidPlist(nestedAndComment.path);

  const nestedDuplicate = await plistFile(
    `<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>${EXPECTED_BUNDLE_IDENTIFIER}</string><key>CFBundleName</key><string>${EXPECTED_BUNDLE_NAME}</string><key>Nested</key><dict><key>Repeated</key><string>one</string><key>Repeated</key><string>two</string></dict></dict></plist>`,
  );
  await assertInvalidPlist(nestedDuplicate.path);

  for (const contents of [
    plistXml({ identifier: "wrong.example" }),
    plistXml({ name: "Wrong" }),
    '<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><integer>1</integer><key>CFBundleName</key><true/></dict></plist>',
    Buffer.alloc(0),
    "not a plist",
    Buffer.alloc(1_048_577),
  ]) {
    const { path } = await plistFile(contents);
    await assertInvalidPlist(path);
  }
});

test("approved wrapper plist parser rejects a symbolic link", async () => {
  const target = await plistFile(plistXml(), "target.plist");
  const link = join(target.root, "Info.plist");
  await symlink(target.path, link);
  await assertInvalidPlist(link);
});

test("the approved identity path uses the structured plist parser", async () => {
  const helper = await readFile(
    new URL("./native-shared-profile-acceptance.mjs", import.meta.url),
    "utf8",
  );
  assert.match(helper, /await parseApprovedBundleInfoPlist\(/);
  assert.doesNotMatch(helper, /infoPlist\.includes/);
});

test("logical snapshot keeps its delay independent of IMAP retry injection", async () => {
  const helper = await readFile(
    new URL("./native-shared-profile-acceptance.mjs", import.meta.url),
    "utf8",
  );
  const start = helper.indexOf("async function stableLogicalSnapshot");
  const end = helper.indexOf("export function readOnlyWavePlan", start);
  assert.ok(start >= 0 && end > start);
  const snapshot = helper.slice(start, end);
  assert.match(
    snapshot,
    /await new Promise\(\(resolvePromise\) => setTimeout\(resolvePromise, 250\)\);/,
  );
  assert.doesNotMatch(snapshot, /await sleep\(250\);/);
});

test("active IMAP parser accepts only exact host-warning exit-one connection rows", () => {
  const warning =
    "lsof: WARNING: can't stat() smbfs file system /Volumes/.timemachine/example/TimeMachine\n" +
    "      Output information may be incomplete.\n" +
    '      assuming "dev=3600000d" from mount table\n';
  const result = {
    status: 1,
    stdout:
      "p42\ncdakia-desktop\nf12u\nn127.0.0.1:49152->127.0.0.1:3993\nf13u\nn127.0.0.1:49153->127.0.0.1:3993\n",
    stderr: warning,
  };
  assert.deepEqual(parseActiveImapConnectionResult(result, 42, 3993), [
    "127.0.0.1:49152->127.0.0.1:3993",
    "127.0.0.1:49153->127.0.0.1:3993",
  ]);
  assert.throws(
    () =>
      parseActiveImapConnectionResult(
        { ...result, stderr: "lsof: warning changed\n" },
        42,
        3993,
      ),
    /unknown diagnostics/,
  );
  assert.throws(
    () =>
      parseActiveImapConnectionResult(
        {
          ...result,
          stdout:
            "p42\ncdakia-desktop\nf12u\nn127.0.0.1:49152->127.0.0.1:3994\n",
        },
        42,
        3993,
      ),
    /escaped the owned proxy/,
  );
  assert.throws(
    () =>
      parseActiveImapConnectionResult(
        {
          ...result,
          stdout:
            "p43\ncdakia-desktop\nf12u\nn127.0.0.1:49152->127.0.0.1:3993\n",
        },
        42,
        3993,
      ),
    /PID row is malformed/,
  );
  assert.throws(
    () =>
      parseActiveImapConnectionResult(
        { status: 1, stdout: "", stderr: warning },
        42,
        3993,
      ),
    /no owned loopback connections/,
  );
});

test("active IMAP integrity failures are never retried into acceptance", async () => {
  const validRows =
    "p42\ncdakia-desktop\nf12u\nn127.0.0.1:49152->127.0.0.1:3993\nf13u\nn127.0.0.1:49153->127.0.0.1:3993\n";
  let calls = 0;
  await assert.rejects(
    () =>
      activeImapConnection(42, 3993, {
        executeCommand: async () => {
          calls += 1;
          if (calls === 1) {
            const error = new Error("lsof exit one");
            error.code = 1;
            error.stdout = validRows;
            error.stderr = "lsof: warning changed\n";
            throw error;
          }
          return { stdout: validRows, stderr: "" };
        },
        sleep: async () => {},
      }),
    /unknown diagnostics/,
  );
  assert.equal(
    calls,
    1,
    "a later clean sample cannot forgive malformed output",
  );
});

test("active IMAP retries only a known-warning probe with no owned rows", async () => {
  const warning =
    "lsof: WARNING: can't stat() smbfs file system /Volumes/.timemachine/example/TimeMachine\n" +
    "      Output information may be incomplete.\n" +
    '      assuming "dev=3600000d" from mount table\n';
  const validRows =
    "p42\ncdakia-desktop\nf12u\nn127.0.0.1:49152->127.0.0.1:3993\nf13u\nn127.0.0.1:49153->127.0.0.1:3993\n";
  let calls = 0;
  const result = await activeImapConnection(42, 3993, {
    executeCommand: async () => {
      calls += 1;
      if (calls === 1) {
        const error = new Error("lsof exit one");
        error.code = 1;
        error.stdout = "";
        error.stderr = warning;
        throw error;
      }
      return { stdout: validRows, stderr: "" };
    },
    sleep: async () => {},
  });
  assert.deepEqual(result, {
    established: true,
    attempts: 2,
    exactLoopbackConnections: 2,
  });
  assert.equal(calls, 2);
});

test("the bounded wave contains only exact-profile local account and search reads", () => {
  const profile = "/private/tmp/fictional/profile";
  const plan = readOnlyWavePlan(profile);
  validateReadOnlyPlan(plan, profile);
  assert.equal(plan.length, 8);
  assert.deepEqual([...new Set(plan.map(({ args }) => args[3]))].sort(), [
    "account",
    "search",
  ]);
  assert.ok(plan.every(({ args }) => !args.includes("--remote")));
  const primaryUid3Query = "Fictional provider body reached the real Reader";
  assert.ok(
    plan
      .filter(({ kind }) => kind === "primary-positive")
      .every(({ args }) => args[4] === primaryUid3Query),
  );
  assert.ok(
    plan
      .filter(({ kind }) => kind === "secondary-negative")
      .every(({ args }) => args[4] === primaryUid3Query),
  );

  const mutated = structuredClone(plan);
  mutated[0].args = ["--data-dir", profile, "--json", "sync"];
  assert.throws(
    () => validateReadOnlyPlan(mutated, profile),
    /contains a mutation/,
  );
  const accountMutation = structuredClone(plan);
  accountMutation[0].args = [
    "--data-dir",
    profile,
    "--json",
    "account",
    "remove",
  ];
  assert.throws(
    () => validateReadOnlyPlan(accountMutation, profile),
    /contains a mutation/,
  );
});

test("account and colliding-message checks reject cross-account output", () => {
  validateAccounts([
    { id: primary, email: "fictional-reader@localhost" },
    { id: secondary, email: "fictional-reader-two@localhost" },
  ]);
  assert.throws(
    () =>
      validateAccounts([{ id: primary, email: "fictional-reader@localhost" }]),
    /exactly two fictional accounts/,
  );

  validateSearch("primary-positive", [
    { account_id: primary, subject: "Remote content safety check", uid: 3 },
  ]);
  assert.throws(
    () =>
      validateSearch("primary-positive", [
        { account_id: primary, subject: "Remote content safety check", uid: 3 },
        { account_id: primary, subject: "Remote content safety check", uid: 5 },
      ]),
    /unexpected count/,
  );
  validateSearch("secondary-positive", [
    {
      account_id: secondary,
      subject: "Account two collision safety check",
      uid: 3,
    },
  ]);
  validateSearch("primary-negative", []);
  validateSearch("secondary-negative", []);
  assert.throws(
    () =>
      validateSearch("secondary-positive", [
        {
          account_id: primary,
          subject: "Account two collision safety check",
          uid: 3,
        },
      ]),
    /crossed an account or immutable UID boundary/,
  );
});

test("the independent cache proof binds the full faithful primary Reader body", () => {
  const fullBody = [
    "Fictional provider body reached the real Reader.",
    "Native remote-resource matrix marker.",
    "Remote CSS and font marker stays readable.",
    "Submit hostile formNative mailto regression anchors. Open each in Dakia compose and close without sending.",
    "Native mailto CR To rejection",
    "Native mailto CR Cc rejection",
    "Native mailto CR subject rejection",
    "Native mailto LF To rejection",
    "Native mailto LF Cc rejection",
    "Native mailto LF subject rejection",
    "Native mailto double encoded To",
    "Native mailto double encoded Cc",
    "Native mailto double encoded subject",
    "Native mailto double encoded body",
    "Native mailto body CRLF",
    "Active-content canaries must remain inert before and after consent.",
    "On 17 October 2026, Fictional Sender wrote:Fictional quoted history remains inside the isolated history iframe.",
    "Expand, collapse, expand, collapse, and expand this history. The marker and Reader controls must remain visible and usable after every transition.",
    "History link",
  ].join("\n");
  const snapshot = {
    tables: {
      messages: {
        rows: [
          {
            id: `${primary}:INBOX:3`,
            account_id: primary,
            uid: 3,
            subject: "Remote content safety check",
          },
        ],
      },
      message_content_cache: {
        rows: [
          {
            message_id: `${primary}:INBOX:3`,
            content_state: "complete",
            body_text: fullBody.replaceAll("\n", "\r\n"),
          },
        ],
      },
    },
  };
  assert.deepEqual(validatePrimaryCachedBody(snapshot), {
    messageId: `${primary}:INBOX:3`,
    normalization: "CRLF and CR normalized to LF; no whitespace is trimmed",
    utf8Bytes: 925,
    sha256: "9ea8bac4c145580dead7b8197b8c89157efd78053a1b12bd65b110407c45f3c5",
  });

  const wrongBody = structuredClone(snapshot);
  wrongBody.tables.message_content_cache.rows[0].body_text = fullBody.replace(
    "Native remote-resource matrix marker.",
    "Native remote-resource matrix marker!",
  );
  assert.throws(
    () => validatePrimaryCachedBody(wrongBody),
    /does not match the faithful Reader fixture/,
  );
});

test("runtime manifest must bind both packaged executables to exact hashes", () => {
  const wrapper = "/private/tmp/fictional/Dakia Fictional Provider Reader.app";
  const app = `${wrapper}/Contents/MacOS/dakia-desktop`;
  const cli = `${wrapper}/Contents/MacOS/dakia`;
  const manifest = {
    schemaVersion: 1,
    files: [
      { path: "Contents/MacOS/dakia-desktop", bytes: 100, sha256: shaA },
      { path: "Contents/MacOS/dakia", bytes: 90, sha256: shaB },
    ],
  };
  validateRuntimeManifest(manifest, app, shaA, cli, shaB);
  assert.throws(
    () => validateRuntimeManifest(manifest, app, shaA, cli, shaA),
    /does not own Contents\/MacOS\/dakia/,
  );
  const duplicate = structuredClone(manifest);
  duplicate.files.push(structuredClone(duplicate.files[0]));
  assert.throws(
    () => validateRuntimeManifest(duplicate, app, shaA, cli, shaB),
    /duplicate paths/,
  );
});

test("approved identity reuse binds the exact active transaction and retained original", () => {
  const root = "/private/tmp/fictional";
  const approvedRoot = "/private/tmp/approved-fictional-fixture";
  const approvedApp = `${approvedRoot}/Dakia Fictional Provider Reader.app`;
  const originalAppSha = "1".repeat(64);
  const originalCliSha = "2".repeat(64);
  const backupInventory = {
    digest: "c".repeat(64),
    entries: [
      { path: ".", type: "directory", mode: 493, uid: 501, gid: 20 },
      {
        path: "Contents/MacOS/dakia-desktop",
        type: "file",
        sha256: originalAppSha,
      },
      { path: "Contents/MacOS/dakia", type: "file", sha256: originalCliSha },
    ],
  };
  const candidateInventory = {
    digest: "d".repeat(64),
    entries: [{ path: ".", type: "directory", mode: 493, uid: 501, gid: 20 }],
  };
  const record = {
    schemaVersion: 1,
    contract: "dakia-native-provider-reader-approved-identity-v1",
    approvedRoot,
    approvedApp,
    displayName: "Dakia Fictional Provider Reader",
    bundleIdentifier: "dev.dakia.fictional-provider-reader.run-approvedtest",
    desktopSha256: originalAppSha,
    cliSha256: originalCliSha,
    inventory: backupInventory,
  };
  const journal = {
    schemaVersion: 1,
    contract: "dakia-native-provider-reader-approved-identity-v1",
    stage: "candidate-installed",
    approvedRoot,
    approvedApp,
    runRoot: root,
    candidateApp: `${root}/Dakia Fictional Provider Reader.app`,
    backupApp: `${root}/sealed-approved-original.app`,
    testedCandidateApp: `${root}/tested-approved-candidate.app`,
    originalInventoryDigest: backupInventory.digest,
    candidateInventoryDigest: candidateInventory.digest,
    candidateDesktopSha256: shaA,
    candidateCliSha256: shaB,
  };
  const input = {
    record,
    recordPath: `${approvedRoot}/approved-native-identity.json`,
    journal,
    root,
    appBundle: approvedApp,
    appSha256: shaA,
    cliSha256: shaB,
    candidateInventory,
    backupInventory,
  };
  validateApprovedIdentityDocuments(input);
  assert.throws(
    () =>
      validateApprovedIdentityDocuments({
        ...input,
        journal: { ...journal, stage: "prepared" },
      }),
    /does not bind the live wrapper/,
  );
  assert.throws(
    () =>
      validateApprovedIdentityDocuments({
        ...input,
        candidateInventory: { ...candidateInventory, digest: "e".repeat(64) },
      }),
    /does not bind the live wrapper/,
  );
  assert.throws(
    () =>
      validateApprovedIdentityDocuments({
        ...input,
        backupInventory: { ...backupInventory, entries: [] },
      }),
    /does not match the retained sealed original/,
  );
});

test("native harness builds and copies the isolated CLI before signing and inventory", async () => {
  const harness = await readFile(
    new URL("./native-provider-reader-acceptance.sh", import.meta.url),
    "utf8",
  );
  const build = harness.indexOf(
    "cargo build --locked -p dakia-cli --bin dakia",
  );
  const copy = harness.indexOf(
    'cp "$cli_binary" "$app_root/Contents/MacOS/dakia"',
  );
  const sign = harness.indexOf('codesign --force --deep --sign - "$app_root"');
  const inventory = harness.indexOf('"$run_root/native-runtime-files.json"');
  assert.ok(build > 0, "isolated CLI build is present");
  assert.match(
    harness.slice(build, copy),
    /--target "\$cli_target"[\s\S]*cli_binary="\$build_target\/\$cli_target\/debug\/dakia"/,
  );
  assert.ok(copy > build, "fresh CLI is copied after its build");
  assert.ok(sign > copy, "wrapper is signed after the CLI is present");
  assert.ok(inventory > sign, "runtime inventory hashes signed executables");
  assert.match(harness, /native-shared-profile-acceptance\.mjs/);
});

test("acceptance implementation launches exactly one eight-reader wave", async () => {
  const helper = await readFile(
    new URL("./native-shared-profile-acceptance.mjs", import.meta.url),
    "utf8",
  );
  assert.equal(
    helper.match(/plan\.map\(\(item\) => runCliReader\(cli, item\)\)/g)?.length,
    1,
  );
  assert.equal(
    helper.match(/await runCliReader\(cli,/g)?.length ?? 0,
    0,
    "no extra sequential readers may run after the bounded wave",
  );
  assert.match(
    helper,
    /const cachedPrimaryBody = validatePrimaryCachedBody\(before\)/,
  );
  assert.match(helper, /cachedReaderBody: cachedPrimaryBody/);
  const searchValidator = helper.slice(
    helper.indexOf("export function validateSearch"),
    helper.indexOf("async function runCliReader"),
  );
  assert.doesNotMatch(
    searchValidator,
    /body_text|body_html/,
    "CLI search remains a metadata assertion; cache content is proven separately",
  );
});
