import { spawn, execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  unlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { sourceSnapshot } from "./run-mail-safety.mjs";
import {
  FIXTURE,
  startLargeMailboxFixture,
} from "./large-mailbox-imap-fixture.mjs";
import { verifyLargeMailboxAudit } from "./verify-large-mailbox-imap-audit.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const ACCOUNT = "21c9e79c-c5b8-4d50-9d1e-6ba3d5323000";
const TEST_NAME = "large_mailbox_tls_smoke_tests::large_mailbox_tls_smoke";
const hash = async (path) =>
  createHash("sha256")
    .update(await readFile(path))
    .digest("hex");
const writeJson = (path, value) =>
  writeFile(path, `${JSON.stringify(value, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
export function requireSnapshot(snapshot) {
  if (
    !snapshot ||
    snapshot.algorithm !== "sha256-path-and-content-manifest" ||
    !/^[a-f0-9]{64}$/.test(snapshot.digest) ||
    !Number.isSafeInteger(snapshot.fileCount) ||
    snapshot.fileCount <= 0
  )
    throw new Error("governed source snapshot unavailable");
  return snapshot;
}
export function validateCliSearch(rows) {
  if (!Array.isArray(rows) || rows.length !== 1)
    throw new Error(
      "CLI search must return exactly one retained Reader message",
    );
  const row = rows[0];
  if (
    row.id !== `${ACCOUNT}:INBOX:2` ||
    row.account_id !== ACCOUNT ||
    row.mailbox !== "INBOX" ||
    row.uid !== 2 ||
    row.mailbox_uid_validity !== 77 ||
    row.content_state !== "complete" ||
    row.subject !== "Fictional retained multipart Reader message" ||
    row.body_text !== "" ||
    row.body_html !== null ||
    row.snippet !== "Fictional retained multipart text." ||
    row.is_read !== true ||
    row.is_flagged !== false ||
    row.has_attachments !== false
  )
    throw new Error("CLI local search catalogue projection differs");
  return {
    id: row.id,
    account: row.account_id,
    mailbox: row.mailbox,
    uid: row.uid,
    uidValidity: row.mailbox_uid_validity,
    contentState: row.content_state,
    bodyProjection: "metadata-catalogue",
    summaryBodyOmitted: true,
    snippetExact: true,
    isRead: row.is_read,
  };
}
export async function exclusiveRunDirectory(requested) {
  if (!requested) {
    const path = await mkdtemp(
      join(await realpath(tmpdir()), "dakia-large-mailbox-evidence-"),
    );
    await chmod(path, 0o700);
    return path;
  }
  if (!isAbsolute(requested)) throw new Error("evidence root must be absolute");
  const parent = await realpath(dirname(requested));
  const path = join(parent, basename(requested));
  await mkdir(path, { mode: 0o700 }); // EEXIST is deliberate: never reuse evidence.
  const stat = await lstat(path);
  if (
    !stat.isDirectory() ||
    stat.isSymbolicLink() ||
    stat.uid !== process.getuid() ||
    stat.mode & 0o077
  )
    throw new Error("evidence root must be private and owned");
  return path;
}

export async function boundedCommand({
  command,
  args,
  cwd,
  env,
  timeoutMs,
  stdoutPath,
  stderrPath,
  signal,
  maxOutputBytes = 64 * 1024 * 1024,
  spawnChild = spawn,
  sendOwnedSignal,
  ownedGroupExists,
  terminationGraceMs = 2000,
  terminationDeadlineMs = 4000,
}) {
  let child;
  let timedOut = false;
  let aborted = false;
  const stdout = [];
  const stderr = [];
  let outputBytes = 0;
  let closedYet = false;
  let escalation;
  let detachDeadline;
  let groupPoll;
  let terminationPromise;
  let resolveClosed;
  let primaryFailure;
  let ownershipUnresolved = false;
  let failure;
  try {
    if (signal?.aborted) throw new Error("run interrupted before phase launch");
    child = spawnChild(command, args, {
      cwd,
      env,
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    });
    // Each command owns the process group created by this exact spawn. Kill the
    // whole group on deadline so Cargo cannot leave compiler descendants alive.
    const groupExists = () => {
      if (ownedGroupExists) return ownedGroupExists(child);
      if (process.platform === "win32") return !closedYet;
      if (!child.pid) return false;
      try {
        process.kill(-child.pid, 0);
        return true;
      } catch (error) {
        if (error.code === "ESRCH") return false;
        failure ??= error;
        return true;
      }
    };
    const terminate = () => {
      if (terminationPromise) return;
      let resolveTermination;
      terminationPromise = new Promise((resolve) => {
        resolveTermination = resolve;
      });
      const send = (signal) => {
        try {
          if (sendOwnedSignal) sendOwnedSignal(child, signal);
          else
            process.platform === "win32"
              ? child.kill(signal)
              : process.kill(-child.pid, signal);
        } catch (error) {
          if (error.code !== "ESRCH") failure ??= error;
        }
      };
      const finish = () => {
        clearInterval(groupPoll);
        clearTimeout(escalation);
        clearTimeout(detachDeadline);
        resolveTermination();
      };
      send("SIGTERM");
      // A leader exiting on TERM does not prove its descendants exited.
      escalation = setTimeout(() => {
        if (groupExists()) send("SIGKILL");
      }, terminationGraceMs);
      groupPoll = setInterval(() => {
        if (!groupExists() && closedYet) finish();
      }, 20);
      detachDeadline = setTimeout(() => {
        ownershipUnresolved = groupExists() || !closedYet;
        if (ownershipUnresolved) {
          failure ??= new Error(
            "owned command failed to close after termination",
          );
          child.stdout.destroy();
          child.stderr.destroy();
          child.unref();
          resolveClosed({ code: null, signal: "ownership-timeout" });
        }
        finish();
      }, terminationDeadlineMs);
    };
    const collect = (chunks, chunk) => {
      const remaining = Math.max(0, maxOutputBytes - outputBytes);
      outputBytes += chunk.length;
      if (remaining) chunks.push(chunk.subarray(0, remaining));
      if (outputBytes > maxOutputBytes) {
        failure ??= new Error(
          `phase output exceeded ${maxOutputBytes}-byte bound; logs truncated`,
        );
        terminate();
      }
    };
    child.stdout.on("data", (chunk) => collect(stdout, chunk));
    child.stderr.on("data", (chunk) => collect(stderr, chunk));
    const closed = new Promise((resolve) => {
      resolveClosed = resolve;
      child.once("error", (error) => {
        failure = error;
      });
      child.once("close", (code, exitSignal) => {
        closedYet = true;
        resolveClosed({ code, signal: exitSignal });
      });
    });
    const stop = () => {
      aborted = true;
      terminate();
    };
    signal?.addEventListener("abort", stop, { once: true });
    const timer = setTimeout(() => {
      timedOut = true;
      terminate();
    }, timeoutMs);
    if (signal?.aborted) stop();
    let result;
    try {
      result = await closed;
      if (!terminationPromise && groupExists()) {
        failure ??= new Error("owned command left live descendants");
        terminate();
      }
      if (terminationPromise) await terminationPromise;
    } finally {
      clearTimeout(timer);
      clearTimeout(escalation);
      clearTimeout(detachDeadline);
      clearInterval(groupPoll);
      signal?.removeEventListener("abort", stop);
    }
    if (failure || timedOut || aborted || result.code !== 0) {
      const error =
        failure ??
        new Error(
          timedOut
            ? "phase deadline exceeded"
            : aborted
              ? "run interrupted"
              : `command exited ${result.code ?? result.signal}`,
        );
      error.phaseResult = { ...result, timedOut, aborted, ownershipUnresolved };
      throw error;
    }
    return {
      ...result,
      timedOut,
      aborted,
      stdout: Buffer.concat(stdout).toString("utf8"),
    };
  } catch (error) {
    primaryFailure = error;
    throw error;
  } finally {
    // The owned group has either closed or exhausted TERM/KILL deadlines.
    // Never wait again on an unresolved child or skip logs because cleanup failed.
    const saved = await Promise.allSettled([
      writeFile(stdoutPath, Buffer.concat(stdout), { mode: 0o600, flag: "wx" }),
      writeFile(stderrPath, Buffer.concat(stderr), { mode: 0o600, flag: "wx" }),
    ]);
    const failures = saved
      .filter((result) => result.status === "rejected")
      .map((result) => String(result.reason.message));
    if (failures.length) {
      if (primaryFailure) primaryFailure.artifactFailures = failures;
      else {
        const error = new Error("phase log preservation failed");
        error.artifactFailures = failures;
        throw error;
      }
    }
  }
}

function buildArtifact(stdout, name, test) {
  const artifacts = stdout
    .split("\n")
    .filter(Boolean)
    .flatMap((line) => {
      try {
        return [JSON.parse(line)];
      } catch {
        return [];
      }
    })
    .filter(
      (value) =>
        value.reason === "compiler-artifact" &&
        value.target?.name === name &&
        Boolean(value.profile?.test) === test &&
        value.executable,
    );
  if (artifacts.length !== 1 || !isAbsolute(artifacts[0].executable))
    throw new Error(`missing unambiguous built ${name} executable`);
  return artifacts[0].executable;
}

export async function runLargeMailboxSmoke(options = {}) {
  const root = options.root ?? ROOT;
  const snapshot = options.snapshot ?? sourceSnapshot;
  const commandRunner = options.commandRunner ?? boundedCommand;
  const fixtureFactory = options.fixtureFactory ?? startLargeMailboxFixture;
  const env = { ...process.env, ...options.env };
  const evidence = await exclusiveRunDirectory(
    options.evidenceRoot ?? env.DAKIA_LARGE_MAILBOX_EVIDENCE_ROOT,
  );
  const receipt = {
    schemaVersion: 1,
    status: "failed",
    evidenceRoot: evidence,
    startedAt: new Date().toISOString(),
    phases: [],
    finalizationFailures: [],
    tools: { node: { version: process.version, executable: process.execPath } },
    limitations: [
      "Test-only loopback CA injection shares production TLS client, authentication, synchronization, Store and Reader MIME core; production endpoint construction/WebPKI policy is not exercised.",
      "CLI local search proves the metadata catalogue projection, identity, UID epoch, complete content state and snippet. It deliberately omits cached body bytes. The Rust smoke separately proves exact reopened Reader cache bytes. CLI show uses public trust and is outside this fixture.",
      "This is not native WebKit or large-mailbox UI acceptance.",
      "The default run-owned target cache is retained for reproducibility and may consume multiple GiB. No existing profile, cache, or evidence is removed.",
      "Source digest binds governed file bytes. Revision and dirty status are separate provenance and do not assert a clean checkout.",
    ],
  };
  let fixture;
  const sessions = [];
  let problem;
  const controller = new AbortController();
  const onSignal = (signal) => {
    receipt.signal = signal;
    controller.abort();
  };
  const signals = new Map(
    ["SIGINT", "SIGTERM"].map((signal) => [signal, () => onSignal(signal)]),
  );
  for (const [signal, handler] of signals) process.on(signal, handler);
  const provenance =
    options.provenance ??
    (() => ({
      revision: execFileSync("git", ["rev-parse", "HEAD"], {
        cwd: root,
        encoding: "utf8",
      }).trim(),
      dirty: execFileSync("git", ["status", "--porcelain=v1"], {
        cwd: root,
        encoding: "utf8",
      }),
    }));
  const takeSnapshot = () => {
    const before = process.cwd();
    try {
      process.chdir(root);
      return requireSnapshot(snapshot());
    } finally {
      process.chdir(before);
    }
  };
  const phase = async (name, command, args, timeoutMs) => {
    if (controller.signal.aborted) throw new Error("run interrupted");
    const entry = {
      name,
      command,
      args,
      timeoutMs,
      startedAt: new Date().toISOString(),
      status: "failed",
    };
    receipt.phases.push(entry);
    try {
      const result = await commandRunner({
        command,
        args,
        cwd: root,
        env,
        timeoutMs,
        stdoutPath: join(evidence, `${name}.stdout`),
        stderrPath: join(evidence, `${name}.stderr`),
        signal: controller.signal,
      });
      entry.status = "passed";
      return result;
    } catch (error) {
      entry.failure = String(error.message);
      entry.result = error.phaseResult ?? null;
      entry.artifactFailures = error.artifactFailures ?? [];
      throw error;
    } finally {
      entry.finishedAt = new Date().toISOString();
    }
  };
  try {
    receipt.sourceBefore = takeSnapshot();
    receipt.provenanceBefore = provenance();
    await writeJson(join(evidence, "source-before.json"), receipt.sourceBefore);
    if (env.DAKIA_LARGE_MAILBOX_SMOKE_ROOT)
      throw new Error(
        "external profile roots are unsupported; this runner creates its own retained profile",
      );
    for (const [tool, args] of [
      ["cargo", ["--version", "--verbose"]],
      ["rustc", ["-vV"]],
      ["openssl", ["version", "-a"]],
      ["git", ["--version"]],
    ]) {
      receipt.tools[tool] = (
        await phase(`tool-${tool}`, tool, args, 30000)
      ).stdout.trim();
    }
    const profile = join(evidence, "profile");
    await mkdir(profile, { mode: 0o700 });
    env.DAKIA_LARGE_MAILBOX_SMOKE_ROOT = profile;
    env.CARGO_TARGET_DIR = env.DAKIA_LARGE_MAILBOX_TARGET_DIR
      ? await realpath(env.DAKIA_LARGE_MAILBOX_TARGET_DIR)
      : join(evidence, "target");
    receipt.profileRoot = profile;
    receipt.targetDirectory = env.CARGO_TARGET_DIR;
    await phase(
      "ca",
      "openssl",
      [
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        join(evidence, "ca.key"),
        "-out",
        join(evidence, "ca.crt"),
        "-days",
        "1",
        "-subj",
        "/CN=Dakia-large-mailbox-fixture-CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
      ],
      30000,
    );
    await phase(
      "csr",
      "openssl",
      [
        "req",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        join(evidence, "tls.key"),
        "-out",
        join(evidence, "tls.csr"),
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=DNS:localhost,IP:127.0.0.1",
        "-addext",
        "basicConstraints=critical,CA:FALSE",
        "-addext",
        "keyUsage=critical,digitalSignature,keyEncipherment",
      ],
      30000,
    );
    await phase(
      "certificate",
      "openssl",
      [
        "x509",
        "-req",
        "-in",
        join(evidence, "tls.csr"),
        "-CA",
        join(evidence, "ca.crt"),
        "-CAkey",
        join(evidence, "ca.key"),
        "-CAcreateserial",
        "-out",
        join(evidence, "tls.crt"),
        "-days",
        "1",
        "-copy_extensions",
        "copy",
      ],
      30000,
    );
    await phase(
      "ca-der",
      "openssl",
      [
        "x509",
        "-in",
        join(evidence, "ca.crt"),
        "-outform",
        "DER",
        "-out",
        join(evidence, "ca.der"),
      ],
      30000,
    );
    const core = await phase(
      "build-core",
      "cargo",
      [
        "test",
        "-p",
        "dakia-core",
        "--lib",
        "--locked",
        "--no-run",
        "--message-format=json",
      ],
      600000,
    );
    const cli = await phase(
      "build-cli",
      "cargo",
      [
        "build",
        "-p",
        "dakia-cli",
        "--bin",
        "dakia",
        "--locked",
        "--message-format=json",
      ],
      600000,
    );
    await mkdir(join(evidence, "bin"), { mode: 0o700 });
    receipt.executables = [];
    for (const [role, built] of [
      ["core", buildArtifact(core.stdout, "dakia_core", true)],
      ["cli", buildArtifact(cli.stdout, "dakia", false)],
    ]) {
      const stat = await lstat(built);
      if (!stat.isFile() || stat.isSymbolicLink())
        throw new Error("built artifact must be a regular executable");
      const frozen = join(evidence, "bin", role);
      await copyFile(built, frozen, 1);
      await chmod(frozen, 0o700);
      const sha256 = await hash(frozen);
      if ((await hash(built)) !== sha256)
        throw new Error("build artifact changed during freezing");
      receipt.executables.push({
        role,
        builtPath: built,
        executedPath: frozen,
        sha256,
      });
    }
    fixture = await fixtureFactory({
      keyPath: join(evidence, "tls.key"),
      certPath: join(evidence, "tls.crt"),
      onSession: (session) => sessions.push(session),
    });
    Object.assign(env, {
      DAKIA_INDEPENDENT_SERVER_HOST: "127.0.0.1",
      DAKIA_INDEPENDENT_SERVER_PORT: String(fixture.port),
      DAKIA_INDEPENDENT_SERVER_NAME: "localhost",
      DAKIA_INDEPENDENT_CA_DER_PATH: join(evidence, "ca.der"),
      DAKIA_INDEPENDENT_USERNAME: FIXTURE.account,
      DAKIA_INDEPENDENT_PASSWORD: FIXTURE.password,
    });
    await phase(
      "smoke",
      receipt.executables[0].executedPath,
      [TEST_NAME, "--ignored", "--exact", "--nocapture"],
      120000,
    );
    const cliResult = await phase(
      "cli-search",
      receipt.executables[1].executedPath,
      [
        "--data-dir",
        profile,
        "--json",
        "search",
        "Fictional retained multipart Reader message",
        "--account",
        ACCOUNT,
      ],
      30000,
    );
    const smoke = JSON.parse(
      await readFile(join(profile, "large-mailbox-tls-smoke.json"), "utf8"),
    );
    if (
      smoke.account_id !== ACCOUNT ||
      smoke.message_count !== 30000 ||
      smoke.uid_validity !== 77 ||
      smoke.reader_uid !== 2 ||
      smoke.reader_body_text !== "Fictional retained multipart text.\r\n"
    )
      throw new Error("Rust smoke receipt differs");
    receipt.cliSearch = validateCliSearch(JSON.parse(cliResult.stdout));
    receipt.rustSmoke = smoke;
  } catch (error) {
    problem = error;
  } finally {
    try {
      if (fixture) await fixture.close();
    } catch (error) {
      receipt.finalizationFailures.push(String(error.message));
      problem ??= error;
    }
    try {
      const audit = { schemaVersion: 2, fixture: "fictional-30000", sessions };
      await writeJson(join(evidence, "imap-audit.json"), audit);
      try {
        receipt.audit = verifyLargeMailboxAudit(audit);
        receipt.auditStatus = "passed";
      } catch (error) {
        receipt.auditStatus = "failed";
        throw error;
      }
    } catch (error) {
      receipt.finalizationFailures.push(String(error.message));
      problem ??= error;
    }
    try {
      receipt.sourceAfter = takeSnapshot();
      receipt.provenanceAfter = provenance();
      await writeJson(join(evidence, "source-after.json"), receipt.sourceAfter);
      if (
        JSON.stringify(receipt.sourceBefore) !==
          JSON.stringify(receipt.sourceAfter) ||
        receipt.provenanceBefore?.revision !==
          receipt.provenanceAfter.revision ||
        receipt.provenanceBefore?.dirty !== receipt.provenanceAfter.dirty
      )
        throw new Error(
          "source or revision/dirty provenance drift during smoke",
        );
      for (const executable of receipt.executables ?? [])
        if ((await hash(executable.executedPath)) !== executable.sha256)
          throw new Error("executed binary changed during smoke");
    } catch (error) {
      receipt.finalizationFailures.push(String(error.message));
      problem ??= error;
    }
    for (const privateKey of ["ca.key", "tls.key"]) {
      try {
        const path = join(evidence, privateKey);
        const stat = await lstat(path);
        if (
          !stat.isFile() ||
          stat.isSymbolicLink() ||
          stat.uid !== process.getuid()
        )
          throw new Error("private fixture key ownership changed");
        await unlink(path);
      } catch (error) {
        if (error.code !== "ENOENT") {
          receipt.finalizationFailures.push(String(error.message));
          problem ??= error;
        }
      }
    }
    for (const [signal, handler] of signals) process.off(signal, handler);
    if (controller.signal.aborted) problem ??= new Error("run interrupted");
    receipt.status = problem ? "failed" : "passed";
    receipt.failure = problem ? String(problem.message) : null;
    receipt.finishedAt = new Date().toISOString();
    await writeJson(join(evidence, "receipt.json"), receipt);
  }
  return receipt;
}
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  if (process.argv.length !== 2)
    throw new Error("usage: run-large-mailbox-tls-smoke.sh");
  const receipt = await runLargeMailboxSmoke();
  process.stdout.write(
    `${JSON.stringify({ status: receipt.status, evidenceRoot: receipt.evidenceRoot, failure: receipt.failure })}\n`,
  );
  process.exitCode = receipt.status === "passed" ? 0 : 1;
}
