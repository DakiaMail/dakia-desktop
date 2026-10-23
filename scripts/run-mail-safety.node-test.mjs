import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  commandsFor,
  runMailSafety,
  SOURCE_SNAPSHOT_PATHS,
  sourceSnapshot,
} from "./run-mail-safety.mjs";

test("source snapshots bind the standalone fatal recovery entry", () => {
  assert.ok(SOURCE_SNAPSHOT_PATHS.includes("apps/desktop/fatal-recovery.html"));
});

test("source snapshots bind native testdata, icons, and resources by content", async (t) => {
  for (const path of [
    "apps/desktop/src-tauri/testdata",
    "apps/desktop/src-tauri/icons",
    "apps/desktop/src-tauri/resources",
  ]) {
    assert.ok(SOURCE_SNAPSHOT_PATHS.includes(path));
  }

  const fixture = await mkdtemp(
    join(tmpdir(), "dakia-snapshot-native-assets-"),
  );
  t.after(() => rm(fixture, { recursive: true, force: true }));
  execFileSync("git", ["init", "--quiet"], { cwd: fixture });
  const assets = [
    ["apps/desktop/src-tauri/testdata/fixture.txt", "testdata-v1"],
    ["apps/desktop/src-tauri/icons/tray-template.png", "icon-v1"],
    ["apps/desktop/src-tauri/resources/model.bin", "resource-v1"],
  ];
  for (const [relativePath, contents] of assets) {
    const target = join(fixture, relativePath);
    await mkdir(join(target, ".."), { recursive: true });
    await writeFile(target, contents);
  }

  const originalCwd = process.cwd();
  try {
    process.chdir(fixture);
    let snapshot = sourceSnapshot();
    assert.equal(snapshot.fileCount, assets.length);
    for (const [relativePath] of assets) {
      const target = join(fixture, relativePath);
      await writeFile(target, `${relativePath}-changed`);
      const changed = sourceSnapshot();
      assert.notEqual(changed.digest, snapshot.digest, relativePath);
      snapshot = changed;
    }
  } finally {
    process.chdir(originalCwd);
  }
});

test("three clean runs require each source gate three times", async () => {
  const seen = [];
  const report = await runMailSafety({
    repeat: 3,
    revision: "fixture",
    run: async (command, args) => {
      seen.push([command, args]);
      return { status: "passed", exitCode: 0, signal: null };
    },
  });
  assert.equal(report.status, "passed");
  assert.equal(report.runs.length, 3);
  assert.equal(seen.length, commandsFor("source").length * 3);
  assert.deepEqual(seen.slice(0, 3), commandsFor("source").slice(0, 3));
  assert.equal(report.revision, "fixture");
  assert.match(report.limitations.join(" "), /Native WebKit/);
});

test("failed or unavailable commands stop the sequence and never become a passing run", async () => {
  for (const outcome of [
    { status: "failed", exitCode: 1 },
    { status: "unavailable", exitCode: null },
    { status: "passed", exitCode: null, signal: "SIGTERM" },
  ]) {
    let calls = 0;
    const report = await runMailSafety({
      repeat: 3,
      run: async () => {
        calls += 1;
        return outcome;
      },
    });
    assert.notEqual(report.status, "passed");
    assert.equal(report.runs.length, 1);
    assert.equal(calls, 1);
    assert.ok(report.finishedAt);
  }
});

test("lanes cannot silently add live provider or release publishing operations", async () => {
  assert.throws(() => commandsFor("provider"));
  await assert.rejects(() => runMailSafety({ repeat: 0 }));
  await assert.rejects(() => runMailSafety({ repeat: 4 }));
  assert.deepEqual(commandsFor("deep"), [
    ["bash", ["scripts/mail-safety-servers.sh", "all"]],
    ["bash", ["scripts/run-mail-safety-fuzz.sh", "30"]],
  ]);
});

