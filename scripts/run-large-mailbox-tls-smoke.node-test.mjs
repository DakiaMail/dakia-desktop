import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import {
  boundedCommand,
  exclusiveRunDirectory,
  requireSnapshot,
  runLargeMailboxSmoke,
  validateCliSearch,
} from "./run-large-mailbox-tls-smoke.mjs";
import { FIXTURE, LargeMailboxSession } from "./large-mailbox-imap-fixture.mjs";
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SNAPSHOT = {
  algorithm: "sha256-path-and-content-manifest",
  digest: "a".repeat(64),
  fileCount: 1000,
};
const ACCOUNT = "21c9e79c-c5b8-4d50-9d1e-6ba3d5323000";
const capturedRoot = new URL("./testdata/large-mailbox/", import.meta.url);
const capturedRows = JSON.parse(
  await readFile(new URL("cli-search.json", capturedRoot), "utf8"),
);
const ROW = capturedRows[0];
const CAPTURED_RUST_SMOKE = JSON.parse(
  await readFile(new URL("rust-smoke.json", capturedRoot), "utf8"),
);
const READER_BODY = "Fictional retained multipart text.\r\n";
const PRIVATE = { mode: 0o600, flag: "wx" };
async function temporary(t) {
  const dir = await mkdtemp(join(tmpdir(), "dakia-load-runner-test-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}
function transcript(reader = false) {
  const session = new LargeMailboxSession();
  const send = (command) => session.receive(`x ${command}\r\n`);
  send("CAPABILITY");
  send(`LOGIN "${FIXTURE.account}" "${FIXTURE.password}"`);
  if (!reader) {
    send("CAPABILITY");
    send('LIST "" "*"');
  }
  send('SELECT "INBOX"');
  if (reader) {
    send(
      "UID FETCH 2 (FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT)])",
    );
    send("UID FETCH 2 (BODY.PEEK[1.MIME])");
    send("UID FETCH 2 (BODY.PEEK[1])");
  } else {
    for (let page = 0; page < 60; page += 1) {
      const end = 30000 - page * 500;
      send(`FETCH ${end - 499}:${end} (UID FLAGS)`);
    }
    for (let batch = 0; batch < 600; batch += 1) {
      send('SELECT "INBOX"');
      for (const fields of [
        "FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT)]",
        "BODY.PEEK[]<0.8192>",
      ])
        for (let half = 0; half < 2; half += 1)
          send(
            `UID FETCH ${Array.from({ length: 25 }, (_, i) => batch * 50 + half * 25 + i + 1).join(",")} (${fields})`,
          );
    }
    send('SELECT "INBOX"');
  }
  send("LOGOUT");
  return { tlsProtocol: "TLSv1.3", events: session.transcript() };
}

test("source snapshot validation fails closed", () => {
  for (const value of [
    null,
    {},
    { ...SNAPSHOT, digest: null },
    { ...SNAPSHOT, fileCount: 0 },
    { ...SNAPSHOT, algorithm: "unknown" },
  ])
    assert.throws(() => requireSnapshot(value));
  assert.equal(requireSnapshot(SNAPSHOT), SNAPSHOT);
});
test("requires fresh absolute private evidence and never reuses existing paths", async (t) => {
  const dir = await temporary(t);
  await assert.rejects(exclusiveRunDirectory("relative"));
  const fresh = await exclusiveRunDirectory(join(dir, "fresh"));
  assert.equal((await lstat(fresh)).mode & 0o777, 0o700);
  await assert.rejects(exclusiveRunDirectory(fresh), /EEXIST/);
  const publicDir = join(dir, "public");
  await mkdir(publicDir);
  await chmod(publicDir, 0o755);
  await assert.rejects(exclusiveRunDirectory(publicDir), /EEXIST/);
});
test("CLI proof accepts the captured metadata-only catalogue row and rejects invented cached body projection", () => {
  const proof = validateCliSearch(capturedRows);
  assert.equal(proof.bodyProjection, "metadata-catalogue");
  assert.equal(proof.summaryBodyOmitted, true);
  for (const patch of [
    { account_id: "other" },
    { uid: 3 },
    { mailbox: "Sent" },
    { mailbox_uid_validity: 78 },
    { content_state: "partial" },
    { body_text: READER_BODY },
    { body_text: "wrong" },
    { body_html: "unexpected" },
    { snippet: "wrong" },
    { is_read: false },
    { is_flagged: true },
    { has_attachments: true },
  ])
    assert.throws(() => validateCliSearch([{ ...ROW, ...patch }]));
});

for (const variant of ["exit", "timeout", "abort", "output"])
  test(`bounded command preserves logs and rejects ${variant}`, async (t) => {
    const dir = await temporary(t);
    const controller = new AbortController();
    const script =
      variant === "exit"
        ? 'console.log("retained-out"); console.error("retained-err"); process.exit(7)'
        : variant === "output"
          ? 'console.log("retained-out".repeat(100)); setInterval(()=>{},1000)'
          : 'console.log("retained-out"); console.error("retained-err"); setInterval(()=>{},1000)';
    if (variant === "abort") setTimeout(() => controller.abort(), 100);
    await assert.rejects(
      boundedCommand({
        command: process.execPath,
        args: ["-e", script],
        cwd: dir,
        env: process.env,
        timeoutMs: variant === "timeout" ? 100 : 2000,
        stdoutPath: join(dir, "out"),
        stderrPath: join(dir, "err"),
        signal: controller.signal,
        maxOutputBytes: variant === "output" ? 100 : 100000,
      }),
    );
    if (variant !== "output")
      assert.match(await readFile(join(dir, "out"), "utf8"), /retained-out/);
    assert.equal((await lstat(join(dir, "out"))).mode & 0o777, 0o600);
    assert.equal((await lstat(join(dir, "err"))).mode & 0o777, 0o600);
  });

async function mockRun(t, variant) {
  const dir = await temporary(t);
  const evidence = join(dir, "evidence");
  let snapshots = 0;
  let phaseCount = 0;
  let fixtureClosed = false;
  const commandRunner = async (context) => {
    phaseCount += 1;
    const phase = context.stdoutPath.split("/").at(-1).replace(".stdout", "");
    const evidenceRoot = dirname(context.stdoutPath);
    let stdout = "tool-version\n";
    if (phase === "ca")
      await writeFile(
        join(evidenceRoot, "ca.key"),
        "fictional-ca-key",
        PRIVATE,
      );
    if (phase === "csr")
      await writeFile(
        join(evidenceRoot, "tls.key"),
        "fictional-server-key",
        PRIVATE,
      );
    if (phase.startsWith("build-")) {
      const role = phase === "build-core" ? "core" : "cli";
      const executable = join(evidenceRoot, `built-${role}`);
      await writeFile(executable, role, PRIVATE);
      stdout = `${JSON.stringify({ reason: "compiler-artifact", target: { name: role === "core" ? "dakia_core" : "dakia" }, profile: { test: role === "core" }, executable })}\n`;
    }
    if (phase === "smoke") {
      assert.equal(context.timeoutMs, 120000);
      await writeFile(
        join(
          context.env.DAKIA_LARGE_MAILBOX_SMOKE_ROOT,
          "large-mailbox-tls-smoke.json",
        ),
        JSON.stringify({
          ...CAPTURED_RUST_SMOKE,
          reader_body_text:
            variant === "reader-body-drift"
              ? READER_BODY.trim()
              : CAPTURED_RUST_SMOKE.reader_body_text,
        }),
        PRIVATE,
      );
      if (variant === "binary-drift")
        await writeFile(context.command, "altered frozen binary");
    }
    if (phase === "cli-search") {
      assert.equal(context.timeoutMs, 30000);
      stdout = JSON.stringify([ROW]);
    }
    await writeFile(context.stdoutPath, stdout, PRIVATE);
    await writeFile(context.stderrPath, "retained-phase-error", PRIVATE);
    if (variant === "command-failure" && phase === "ca")
      throw new Error("controlled command failure");
    if (variant === "interrupted" && phase === "ca") {
      process.emit("SIGTERM");
      throw new Error("controlled interruption");
    }
    return { stdout };
  };
  const fixtureFactory = async ({ onSession }) => ({
    port: 12345,
    close: async () => {
      fixtureClosed = true;
      for (const reader of [false, true]) {
        const session = transcript(reader);
        if (variant === "audit-corruption" && reader)
          session.events.push({ kind: "command", command: "UID STORE" });
        onSession(session);
      }
    },
  });
  const originalCwd = process.cwd();
  process.chdir(dir);
  let result;
  try {
    result = await runLargeMailboxSmoke({
      root: ROOT,
      evidenceRoot: evidence,
      provenance: () => ({
        revision: "a".repeat(40),
        dirty: "fictional-dirty-cohort",
      }),
      env: {
        DAKIA_LARGE_MAILBOX_SMOKE_ROOT: "",
        DAKIA_LARGE_MAILBOX_TARGET_DIR: "",
      },
      snapshot: () => {
        assert.equal(process.cwd(), ROOT);
        snapshots += 1;
        return variant === "null-source"
          ? null
          : {
              ...SNAPSHOT,
              digest:
                variant === "source-drift" && snapshots > 1
                  ? "b".repeat(64)
                  : SNAPSHOT.digest,
            };
      },
      commandRunner,
      fixtureFactory,
    });
  } finally {
    process.chdir(originalCwd);
  }
  assert.deepEqual(
    JSON.parse(await readFile(join(evidence, "receipt.json"), "utf8")),
    result,
  );
  assert.equal(
    (await lstat(join(evidence, "receipt.json"))).mode & 0o777,
    0o600,
  );
  assert.equal(result.status, variant === "success" ? "passed" : "failed");
  assert.equal(snapshots, 2);
  await assert.rejects(readFile(join(evidence, "ca.key")), /ENOENT/);
  await assert.rejects(readFile(join(evidence, "tls.key")), /ENOENT/);
  assert.equal(
    Array.isArray(
      JSON.parse(await readFile(join(evidence, "imap-audit.json"), "utf8"))
        .sessions,
    ),
    true,
  );
  if (variant === "success") {
    assert(fixtureClosed);
    assert.equal(result.cliSearch.summaryBodyOmitted, true);
    assert.equal(result.rustSmoke.reader_body_text, READER_BODY);
    assert.equal(result.executables.length, 2);
    assert.equal(result.tools.cargo, "tool-version");
  }
  if (variant === "null-source") assert.equal(phaseCount, 0);
  if (variant === "interrupted") assert.equal(result.signal, "SIGTERM");
  return result;
}
for (const variant of [
  "success",
  "null-source",
  "command-failure",
  "source-drift",
  "audit-corruption",
  "binary-drift",
  "reader-body-drift",
  "interrupted",
])
  test(`runner retains ${variant} receipt from unrelated working directory`, async (t) => {
    await mockRun(t, variant);
  });

test("checked-in shell wrapper resolves repository from unrelated working directory", async (t) => {
  const dir = await temporary(t);
  let error;
  try {
    execFileSync(
      "sh",
      [join(ROOT, "scripts/run-large-mailbox-tls-smoke.sh"), "unexpected"],
      { cwd: dir, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    );
  } catch (caught) {
    error = caught;
  }
  assert.equal(error.status, 1);
  assert.match(error.stderr, /usage: run-large-mailbox-tls-smoke.sh/);
  assert.doesNotMatch(error.stderr, /ERR_MODULE_NOT_FOUND/);
});

test("deadline kills owned descendants and escalates when SIGTERM is ignored", async (t) => {
  const dir = await temporary(t);
  const childScript = 'process.on("SIGTERM",()=>{}); setInterval(()=>{},1000)';
  const script = `process.on("SIGTERM",()=>{}); const child=require("node:child_process").spawn(process.execPath,["-e", ${JSON.stringify(childScript)}],{stdio:"ignore"}); console.log(child.pid); setInterval(()=>{},1000)`;
  const started = Date.now();
  let failure;
  try {
    await boundedCommand({
      command: process.execPath,
      args: ["-e", script],
      cwd: dir,
      env: process.env,
      timeoutMs: 100,
      stdoutPath: join(dir, "out"),
      stderrPath: join(dir, "err"),
    });
  } catch (error) {
    failure = error;
  }
  assert.match(
    String(failure?.message),
    /deadline/,
    await readFile(join(dir, "err"), "utf8"),
  );
  assert(Date.now() - started < 8000);
  const descendant = Number((await readFile(join(dir, "out"), "utf8")).trim());
  assert(Number.isSafeInteger(descendant));
  // A PID obtained from our own child's output is checked only, never signalled.
  await new Promise((r) => setTimeout(r, 100));
  assert.throws(() => process.kill(descendant, 0), /ESRCH/);
});

test("deadline kills descendants even when their leader exits on SIGTERM", async (t) => {
  const dir = await temporary(t);
  const childScript = 'process.on("SIGTERM",()=>{}); setInterval(()=>{},1000)';
  const script = `const child=require("node:child_process").spawn(process.execPath,["-e", ${JSON.stringify(childScript)}],{stdio:"ignore"}); console.log(child.pid); setInterval(()=>{},1000)`;
  const started = Date.now();
  let failure;
  try {
    await boundedCommand({
      command: process.execPath,
      args: ["-e", script],
      cwd: dir,
      env: process.env,
      timeoutMs: 100,
      stdoutPath: join(dir, "out"),
      stderrPath: join(dir, "err"),
    });
  } catch (error) {
    failure = error;
  }
  assert.match(
    String(failure?.message),
    /deadline/,
    await readFile(join(dir, "err"), "utf8"),
  );
  assert(Date.now() - started < 8000);
  const descendant = Number((await readFile(join(dir, "out"), "utf8")).trim());
  assert(Number.isSafeInteger(descendant));
  // A PID obtained from our own child's output is checked only, never signalled.
  await new Promise((r) => setTimeout(r, 100));
  assert.throws(() => process.kill(descendant, 0), /ESRCH/);
});

test("missing child close cannot block termination deadline or lose retained logs", async (t) => {
  const { EventEmitter } = await import("node:events");
  const { PassThrough } = await import("node:stream");
  const dir = await temporary(t);
  const sent = [];
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.unref = () => {
    child.unreferenced = true;
  };
  const started = Date.now();
  let failure;
  try {
    await boundedCommand({
      command: "mock-owned-child",
      args: [],
      cwd: dir,
      env: {},
      timeoutMs: 10,
      stdoutPath: join(dir, "out"),
      stderrPath: join(dir, "err"),
      terminationGraceMs: 20,
      terminationDeadlineMs: 40,
      spawnChild: () => {
        setImmediate(() => {
          child.stdout.write("retained-out");
          child.stderr.write("retained-err");
        });
        return child;
      },
      sendOwnedSignal: (_, signal) => sent.push(signal),
      ownedGroupExists: () => true,
    });
  } catch (error) {
    failure = error;
  }
  assert(Date.now() - started < 1000);
  assert.match(failure.message, /failed to close/);
  assert.equal(failure.phaseResult.ownershipUnresolved, true);
  assert.deepEqual(sent, ["SIGTERM", "SIGKILL"]);
  assert(child.unreferenced);
  assert.equal(await readFile(join(dir, "out"), "utf8"), "retained-out");
  assert.equal(await readFile(join(dir, "err"), "utf8"), "retained-err");
});

test("missing child close remains bounded when its process group is already gone", async (t) => {
  const { EventEmitter } = await import("node:events");
  const { PassThrough } = await import("node:stream");
  const dir = await temporary(t);
  const sent = [];
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.unref = () => {
    child.unreferenced = true;
  };
  const started = Date.now();
  let failure;
  try {
    await boundedCommand({
      command: "mock-owned-child",
      args: [],
      cwd: dir,
      env: {},
      timeoutMs: 10,
      stdoutPath: join(dir, "out"),
      stderrPath: join(dir, "err"),
      terminationGraceMs: 20,
      terminationDeadlineMs: 40,
      spawnChild: () => {
        setImmediate(() => {
          child.stdout.write("retained-out");
          child.stderr.write("retained-err");
        });
        return child;
      },
      sendOwnedSignal: (_, signal) => sent.push(signal),
      ownedGroupExists: () => false,
    });
  } catch (error) {
    failure = error;
  }
  assert(Date.now() - started < 1000);
  assert.match(failure.message, /failed to close/);
  assert.equal(failure.phaseResult.ownershipUnresolved, true);
  assert.deepEqual(sent, ["SIGTERM"]);
  assert(child.unreferenced);
  assert.equal(await readFile(join(dir, "out"), "utf8"), "retained-out");
  assert.equal(await readFile(join(dir, "err"), "utf8"), "retained-err");
});
