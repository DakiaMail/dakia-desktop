import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter, once } from "node:events";
import test from "node:test";

import { stopFixture } from "./fixture-process-lifecycle.mjs";

class FakeChild extends EventEmitter {
  exitCode = null;
  signalCode = null;
  signals = [];
  unrefCalled = false;

  constructor({ exitOnKill = false } = {}) {
    super();
    this.exitOnKill = exitOnKill;
  }

  kill(signal) {
    this.signals.push(signal);
    if (signal === "SIGKILL" && this.exitOnKill) {
      queueMicrotask(() => {
        this.signalCode = signal;
        this.emit("exit", null, signal);
      });
    }
    return true;
  }

  unref() {
    this.unrefCalled = true;
  }
}

test("already signal-terminated fixture cleanup returns immediately", async () => {
  const child = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
    stdio: "ignore",
  });
  await once(child, "spawn");
  child.kill("SIGKILL");
  await once(child, "exit");
  assert.equal(child.exitCode, null);
  assert.equal(child.signalCode, "SIGKILL");
  await stopFixture(child, { termTimeoutMs: 5, killTimeoutMs: 5 });
});

test("fixture cleanup escalates from SIGTERM to SIGKILL", async () => {
  const child = new FakeChild({ exitOnKill: true });
  await stopFixture(child, { termTimeoutMs: 5, killTimeoutMs: 50 });
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.signalCode, "SIGKILL");
  assert.equal(child.unrefCalled, false);
});

test("fixture cleanup rejects and unreferences after a bounded kill wait", async () => {
  const child = new FakeChild();
  await assert.rejects(
    stopFixture(child, { termTimeoutMs: 5, killTimeoutMs: 5 }),
    /did not exit after SIGKILL/,
  );
  assert.deepEqual(child.signals, ["SIGTERM", "SIGKILL"]);
  assert.equal(child.unrefCalled, true);
  assert.equal(child.listenerCount("exit"), 0);
});
