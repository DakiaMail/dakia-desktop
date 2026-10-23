import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);
const scriptPath = fileURLToPath(
  new URL("./run-local-translation-privacy.mjs", import.meta.url),
);
const passingResult = {
  status: "passed",
  positiveControl: { unsandboxedLoopbackConnection: true },
  probe: { denied: true, explicitErrno: 1, exitCode: 0, timedOut: false },
  inference: {
    succeeded: true,
    exitCode: 0,
    signal: null,
    timedOut: false,
    postInferenceMarkerObserved: true,
    canaryAbsentFromOutput: true,
  },
};

async function validates(candidate) {
  const directory = await mkdtemp("/private/tmp/dakia-translation-privacy-oracle-");
  const path = `${directory}/result.json`;
  await writeFile(path, JSON.stringify(candidate), { mode: 0o600 });
  try {
    await execFileAsync(process.execPath, [scriptPath, "--validate-result", path]);
    return true;
  } catch {
    return false;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("translation privacy result oracle accepts completed local inference", async () => {
  assert.equal(await validates(passingResult), true);
});

test("translation privacy result oracle rejects failed probe or missing marker", async () => {
  const probeFailure = structuredClone(passingResult);
  probeFailure.probe = { denied: false, explicitErrno: null, exitCode: 3, timedOut: false };
  assert.equal(await validates(probeFailure), false);

  const missingMarker = structuredClone(passingResult);
  missingMarker.inference.postInferenceMarkerObserved = false;
  assert.equal(await validates(missingMarker), false);

  for (const field of ["exitCode", "signal", "timedOut"]) {
    const incomplete = structuredClone(passingResult);
    delete incomplete.inference[field];
    assert.equal(await validates(incomplete), false, `missing ${field} must fail`);
  }
});
