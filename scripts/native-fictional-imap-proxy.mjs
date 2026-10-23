import net from "node:net";
import { randomBytes, randomUUID } from "node:crypto";
import {
  existsSync,
  linkSync,
  unlinkSync,
  readFileSync,
  realpathSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

// This forwards encrypted bytes without inspecting them. Disconnect closes
// only owned fixture connections. Hold applies bounded socket backpressure and
// automatically releases it without changing GreenMail or its mailbox.
export async function createFictionalImapProxy({
  backendPort,
  onState = () => {},
}) {
  if (!Number.isInteger(backendPort) || backendPort < 1 || backendPort > 65535)
    throw new Error("A valid fictional loopback backend port is required");
  const sockets = new Set();
  const pairs = new Set();
  let disabled = false;
  let held = false;
  let holdDeadlineAt = null;
  let holdDurationMs = null;
  let holdTimer;
  let accepted = 0;
  let blocked = 0;
  const snapshot = () => ({
    disabled,
    held,
    holdDeadlineAt,
    holdDurationMs,
    accepted,
    blocked,
    activeSockets: sockets.size,
  });
  const changed = () => onState(snapshot());
  const track = (socket) => {
    sockets.add(socket);
    socket.once("close", () => {
      sockets.delete(socket);
      changed();
    });
    socket.on("error", () => {});
  };
  const clearHold = ({ resumeSockets }) => {
    if (holdTimer) clearTimeout(holdTimer);
    holdTimer = undefined;
    held = false;
    holdDeadlineAt = null;
    holdDurationMs = null;
    if (!resumeSockets) return;
    for (const pair of pairs) {
      if (!pair.piped && !pair.client.destroyed && !pair.backend.destroyed) {
        pair.client.pipe(pair.backend).pipe(pair.client);
        pair.piped = true;
      }
      pair.client.resume();
      pair.backend.resume();
    }
    changed();
  };
  const releaseHold = () => clearHold({ resumeSockets: true });
  const hold = (durationMs) => {
    if (
      !Number.isInteger(durationMs) ||
      durationMs < 100 ||
      durationMs > 35_000
    )
      throw new Error("A hold duration from 100 through 35000 ms is required");
    if (disabled) throw new Error("Cannot hold a disabled fictional proxy");
    if (holdTimer) clearTimeout(holdTimer);
    held = true;
    holdDurationMs = durationMs;
    holdDeadlineAt = new Date(Date.now() + durationMs).toISOString();
    for (const { client, backend } of pairs) {
      client.pause();
      backend.pause();
    }
    holdTimer = setTimeout(releaseHold, durationMs);
    changed();
  };
  const server = net.createServer((client) => {
    if (disabled) {
      blocked++;
      client.destroy();
      changed();
      return;
    }
    accepted++;
    const backend = net.connect({ host: "127.0.0.1", port: backendPort });
    const pair = { client, backend, piped: false };
    pairs.add(pair);
    track(client);
    track(backend);
    client.once("close", () => {
      pairs.delete(pair);
      backend.destroy();
    });
    backend.once("close", () => {
      pairs.delete(pair);
      client.destroy();
    });
    backend.once("error", () => client.destroy());
    if (held) {
      client.pause();
      backend.pause();
    } else {
      client.pipe(backend).pipe(client);
      pair.piped = true;
    }
    changed();
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = server.address().port;
  changed();
  return {
    port,
    snapshot,
    disable() {
      clearHold({ resumeSockets: false });
      disabled = true;
      for (const socket of sockets) socket.destroy();
      changed();
    },
    hold,
    resume() {
      disabled = false;
      releaseHold();
      changed();
    },
    async close() {
      clearHold({ resumeSockets: false });
      disabled = true;
      for (const socket of sockets) socket.destroy();
      await new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    },
  };
}

function ownedRoot(path) {
  const root = realpathSync(path);
  if (
    readFileSync(join(root, ".dakia-native-mail-safety-root"), "utf8") !==
    "dakia-native-mail-safety-fixture-v1\n"
  )
    throw new Error("Fictional proxy root lacks its exact ownership sentinel");
  return root;
}

export async function controlFictionalImapProxy(rootPath, action, durationMs) {
  const root = ownedRoot(rootPath);
  if (!["disconnect", "hold", "resume", "status"].includes(action))
    throw new Error("Unknown fictional proxy control action");
  if (
    (action === "hold" &&
      (!Number.isInteger(durationMs) ||
        durationMs < 100 ||
        durationMs > 35_000)) ||
    (action !== "hold" && durationMs !== undefined)
  )
    throw new Error("Hold requires a duration from 100 through 35000 ms");
  const metadata = JSON.parse(
    readFileSync(join(root, "imap-proxy.json"), "utf8"),
  );
  if (!/^[a-f0-9]{48}$/.test(metadata.controlNonce))
    throw new Error("Fictional proxy control metadata is invalid");
  const commandId = randomUUID();
  const commandPath = join(root, "imap-proxy-command.json");
  const temporary = `${commandPath}.${commandId}.tmp`;
  try {
    writeFileSync(
      temporary,
      JSON.stringify({
        action,
        commandId,
        nonce: metadata.controlNonce,
        ...(action === "hold" ? { durationMs } : {}),
      }),
      { flag: "wx", mode: 0o600 },
    );
    // Publish a complete command without replacing a pending command. No PID
    // from old metadata is ever signaled, even if the proxy has exited.
    linkSync(temporary, commandPath);
  } finally {
    if (existsSync(temporary)) unlinkSync(temporary);
  }
  const deadline = Date.now() + 3000;
  while (Date.now() < deadline) {
    const state = JSON.parse(
      readFileSync(join(root, "imap-proxy-state.json"), "utf8"),
    );
    if (state.lastCommandId === commandId && state.lastCommandError) {
      throw new Error(
        `Owned fictional proxy rejected control: ${state.lastCommandError}`,
      );
    }
    const parsedHoldDeadline = Date.parse(state.holdDeadlineAt);
    if (
      state.lastCommandId === commandId &&
      (action !== "disconnect" ||
        (state.disabled === true && state.activeSockets === 0)) &&
      (action !== "resume" || state.disabled === false) &&
      (action !== "hold" ||
        (state.held === true &&
          state.holdDurationMs === durationMs &&
          Number.isFinite(parsedHoldDeadline) &&
          parsedHoldDeadline > Date.now()))
    )
      return state;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  throw new Error(
    "Owned fictional proxy did not acknowledge control; no process was signaled",
  );
}

async function main(args) {
  if (
    (args.length === 4 || args.length === 6) &&
    args[0] === "--control-root" &&
    args[2] === "--action"
  ) {
    const durationMs =
      args.length === 6 && args[4] === "--duration-ms"
        ? Number(args[5])
        : undefined;
    console.log(
      JSON.stringify(
        await controlFictionalImapProxy(args[1], args[3], durationMs),
      ),
    );
    return;
  }
  if (args.length !== 4 || args[0] !== "--root" || args[2] !== "--backend-port")
    throw new Error("Use --root OWNED_FIXTURE_ROOT --backend-port PORT");
  const root = ownedRoot(args[1]);
  const statePath = join(root, "imap-proxy-state.json");
  const portPath = join(root, "imap-proxy.json");
  if (existsSync(statePath) || existsSync(portPath))
    throw new Error("Refusing to overwrite existing fictional proxy evidence");
  const controlNonce = randomBytes(24).toString("hex");
  let lastCommandId = null;
  let lastCommandError = null;
  const proxy = await createFictionalImapProxy({
    backendPort: Number(args[3]),
    onState(state) {
      const temporary = `${statePath}.${process.pid}.tmp`;
      writeFileSync(
        temporary,
        `${JSON.stringify({ ...state, lastCommandId, lastCommandError, capturedAt: new Date().toISOString() }, null, 2)}\n`,
        { flag: "wx", mode: 0o600 },
      );
      renameSync(temporary, statePath);
    },
  });
  const portTemporary = `${portPath}.${process.pid}.tmp`;
  try {
    writeFileSync(
      portTemporary,
      `${JSON.stringify({ pid: process.pid, port: proxy.port, backendPort: Number(args[3]), controlNonce }, null, 2)}\n`,
      { flag: "wx", mode: 0o600 },
    );
    linkSync(portTemporary, portPath);
  } finally {
    if (existsSync(portTemporary)) unlinkSync(portTemporary);
  }
  const commandPath = join(root, "imap-proxy-command.json");
  const controlTimer = setInterval(() => {
    if (!existsSync(commandPath)) return;
    let command;
    try {
      const bytes = readFileSync(commandPath);
      if (bytes.length > 512) throw new Error("oversized control");
      command = JSON.parse(bytes.toString("utf8"));
      if (
        command.nonce !== controlNonce ||
        !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
          command.commandId,
        ) ||
        !["disconnect", "hold", "resume", "status"].includes(command.action) ||
        (command.action === "hold" &&
          (!Number.isInteger(command.durationMs) ||
            command.durationMs < 100 ||
            command.durationMs > 35_000)) ||
        (command.action !== "hold" && command.durationMs !== undefined)
      )
        throw new Error("invalid control");
    } catch {
      console.error("Invalid owned fictional proxy control command");
      unlinkSync(commandPath);
      return;
    }
    unlinkSync(commandPath);
    lastCommandId = command.commandId;
    lastCommandError = null;
    try {
      if (command.action === "disconnect") proxy.disable();
      else if (command.action === "hold") proxy.hold(command.durationMs);
      else if (command.action === "resume") proxy.resume();
      else {
        const temporary = `${statePath}.${process.pid}.tmp`;
        writeFileSync(
          temporary,
          `${JSON.stringify({ ...proxy.snapshot(), lastCommandId, lastCommandError, capturedAt: new Date().toISOString() }, null, 2)}\n`,
          { flag: "wx", mode: 0o600 },
        );
        renameSync(temporary, statePath);
      }
    } catch (error) {
      lastCommandError = error instanceof Error ? error.message : String(error);
      const temporary = `${statePath}.${process.pid}.tmp`;
      writeFileSync(
        temporary,
        `${JSON.stringify({ ...proxy.snapshot(), lastCommandId, lastCommandError, capturedAt: new Date().toISOString() }, null, 2)}\n`,
        { flag: "wx", mode: 0o600 },
      );
      renameSync(temporary, statePath);
    }
  }, 50);
  let closing = false;
  const stop = async () => {
    if (closing) return;
    closing = true;
    clearInterval(controlTimer);
    await proxy.close();
  };
  process.on("SIGTERM", stop);
  process.on("SIGINT", stop);
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
