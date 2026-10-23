import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(
  new URL("./run-local-classifier-privacy.mjs", import.meta.url),
);

const script = await readFile(
  scriptPath,
  "utf8",
);

const passingResult = {
  status: "passed",
  positiveControl: { unsandboxedLoopbackConnection: true },
  probe: { denied: true, explicitErrno: 1, exitCode: 0, timedOut: false },
  classifier: {
    inferenceSucceeded: true,
    exactHarnessCounts: { passed: 1, failed: 0, ignored: 0 },
    postInferenceMarkerObserved: true,
    canaryAbsentFromOutput: true,
  },
};

async function validate(candidate) {
  const directory = await mkdtemp("/private/tmp/dakia-classifier-privacy-oracle-");
  const path = `${directory}/result.json`;
  await writeFile(path, `${JSON.stringify(candidate)}\n`, { mode: 0o600 });
  try {
    await execFileAsync(process.execPath, [scriptPath, "--validate-result", path]);
    return true;
  } catch {
    return false;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("classifier privacy result oracle accepts one completed real inference", async () => {
  assert.equal(await validate(passingResult), true);
});

test("classifier privacy result oracle rejects zero tests, probe errors, and timeouts", async () => {
  const zeroTests = structuredClone(passingResult);
  zeroTests.classifier.exactHarnessCounts.passed = 0;
  zeroTests.classifier.inferenceSucceeded = false;
  assert.equal(await validate(zeroTests), false);

  const probeError = structuredClone(passingResult);
  probeError.probe = { denied: false, explicitErrno: null, exitCode: 3, timedOut: false };
  assert.equal(await validate(probeError), false);

  const timeout = structuredClone(passingResult);
  timeout.probe.timedOut = true;
  assert.equal(await validate(timeout), false);
});

test("classifier privacy runner requires an explicit denied probe and one real model test", () => {
  assert.match(script, /unsandboxedLoopbackConnection/);
  assert.match(script, /status.*denied.*if denied/);
  assert.match(script, /\[1, 13\]\.includes\(deniedObservation\.errno\)/);
  assert.match(script, /--no-run/);
  assert.match(script, /--ignored/);
  assert.match(script, /--exact/);
  assert.match(script, /counts\[1\] === "1"/);
  assert.match(script, /counts\[2\] === "0"/);
  assert.match(script, /counts\[3\] === "0"/);
  assert.match(script, /classifier-privacy-inference-complete/);
  assert.doesNotMatch(script, /allow network-outbound/);
});

test("classifier privacy runner reports limitations instead of overstating syscall evidence", () => {
  assert.match(script, /does not observe blocked syscall attempts/);
  assert.match(script, /does not prove behavior outside this sandbox/);
});
