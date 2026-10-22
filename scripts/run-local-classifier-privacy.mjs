#!/usr/bin/env node

import { createServer } from "node:net";
import { cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const repository = resolve(fileURLToPath(new URL("..", import.meta.url)));
const canary = "classifier-privacy-canary-20261020";
const marker = "classifier-privacy-inference-complete";
const testName =
  "classification::model_integration_tests::local_model_privacy_canary_inference_succeeds_without_emitting_message_data";
const resultPath = process.argv[2]
  ? resolve(process.argv[2])
  : join(repository, "docs/mail-safety-results/local-classifier-privacy.json");

export function validateResult(candidate) {
  const failures = [];
  if (candidate?.status !== "passed") failures.push("status");
  if (candidate?.positiveControl?.unsandboxedLoopbackConnection !== true) {
    failures.push("positive control");
  }
  if (
    candidate?.probe?.denied !== true ||
    ![1, 13].includes(candidate?.probe?.explicitErrno) ||
    candidate?.probe?.exitCode !== 0 ||
    candidate?.probe?.timedOut === true
  ) {
    failures.push("network denial probe");
  }
  const counts = candidate?.classifier?.exactHarnessCounts;
  if (
    candidate?.classifier?.inferenceSucceeded !== true ||
    counts?.passed !== 1 ||
    counts?.failed !== 0 ||
    counts?.ignored !== 0 ||
    candidate?.classifier?.postInferenceMarkerObserved !== true ||
    candidate?.classifier?.canaryAbsentFromOutput !== true
  ) {
    failures.push("exact classifier inference");
  }
  return failures;
}

if (process.argv[2] === "--validate-result") {
  let candidate;
  try {
    candidate = JSON.parse(await readFile(resolve(process.argv[3]), "utf8"));
  } catch (error) {
    process.stderr.write(`could not read classifier privacy result: ${String(error)}\n`);
    process.exit(1);
  }
  const failures = validateResult(candidate);
  if (failures.length > 0) {
    process.stderr.write(`invalid classifier privacy result: ${failures.join(", ")}\n`);
    process.exit(1);
  }
  process.stdout.write("classifier privacy result validated\n");
  process.exit(0);
}

const run = (command, args, options = {}) =>
  new Promise((resolvePromise) => {
    const child = spawn(command, args, {
      cwd: repository,
      env: { ...process.env, ...options.env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stdout = [];
    const stderr = [];
    let timedOut = false;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, options.timeoutMs ?? 300_000);
    child.stdout.on("data", (chunk) => stdout.push(chunk));
    child.stderr.on("data", (chunk) => stderr.push(chunk));
    child.on("error", (error) => {
      clearTimeout(timeout);
      resolvePromise({
        code: null,
        signal: null,
        timedOut,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.from(String(error)),
      });
    });
    child.on("close", (code, signal) => {
      clearTimeout(timeout);
      resolvePromise({
        code,
        signal,
        timedOut,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.concat(stderr),
      });
    });
  });

const sandboxProfile = (targetDirectory, artifactDirectory) => `(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow sysctl-read)
(allow mach-lookup)
(allow file-read*)
(allow file-write* (subpath "${targetDirectory}"))
(allow file-write* (subpath "${artifactDirectory}"))
`;

const server = createServer();
let connectionCount = 0;
server.on("connection", (socket) => {
  connectionCount += 1;
  socket.destroy();
});

let workDirectory;
let result;
try {
  await new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolvePromise);
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("could not obtain the loopback control port");
  }

  const probeCode = [
    "import socket, sys",
    "s = socket.socket()",
    `s.settimeout(2); s.connect(('127.0.0.1', ${address.port}))`,
    "sys.exit(0)",
  ].join("; ");
  const unsandboxedProbe = await run("/usr/bin/python3", ["-c", probeCode]);
  await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
  const positiveControl = unsandboxedProbe.code === 0 && connectionCount === 1;
  if (!positiveControl) {
    throw new Error("the unsandboxed loopback positive control did not connect");
  }

  workDirectory = await mkdtemp("/private/tmp/dakia-classifier-privacy-");
  const targetDirectory = join(workDirectory, "target");
  const profilePath = join(workDirectory, "network-denied.sb");
  await writeFile(profilePath, sandboxProfile(targetDirectory, workDirectory), {
    mode: 0o600,
  });

  const deniedProbeCode = `import errno, json, socket, sys
s = socket.socket()
s.settimeout(2)
try:
    s.connect(('127.0.0.1', ${address.port}))
except OSError as error:
    denied = error.errno in (errno.EPERM, errno.EACCES)
    print(json.dumps({'status': 'denied' if denied else 'error', 'errno': error.errno}))
    sys.exit(0 if denied else 3)
else:
    print(json.dumps({'status': 'allowed'}))
    sys.exit(2)
`;
  const deniedProbe = await run(
    "sandbox-exec",
    ["-f", profilePath, "/usr/bin/python3", "-c", deniedProbeCode],
    { timeoutMs: 10_000 },
  );
  const deniedOutput = deniedProbe.stdout.toString("utf8").trim();
  let deniedObservation;
  try {
    deniedObservation = JSON.parse(deniedOutput);
  } catch {
    deniedObservation = null;
  }
  const probeDenied =
    deniedObservation?.status === "denied" &&
    [1, 13].includes(deniedObservation.errno) &&
    deniedProbe.code === 0 &&
    !deniedProbe.timedOut;

  // Compile outside the network sandbox. The resulting test executable runs
  // directly inside the deny-network sandbox, so a filtered test cannot pass.
  const cargo = process.env.DAKIA_CARGO ?? "cargo";
  const targetDirectoryForBuild = process.env.DAKIA_CLASSIFIER_TARGET_DIR;
  const ortLocation =
    process.env.ORT_LIB_LOCATION ??
    join(repository, "apps/desktop/src-tauri/frameworks");
  const build = await run(
    cargo,
    [
      "test",
      "--locked",
      "-p",
      "dakia-core",
      "--lib",
      "--no-run",
      "--message-format=json",
    ],
    {
      env: {
        CARGO_NET_OFFLINE: "true",
        ORT_LIB_LOCATION: ortLocation,
        ORT_PREFER_DYNAMIC_LINK: "1",
        DYLD_LIBRARY_PATH: ortLocation,
        DYLD_FALLBACK_LIBRARY_PATH: ortLocation,
        ...(targetDirectoryForBuild
          ? { CARGO_TARGET_DIR: targetDirectoryForBuild }
          : {}),
      },
      timeoutMs: 600_000,
    },
  );
  let testExecutable;
  for (const line of build.stdout.toString("utf8").split("\n")) {
    try {
      const event = JSON.parse(line);
      if (event.reason === "compiler-artifact" && event.profile?.test && event.executable) {
        testExecutable = event.executable;
      }
    } catch {
      // Older Cargo versions may include non-JSON diagnostics on stdout.
    }
  }
  if (build.code !== 0 || build.timedOut || !testExecutable) {
    throw new Error("could not build the classifier test executable outside the sandbox");
  }

  // Cargo's test executable uses @rpath for ONNX Runtime. Copy and adjust
  // only this temporary executable because sandbox-exec strips DYLD lookup
  // variables; the checked-out or target binary is never modified.
  const sandboxTestExecutable = join(workDirectory, "dakia-core-classifier-privacy");
  await cp(testExecutable, sandboxTestExecutable);
  const rpath = await run("install_name_tool", ["-add_rpath", ortLocation, sandboxTestExecutable]);
  if (rpath.code !== 0 || rpath.timedOut) {
    throw new Error("could not prepare the temporary classifier test executable");
  }

  const classifier = await run(
    "sandbox-exec",
    ["-f", profilePath, sandboxTestExecutable, "--ignored", "--exact", testName, "--nocapture"],
    {
      env: {
        CARGO_NET_OFFLINE: "true",
        ORT_LIB_LOCATION: ortLocation,
        ORT_PREFER_DYNAMIC_LINK: "1",
        DYLD_LIBRARY_PATH: ortLocation,
        DYLD_FALLBACK_LIBRARY_PATH: ortLocation,
        RUST_BACKTRACE: "0",
      },
      timeoutMs: 300_000,
    },
  );
  const classifierOutput = Buffer.concat([classifier.stdout, classifier.stderr]).toString("utf8");
  const counts = classifierOutput.match(
    /test result: .*?(\d+) passed; (\d+) failed; (\d+) ignored;/,
  );
  const classifierPassed =
    classifier.code === 0 &&
    classifier.signal === null &&
    !classifier.timedOut &&
    counts &&
    counts[1] === "1" &&
    counts[2] === "0" &&
    counts[3] === "0" &&
    classifierOutput.includes(`test ${testName} ... ok`) &&
    classifierOutput.includes(marker) &&
    !classifierOutput.includes(canary);

  result = {
    schema: 2,
    generatedAt: new Date().toISOString(),
    repository,
    sourceRevision: (await run("git", ["rev-parse", "HEAD"])).stdout.toString("utf8").trim(),
    boundary: "macOS sandbox-exec deny-default profile with local file access",
    positiveControl: {
      unsandboxedLoopbackConnection: positiveControl,
      serverConnectionsAfterControl: connectionCount,
    },
    probe: {
      attemptedLoopbackConnection: true,
      denied: probeDenied,
      explicitErrno: deniedObservation?.errno ?? null,
      exitCode: deniedProbe.code,
      signal: deniedProbe.signal,
      timedOut: deniedProbe.timedOut,
      stderr: deniedProbe.stderr.toString("utf8").slice(-400),
      serverConnectionsAfterDeniedProbe: connectionCount,
    },
    classifier: {
      productionModelIntegrationTest: testName,
      inferenceSucceeded: Boolean(classifierPassed),
      exactHarnessCounts: counts
        ? { passed: Number(counts[1]), failed: Number(counts[2]), ignored: Number(counts[3]) }
        : null,
      postInferenceMarkerObserved: classifierOutput.includes(marker),
      canaryAbsentFromOutput: !classifierOutput.includes(canary),
      outputBytes: classifierOutput.length,
      stderr: classifier.stderr.toString("utf8").slice(-400),
    },
    limitations: [
      "This proves successful inference under the enforced macOS sandbox; it does not observe blocked syscall attempts inside the classifier process.",
      "It does not prove behavior outside this sandbox or replace packaged WebKit and release acceptance.",
    ],
    status: probeDenied && classifierPassed ? "passed" : "failed",
  };
} catch (error) {
  result = {
    schema: 2,
    generatedAt: new Date().toISOString(),
    repository,
    boundary: "macOS sandbox-exec deny-default profile with local file access",
    status: "failed",
    error: String(error),
  };
} finally {
  server.close();
  if (workDirectory) await rm(workDirectory, { recursive: true, force: true });
}

await writeFile(resultPath, `${JSON.stringify(result, null, 2)}\n`, { mode: 0o600 });
if (result.status !== "passed") {
  process.stderr.write(`${JSON.stringify(result)}\n`);
  process.exitCode = 1;
}