test("source lane runs the nonce-bound real MIME to Tauri to Reader artifact", async () => {
  assert.ok(
    commandsFor("source").some(
      ([command, args]) =>
        command === "node" &&
        args[0] === "scripts/test-mime-reader-artifact.mjs",
    ),
  );
  const script = await readFile(
    "scripts/test-mime-reader-artifact.mjs",
    "utf8",
  );
  assert.match(script, /randomUUID\(\)/);
  assert.match(script, /DAKIA_MIME_READER_ARTIFACT_NONCE/);
  assert.match(
    script,
    /mime_reader_handoff_tests::public_full_and_selective_reader_fetches_preserve_every_governed_reader_fixture/,
  );
  assert.match(script, /mime_reader_artifact_tests/);
  assert.match(script, /mimeReaderArtifact\.test\.tsx/);
  assert.match(script, /EXPECTED_FIXTURE_COUNT = EXPECTED_FIXTURE_IDS\.length/);
  assert.match(script, /numTotalTests !== EXPECTED_FIXTURE_COUNT/);
  assert.match(script, /standards-unknown-transfer-encoding/);
  assert.match(script, /numPendingTests !== 0/);
  assert.match(script, /rm\(root, \{ recursive: true, force: true \}\)/);
  assert.doesNotMatch(script, /https?:\/\//);
});

test("source lane validates the reviewed standards requirement matrix", () => {
  const plan = commandsFor("source");
  assert.equal(plan.length, 18);
  assert.ok(
    plan.some(
      ([command, args]) =>
        command === "node" &&
        args[0] === "scripts/validate-mail-standards-requirements.mjs",
    ),
  );
});

test("source lane requires the native fixture library suite after workspace Rust", async () => {
  const plan = commandsFor("source");
  const workspaceRustIndex = plan.findIndex(
    ([command, args]) =>
      command === "cargo" && args.join(" ") === "test --workspace --locked",
  );
  const nativeFixtureIndex = plan.findIndex(
    ([command, args]) =>
      command === "cargo" &&
      args.join(" ") ===
        "test --locked -p dakia-desktop --features native-mail-safety-fixture --lib",
  );

  assert.equal(nativeFixtureIndex, workspaceRustIndex + 1);

  const seen = [];
  const report = await runMailSafety({
    run: async (command, args) => {
      seen.push([command, args]);
      return seen.length === nativeFixtureIndex + 1
        ? { status: "failed", exitCode: 1, signal: null }
        : { status: "passed", exitCode: 0, signal: null };
    },
  });

  assert.equal(report.status, "failed");
  assert.equal(seen.length, nativeFixtureIndex + 1);
  assert.deepEqual(seen.at(-1), plan[nativeFixtureIndex]);
  assert.equal(report.runs[0].commands.at(-1).status, "failed");
});

test("source lane runs the fictional TLS attachment-command artifact", async () => {
  assert.ok(
    commandsFor("source").some(
      ([command, args]) =>
        command === "node" &&
        args[0] === "scripts/test-attachment-command-artifact.mjs",
    ),
  );
  const script = await readFile(
    "scripts/test-attachment-command-artifact.mjs",
    "utf8",
  );
  assert.match(script, /DAKIA_NATIVE_MAIL_SAFETY_FIXTURE/);
  assert.match(script, /attachment-command-fixture-server/);
  assert.match(script, /attachment_command_artifact_tests/);
  assert.match(script, /attachmentCommandArtifact\.test\.tsx/);
  assert.match(script, /rm\(root, \{ recursive: true, force: true \}\)/);
  assert.doesNotMatch(script, /https?:\/\//);
});

test("source lane runs the fictional TLS unsubscribe command boundary", () => {
  assert.ok(
    commandsFor("source").some(
      ([command, args]) =>
        command === "node" &&
        args[0] === "scripts/test-unsubscribe-command-tls.mjs",
    ),
  );
});

test("source lane runs the fictional TLS mail-action command boundary", async () => {
  assert.ok(
    commandsFor("source").some(
      ([command, args]) =>
        command === "node" && args[0] === "scripts/test-mail-command-tls.mjs",
    ),
  );
  const script = await readFile("scripts/test-mail-command-tls.mjs", "utf8");
  assert.match(script, /DAKIA_NATIVE_MAIL_SAFETY_FIXTURE/);
  assert.match(script, /mail-command-tls-fixture-server/);
  assert.match(script, /mail_command_tls_tests/);
  assert.match(script, /UID EXPUNGE 25/);
  assert.match(
    script,
    /stale command opened an unexpected fictional provider session/,
  );
  assert.match(script, /await stopFixture\(server\)/);
  assert.match(script, /rm\(root, \{ recursive: true, force: true \}\)/);
  assert.doesNotMatch(script, /https?:\/\//);
});
