import { strict as assert } from "node:assert";
import { once } from "node:events";
import net from "node:net";
import test from "node:test";
import {
  controlFictionalImapProxy,
  createFictionalImapProxy,
} from "./native-fictional-imap-proxy.mjs";

const noDataWithin = async (socket, durationMs = 40) => {
  let received = false;
  const receivedData = () => {
    received = true;
  };
  socket.once("data", receivedData);
  await new Promise((resolve) => setTimeout(resolve, durationMs));
  socket.off("data", receivedData);
  assert.equal(received, false);
};

test("fictional proxy preserves bytes, drops owned connections, blocks and resumes without restarting backend", async () => {
  const server = net.createServer((socket) => socket.pipe(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
  });
  const connect = async () => {
    const socket = net.connect({ host: "127.0.0.1", port: proxy.port });
    socket.on("error", () => {});
    await once(socket, "connect");
    return socket;
  };
  try {
    const first = await connect();
    const bytes = Buffer.from([0, 255, 13, 10, 22, 3, 3, 42]);
    const echoed = once(first, "data");
    first.write(bytes);
    assert.deepEqual((await echoed)[0], bytes);
    const firstClosed = once(first, "close");
    proxy.disable();
    await firstClosed;
    const blocked = await connect();
    await once(blocked, "close");
    assert.equal(proxy.snapshot().disabled, true);
    assert.equal(proxy.snapshot().blocked, 1);
    proxy.resume();
    const resumed = await connect();
    const resumedEcho = once(resumed, "data");
    resumed.write("same backend survives");
    assert.equal((await resumedEcho)[0].toString(), "same backend survives");
    resumed.destroy();
    assert.equal(proxy.snapshot().accepted, 2);
  } finally {
    await proxy.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("owned hold keeps existing and new connections pending, then resumes their exact bytes", async () => {
  const backendSockets = [];
  const server = net.createServer((socket) => backendSockets.push(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
  });
  const connect = async () => {
    const socket = net.connect({ host: "127.0.0.1", port: proxy.port });
    socket.on("error", () => {});
    await once(socket, "connect");
    return socket;
  };
  let existing;
  let duringHold;
  const waitForBackendCount = async (count) => {
    for (let attempt = 0; attempt < 100; attempt++) {
      if (backendSockets.length === count) return;
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    throw new Error(`Expected ${count} backend connections`);
  };
  try {
    existing = await connect();
    await waitForBackendCount(1);
    const existingBackend = backendSockets[0];
    proxy.hold(1_000);
    assert.equal(proxy.snapshot().held, true);
    existing.write("existing request remains pending");
    await noDataWithin(existingBackend);
    existingBackend.write("existing response remains pending");
    await noDataWithin(existing);

    duringHold = await connect();
    await waitForBackendCount(2);
    const newBackend = backendSockets[1];
    duringHold.write("new request remains pending");
    await noDataWithin(newBackend);

    const existingRequest = once(existingBackend, "data");
    const existingResponse = once(existing, "data");
    const newRequest = once(newBackend, "data");
    proxy.resume();
    assert.equal(proxy.snapshot().held, false);
    assert.equal(
      (await existingRequest)[0].toString(),
      "existing request remains pending",
    );
    assert.equal(
      (await existingResponse)[0].toString(),
      "existing response remains pending",
    );
    assert.equal(
      (await newRequest)[0].toString(),
      "new request remains pending",
    );
  } finally {
    existing?.destroy();
    duringHold?.destroy();
    await proxy.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("owned hold automatically resumes inside its bounded deadline", async () => {
  const server = net.createServer((socket) => socket.pipe(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
  });
  const socket = net.connect({ host: "127.0.0.1", port: proxy.port });
  socket.on("error", () => {});
  await once(socket, "connect");
  try {
    proxy.hold(100);
    const echoed = once(socket, "data");
    socket.write("automatically resumed");
    assert.equal((await echoed)[0].toString(), "automatically resumed");
    assert.equal(proxy.snapshot().held, false);
    assert.equal(proxy.snapshot().holdDeadlineAt, null);
    assert.equal(proxy.snapshot().holdDurationMs, null);
  } finally {
    socket.destroy();
    await proxy.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("explicit resume clears the automatic timer and repeated hold replaces its deadline", async () => {
  const states = [];
  const server = net.createServer((socket) => socket.pipe(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
    onState: (state) => states.push(state),
  });
  try {
    proxy.hold(100);
    const firstDeadline = proxy.snapshot().holdDeadlineAt;
    await new Promise((resolve) => setTimeout(resolve, 60));
    proxy.hold(200);
    assert.notEqual(proxy.snapshot().holdDeadlineAt, firstDeadline);
    await new Promise((resolve) => setTimeout(resolve, 70));
    assert.equal(proxy.snapshot().held, true);
    proxy.resume();
    const stateCountAfterResume = states.length;
    await new Promise((resolve) => setTimeout(resolve, 220));
    assert.equal(proxy.snapshot().held, false);
    assert.equal(states.length, stateCountAfterResume);
  } finally {
    await proxy.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("close while held clears the deadline and does not leave the server stuck", async () => {
  const server = net.createServer((socket) => socket.pipe(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
  });
  const socket = net.connect({ host: "127.0.0.1", port: proxy.port });
  socket.on("error", () => {});
  await once(socket, "connect");
  proxy.hold(1_000);
  await Promise.race([
    proxy.close(),
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error("held proxy close timed out")), 500),
    ),
  ]);
  await new Promise((resolve) => server.close(resolve));
});

test("hold rejects missing, excessive, and disabled durations", async () => {
  const server = net.createServer((socket) => socket.pipe(socket));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const proxy = await createFictionalImapProxy({
    backendPort: server.address().port,
  });
  try {
    for (const duration of [undefined, 0, 99, 35_001, NaN]) {
      assert.throws(() => proxy.hold(duration), /hold duration/);
    }
    proxy.disable();
    assert.throws(() => proxy.hold(1_000), /disabled/);
  } finally {
    await proxy.close();
    await new Promise((resolve) => server.close(resolve));
  }
});

test("control surface rejects unknown actions and invalid hold bounds before publication", async () => {
  const { mkdtemp, writeFile, rm } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const root = await mkdtemp(
    join(tmpdir(), "dakia-native-proxy-control-test-"),
  );
  await writeFile(
    join(root, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
  );
  try {
    await assert.rejects(
      controlFictionalImapProxy(root, "pause"),
      /Unknown fictional proxy control action/,
    );
    for (const duration of [undefined, 99, 35_001, NaN]) {
      await assert.rejects(
        controlFictionalImapProxy(root, "hold", duration),
        /Hold requires a duration/,
      );
    }
    await assert.rejects(
      controlFictionalImapProxy(root, "status", 100),
      /Hold requires a duration/,
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("fictional proxy rejects invalid backend ports before opening a listener", async () => {
  for (const backendPort of [0, -1, 65536, NaN, "993"])
    await assert.rejects(
      createFictionalImapProxy({ backendPort }),
      /valid fictional loopback/,
    );
});

test(
  "CLI acknowledges owned commands without signaling a stale metadata PID",
  { timeout: 10_000 },
  async () => {
    const { access, mkdtemp, readFile, writeFile, rm, stat } =
      await import("node:fs/promises");
    const { tmpdir } = await import("node:os");
    const { join } = await import("node:path");
    const { fileURLToPath } = await import("node:url");
    const { spawn, execFile } = await import("node:child_process");
    const { promisify } = await import("node:util");
    const execFileAsync = promisify(execFile);
    const root = await mkdtemp(join(tmpdir(), "dakia-native-proxy-test-"));
    await writeFile(
      join(root, ".dakia-native-mail-safety-root"),
      "dakia-native-mail-safety-fixture-v1\n",
    );
    const backend = net.createServer((socket) => socket.pipe(socket));
    await new Promise((resolve) => backend.listen(0, "127.0.0.1", resolve));
    const child = spawn(
      process.execPath,
      [
        fileURLToPath(
          new URL("./native-fictional-imap-proxy.mjs", import.meta.url),
        ),
        "--root",
        root,
        "--backend-port",
        String(backend.address().port),
      ],
      { stdio: "ignore" },
    );
    const exited = once(child, "exit");
    let socket;
    const waitFor = async (file, predicate) => {
      for (let attempt = 0; attempt < 100; attempt++) {
        if (child.exitCode !== null)
          throw new Error("Fictional proxy exited before acknowledgement");
        try {
          const value = JSON.parse(await readFile(join(root, file), "utf8"));
          if (predicate(value)) return value;
        } catch (error) {
          if (error.code !== "ENOENT" && !(error instanceof SyntaxError))
            throw error;
        }
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      throw new Error("Fictional proxy acknowledgement timed out");
    };
    try {
      const ready = await waitFor(
        "imap-proxy.json",
        (value) => value.pid === child.pid,
      );
      assert.equal(
        (await stat(join(root, "imap-proxy.json"))).mode & 0o777,
        0o600,
      );
      socket = net.connect({ host: "127.0.0.1", port: ready.port });
      socket.on("error", () => {});
      await once(socket, "connect");
      const data = once(socket, "data");
      socket.write("opaque TLS bytes");
      assert.equal((await data)[0].toString(), "opaque TLS bytes");
      const closed = once(socket, "close");
      // Deliberately stale PID points at this test process. Control must use
      // only the private root/nonce acknowledgement, never signal that PID.
      await writeFile(
        join(root, "imap-proxy.json"),
        JSON.stringify({ ...ready, pid: process.pid }),
      );
      const control = (action, durationMs) =>
        execFileAsync(process.execPath, [
          fileURLToPath(
            new URL("./native-fictional-imap-proxy.mjs", import.meta.url),
          ),
          "--control-root",
          root,
          "--action",
          action,
          ...(durationMs === undefined
            ? []
            : ["--duration-ms", String(durationMs)]),
        ]);
      const badCommandId = "00000000-0000-4000-8000-000000000001";
      await writeFile(
        join(root, "imap-proxy-command.json"),
        JSON.stringify({
          action: "hold",
          commandId: badCommandId,
          nonce: "0".repeat(48),
          durationMs: 500,
        }),
        { flag: "wx", mode: 0o600 },
      );
      for (let attempt = 0; attempt < 100; attempt++) {
        try {
          await access(join(root, "imap-proxy-command.json"));
        } catch (error) {
          if (error.code === "ENOENT") break;
          throw error;
        }
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      await assert.rejects(
        access(join(root, "imap-proxy-command.json")),
        /ENOENT/,
      );
      assert.notEqual(
        JSON.parse(await readFile(join(root, "imap-proxy-state.json"), "utf8"))
          .lastCommandId,
        badCommandId,
      );
      await control("hold", 500);
      const held = await waitFor(
        "imap-proxy-state.json",
        (value) =>
          value.held === true &&
          value.holdDurationMs === 500 &&
          Date.parse(value.holdDeadlineAt) > Date.now(),
      );
      assert.equal(held.lastCommandError, null);
      await control("resume");
      await waitFor(
        "imap-proxy-state.json",
        (value) =>
          value.held === false &&
          value.holdDeadlineAt === null &&
          value.holdDurationMs === null,
      );
      await control("disconnect");
      await closed;
      await waitFor(
        "imap-proxy-state.json",
        (value) => value.disabled && value.activeSockets === 0,
      );
      await assert.rejects(control("hold", 500), /rejected control.*disabled/);
      assert.equal(child.exitCode, null);
      await waitFor(
        "imap-proxy-state.json",
        (value) =>
          value.disabled === true &&
          value.held === false &&
          value.lastCommandError === "Cannot hold a disabled fictional proxy",
      );
      await control("resume");
      await waitFor(
        "imap-proxy-state.json",
        (value) => value.disabled === false,
      );
      child.kill("SIGTERM");
      const [code, signal] = await exited;
      assert.equal(code, 0);
      assert.equal(signal, null);
      await assert.rejects(control("status"), /no process was signaled/);
      assert.equal(
        JSON.parse(await readFile(join(root, "imap-proxy-state.json"), "utf8"))
          .activeSockets,
        0,
      );
    } finally {
      socket?.destroy();
      if (child.exitCode === null) child.kill("SIGTERM");
      await exited;
      await new Promise((resolve) => backend.close(resolve));
      await rm(root, { recursive: true, force: true });
    }
  },
);
