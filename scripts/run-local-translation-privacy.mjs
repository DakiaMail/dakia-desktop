#!/usr/bin/env node

import { createHash } from "node:crypto";
import { createServer } from "node:net";
import {
  cp,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = join(
  repository,
  "apps/desktop/src-tauri/src/translation_models.json",
);
const source = "et";
const target = "en";
const canary = "fictional-translation-privacy-canary-20261021";
const marker = "translation-privacy-inference-complete";
const resultPath = process.argv[2]
  ? resolve(process.argv[2])
  : join(repository, "docs/mail-safety-results/local-translation-privacy.json");

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
  if (
    candidate?.inference?.succeeded !== true ||
    candidate?.inference?.exitCode !== 0 ||
    candidate?.inference?.signal !== null ||
    candidate?.inference?.timedOut !== false ||
    candidate?.inference?.postInferenceMarkerObserved !== true ||
    candidate?.inference?.canaryAbsentFromOutput !== true
  ) {
    failures.push("real translation inference");
  }
  return failures;
}

if (process.argv[2] === "--validate-result") {
  let candidate;
  try {
    candidate = JSON.parse(await readFile(resolve(process.argv[3]), "utf8"));
  } catch (error) {
    process.stderr.write(`could not read translation privacy result: ${String(error)}\n`);
    process.exit(1);
  }
  const failures = validateResult(candidate);
  if (failures.length > 0) {
    process.stderr.write(`invalid translation privacy result: ${failures.join(", ")}\n`);
    process.exit(1);
  }
  process.stdout.write("translation privacy result validated\n");
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

const sha256File = async (path) => {
  const bytes = await readFile(path);
  return {
    bytes: bytes.byteLength,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
};

const relevantInputPaths = {
  runner: fileURLToPath(import.meta.url),
  modelManifest: manifestPath,
  packageWorkerJs: join(
    repository,
    "node_modules/@browsermt/bergamot-translator/worker/bergamot-translator-worker.js",
  ),
  packageWorkerWasm: join(
    repository,
    "node_modules/@browsermt/bergamot-translator/worker/bergamot-translator-worker.wasm",
  ),
  packageTranslatorJs: join(
    repository,
    "node_modules/@browsermt/bergamot-translator/translator.js",
  ),
  packageLock: join(repository, "package-lock.json"),
};

const fingerprintInputs = async () => {
  const output = {};
  for (const [name, path] of Object.entries(relevantInputPaths)) {
    output[name] = { path, ...(await sha256File(path)) };
  }
  return output;
};

const sandboxProfile = (artifactDirectory) => `(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow sysctl-read)
(allow mach-lookup)
(allow file-read*)
(allow file-write* (subpath "${artifactDirectory}"))
`;

function bytesToArrayBuffer(bytes) {
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
}

async function inference(artifactDirectory) {
  // The npm package's Node shim currently evaluates its .js worker as an ES
  // module even though the worker uses CommonJS require(). Run a private copy
  // as .cjs so this lane exercises the same Bergamot WASM worker without
  // changing the application dependency or production worker path.
  const { Worker: NodeWorker } = await import("node:worker_threads");
  const workerPath = join(artifactDirectory, "worker", "translator-worker.cjs");
  globalThis.Worker = class LocalWorker {
    #worker = new NodeWorker(workerPath);
    addEventListener(eventName, callback) {
      this.#worker.on(eventName, (data) => callback({ data }));
    }
    postMessage(message) {
      this.#worker.postMessage(message);
    }
    terminate() {
      return this.#worker.terminate();
    }
  };
  globalThis.window = { Worker: globalThis.Worker };
  const { BatchTranslator, TranslatorBacking } = await import(
    "@browsermt/bergamot-translator/translator.js"
  );
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  const model = manifest.models.find(
    (candidate) => candidate.source === source && candidate.target === target,
  );
  if (!model) throw new Error(`missing pinned ${source}-${target} model`);

  class LocalBacking extends TranslatorBacking {
    async loadModelRegistery() {
      return [{ from: source, to: target }];
    }

    async loadTranslationModel() {
      const files = [
        ["model.bin", model.files.model],
        ["shortlist.bin", model.files.shortlist],
        ...model.files.vocabs.map((vocab, index) => [`vocab-${index}.spm`, vocab]),
      ];
      const buffers = await Promise.all(
        files.map(async ([name]) =>
          bytesToArrayBuffer(await readFile(join(artifactDirectory, name))),
        ),
      );
      return {
        model: buffers[0],
        shortlist: buffers[1],
        vocabs: buffers.slice(2),
        config: model.config ?? {},
      };
    }
  }

  const translator = new BatchTranslator(
    { workers: 1, batchSize: 1, cacheSize: 0, pivotLanguage: null },
    new LocalBacking({ cacheSize: 0, pivotLanguage: null }),
  );
  try {
    const response = await translator.translate({
      from: source,
      to: target,
      text: `Tere, see on ${canary}.`,
      html: false,
    });
    if (!response?.target?.text) {
      throw new Error("translation result was empty");
    }
    console.log(marker);
  } finally {
    await translator.delete();
  }
}

if (process.argv[2] === "--infer") {
  await inference(resolve(process.argv[3]));
  process.exit(0);
}

const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
const model = manifest.models.find(
  (candidate) => candidate.source === source && candidate.target === target,
);
if (!model) throw new Error(`missing pinned ${source}-${target} model`);

const sourceRevision = (
  await run("git", ["rev-parse", "HEAD"])
).stdout.toString("utf8").trim();
const workingTreeStatus = await run("git", ["status", "--porcelain"]);
const workingTreeText = workingTreeStatus.stdout.toString("utf8");
const provenanceBefore = await fingerprintInputs();

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
  if (!address || typeof address === "string") throw new Error("no control port");

  const positiveProbe = await run("/usr/bin/python3", [
    "-c",
    `import socket; s=socket.socket(); s.settimeout(2); s.connect(('127.0.0.1', ${address.port}))`,
  ]);
  await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
  const positiveControl = positiveProbe.code === 0 && connectionCount === 1;
  if (!positiveControl) throw new Error("unsandboxed loopback control failed");

  workDirectory = await mkdtemp("/private/tmp/dakia-translation-privacy-");
  const artifactDirectory = join(workDirectory, "artifacts");
  await import("node:fs/promises").then(({ mkdir }) => mkdir(artifactDirectory));
  const profilePath = join(workDirectory, "network-denied.sb");
  await writeFile(profilePath, sandboxProfile(artifactDirectory), { mode: 0o600 });

  const cacheDirectory = process.env.DAKIA_TRANSLATION_CACHE
    ? resolve(process.env.DAKIA_TRANSLATION_CACHE)
    : join(workDirectory, "artifacts");
  await mkdir(cacheDirectory, { recursive: true });
  const files = [
    ["model.bin", model.files.model],
    ["shortlist.bin", model.files.shortlist],
    ...model.files.vocabs.map((vocab, index) => [`vocab-${index}.spm`, vocab]),
  ];
  for (const [name, [url, expectedBytes, expectedHash]] of files) {
    const destination = join(cacheDirectory, name);
    let valid = false;
    try {
      const bytes = await readFile(destination);
      valid =
        bytes.byteLength === expectedBytes &&
        createHash("sha256").update(bytes).digest("hex") === expectedHash;
    } catch {
      valid = false;
    }
    if (!valid) {
      const response = await fetch(url);
      if (!response.ok) throw new Error(`model download failed: ${response.status}`);
      const bytes = Buffer.from(await response.arrayBuffer());
      if (
        bytes.byteLength !== expectedBytes ||
        createHash("sha256").update(bytes).digest("hex") !== expectedHash
      ) {
        throw new Error(`pinned hash or size mismatch for ${name}`);
      }
      await writeFile(destination, bytes, { mode: 0o600 });
    }
  }
  if (cacheDirectory !== artifactDirectory) {
    for (const [name] of files) await cp(join(cacheDirectory, name), join(artifactDirectory, name));
  }
  const packageWorkerDirectory = join(
    repository,
    "node_modules/@browsermt/bergamot-translator/worker",
  );
  const workerDirectory = join(artifactDirectory, "worker");
  await mkdir(workerDirectory, { recursive: true });
  await cp(join(packageWorkerDirectory, "translator-worker.js"), join(workerDirectory, "translator-worker.cjs"));
  await cp(join(packageWorkerDirectory, "bergamot-translator-worker.js"), join(workerDirectory, "bergamot-translator-worker.js"));
  await cp(join(packageWorkerDirectory, "bergamot-translator-worker.wasm"), join(workerDirectory, "bergamot-translator-worker.wasm"));
  const adaptationSource = await sha256File(join(packageWorkerDirectory, "translator-worker.js"));
  const adaptationCopy = await sha256File(join(workerDirectory, "translator-worker.cjs"));

  const deniedProbeCode = `import errno, json, socket, sys
s=socket.socket(); s.settimeout(2)
try:
 s.connect(('127.0.0.1', ${address.port}))
except OSError as error:
 denied=error.errno in (errno.EPERM, errno.EACCES)
 print(json.dumps({'status':'denied' if denied else 'error','errno':error.errno}))
 sys.exit(0 if denied else 3)
else:
 print(json.dumps({'status':'allowed'})); sys.exit(2)
`;
  const deniedProbe = await run(
    "sandbox-exec",
    ["-f", profilePath, "/usr/bin/python3", "-c", deniedProbeCode],
    { timeoutMs: 10_000 },
  );
  const probe = JSON.parse(deniedProbe.stdout.toString("utf8").trim());
  const denied =
    probe.status === "denied" &&
    [1, 13].includes(probe.errno) &&
    deniedProbe.code === 0 &&
    !deniedProbe.timedOut;
  if (!denied) throw new Error("sandbox network denial probe did not report EPERM/EACCES");

  const classifier = await run(
    "sandbox-exec",
    ["-f", profilePath, process.execPath, fileURLToPath(import.meta.url), "--infer", artifactDirectory],
    { timeoutMs: 300_000 },
  );
  const output = Buffer.concat([classifier.stdout, classifier.stderr]).toString("utf8");
  const passed =
    classifier.code === 0 &&
    classifier.signal === null &&
    !classifier.timedOut &&
    output.includes(marker) &&
    !output.includes(canary);
  const provenanceAfter = await fingerprintInputs();
  result = {
    schema: 1,
    generatedAt: new Date().toISOString(),
    lane: "local-translation-privacy",
    sourceRevision,
    workingTree: {
      dirty: workingTreeText.length > 0,
      statusSha256: createHash("sha256").update(workingTreeText).digest("hex"),
    },
    environment: {
      node: process.version,
      platform: process.platform,
      arch: process.arch,
      sandbox: "macOS sandbox-exec deny-default",
    },
    relevantInputs: { before: provenanceBefore, after: provenanceAfter },
    source,
    target,
    modelVersion: model.version,
    modelFiles: files.map(([name, [url, bytes, sha256]]) => ({ name, url, bytes, sha256 })),
    boundary: "macOS sandbox-exec deny-default profile with local model files",
    provenance: {
      modelManifest: "Reused the existing checked-in Mozilla Firefox Translations manifest; no model binaries are committed.",
      modelLicenseReview: "Model license and registry provenance remain tied to the existing Mozilla manifest and must be reviewed before redistribution.",
      workerAdaptation: {
        reason: "The package worker is .js under type=module but uses CommonJS require().",
        sourceFile: "node_modules/@browsermt/bergamot-translator/worker/translator-worker.js",
        sourceBytes: adaptationSource.bytes,
        sourceSha256: adaptationSource.sha256,
        temporaryCopy: "translator-worker.cjs",
        copyBytes: adaptationCopy.bytes,
        copySha256: adaptationCopy.sha256,
        limitation: "This temporary adaptation is test-only and is not evidence for the production WebKit worker path.",
      },
    },
    positiveControl: { unsandboxedLoopbackConnection: positiveControl, serverConnections: connectionCount },
    probe: { denied, explicitErrno: probe.errno, exitCode: deniedProbe.code, timedOut: deniedProbe.timedOut, serverConnections: connectionCount },
    inference: { succeeded: passed, exitCode: classifier.code, signal: classifier.signal, timedOut: classifier.timedOut, postInferenceMarkerObserved: output.includes(marker), canaryAbsentFromOutput: !output.includes(canary), outputBytes: output.length, diagnostics: output.slice(-2000) },
    limitations: [
      "This proves successful Bergamot inference under the enforced macOS sandbox; it does not observe blocked syscall attempts inside the translator process.",
      "It does not replace packaged WebKit acceptance or prove behavior outside this sandbox.",
    ],
    status: passed ? "passed" : "failed",
  };
} catch (error) {
  result = { schema: 1, generatedAt: new Date().toISOString(), status: "failed", error: String(error) };
} finally {
  server.close();
  if (workDirectory) await rm(workDirectory, { recursive: true, force: true });
}

await writeFile(resultPath, `${JSON.stringify(result, null, 2)}\n`, { mode: 0o600 });
if (result.status !== "passed") {
  process.stderr.write(`${JSON.stringify(result)}\n`);
  process.exitCode = 1;
}
