import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { stopFixture } from "./fixture-process-lifecycle.mjs";
import { run, waitForCommandChild } from "./test-unsubscribe-command-tls.mjs";

function waitForReady(child, timeoutMs = 1_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("timeout test child did not announce readiness")),
      timeoutMs,
    );
    child.once("message", (message) => {
      clearTimeout(timer);
      resolve(message);
    });
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      reject(
        new Error(
          `timeout test child exited before readiness: ${code ?? signal}`,
        ),
      );
    });
  });
}

test("bounded command timeout terminates a ready owned child before rejecting", async () => {
  const root = await mkdtemp(join(tmpdir(), "dakia-unsubscribe-timeout-"));
  const marker = join(root, "terminated");
  const child = spawn(
    process.execPath,
    [
      "-e",
      "const fs=require('node:fs'); process.on('SIGTERM',()=>{fs.writeFileSync(process.argv[1], 'terminated'); process.exit(0)}); process.send('ready'); setInterval(()=>{}, 1000)",
      marker,
    ],
    { stdio: ["ignore", "ignore", "ignore", "ipc"] },
  );
  try {
    assert.equal(await waitForReady(child), "ready");
    await assert.rejects(
      waitForCommandChild(child, process.execPath, ["ready-child"], {
        timeoutMs: 20,
      }),
      /timed out after 20ms/,
    );
    assert.equal(await readFile(marker, "utf8"), "terminated");
  } finally {
    await stopFixture(child);
    await rm(root, { recursive: true, force: true });
  }
});

test("run resolves a normally exiting child", async () => {
  await run(
    process.execPath,
    ["-e", "process.exit(0)"],
    process.env,
    process.cwd(),
    { timeoutMs: 1_000 },
  );
});
