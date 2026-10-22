import { randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

function run(command, args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: "inherit", env });
    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0 && !signal) resolve();
      else reject(new Error(`${command} ${args.join(" ")} failed`));
    });
  });
}

const directory = await mkdtemp(join(tmpdir(), "dakia-realtime-delivery-"));
const coreArtifact = join(directory, "core-cycle.json");
const artifact = join(directory, "delivery.json");
const frontendResults = join(directory, "frontend-results.json");
const nonce = randomUUID();
const env = {
  ...process.env,
  DAKIA_REALTIME_DELIVERY_CORE_ARTIFACT: coreArtifact,
  DAKIA_REALTIME_DELIVERY_ARTIFACT: artifact,
  DAKIA_REALTIME_DELIVERY_NONCE: nonce,
};
try {
  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-core",
      "--lib",
      "--locked",
      "mail::tests::production_scripted_realtime_cycle_exports_runtime_artifact",
      "--",
      "--exact",
    ],
    env,
  );
  const coreCycle = JSON.parse(await readFile(coreArtifact, "utf8"));
  if (coreCycle.nonce !== nonce || !coreCycle.cycle?.newMessages?.length) {
    throw new Error(
      "scripted IMAP provider cycle did not produce the requested realtime artifact",
    );
  }
  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-desktop",
      "production_tauri_mock_runtime_sink_serializes_the_watcher_arrival",
      "--lib",
      "--locked",
    ],
    env,
  );
  const captured = JSON.parse(await readFile(artifact, "utf8"));
  if (captured.nonce !== nonce || !captured.arrival?.messages?.length) {
    throw new Error(
      "native watcher did not produce the requested coupled delivery artifact",
    );
  }
  const frontendEnv = {
    ...env,
    VITE_DAKIA_REALTIME_DELIVERY_NONCE: nonce,
    VITE_DAKIA_REALTIME_DELIVERY_ARTIFACT: Buffer.from(
      JSON.stringify(captured),
    ).toString("base64"),
  };
  await run(
    "npx",
    [
      "vitest",
      "run",
      "apps/desktop/src/realtimeDelivery.test.tsx",
      "--maxWorkers=1",
      "--reporter=json",
      "--outputFile",
      frontendResults,
    ],
    frontendEnv,
  );
  const frontend = JSON.parse(await readFile(frontendResults, "utf8"));
  if (frontend.numTotalTests !== 1 || frontend.numPendingTests !== 0) {
    throw new Error(
      "the coupled frontend test did not execute exactly one non-skipped test",
    );
  }
} finally {
  await rm(directory, { recursive: true, force: true });
}
