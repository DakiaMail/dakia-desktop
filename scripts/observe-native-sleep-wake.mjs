import { execFile } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import tls from "node:tls";
import { once } from "node:events";
import { promisify } from "node:util";
import { pathToFileURL } from "node:url";

const execFileAsync = promisify(execFile);
const RETRY_MARKER = "real-time sync reconnect scheduled";
const ARRIVAL_MARKER = "new mail headers persisted";

export function countMarker(log, marker) {
  return log.split(marker).length - 1;
}

export function retryEvidence(before, after, recoveryMilliseconds) {
  const retryDelta = after - before;
  const allowedRetries = Math.min(
    10,
    Math.max(1, Math.ceil(recoveryMilliseconds / 1_000)),
  );
  return {
    retryDelta,
    allowedRetries,
    bounded: retryDelta >= 0 && retryDelta <= allowedRetries,
  };
}

export function parsePowerCycle(
  log,
  observedAfterMilliseconds,
  minimumSleepMilliseconds = 15_000,
) {
  let sleep = null;
  const observedAfterSecond =
    Math.floor(observedAfterMilliseconds / 1_000) * 1_000;
  for (const line of log.split(/\r?\n/)) {
    const stamp = /^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) ([+-]\d{4})/.exec(
      line,
    );
    if (!stamp) continue;
    const timezone = `${stamp[2].slice(0, 3)}:${stamp[2].slice(3)}`;
    const milliseconds = Date.parse(`${stamp[1].replace(" ", "T")}${timezone}`);
    if (!Number.isFinite(milliseconds) || milliseconds < observedAfterSecond) {
      continue;
    }
    if (line.includes("Entering Sleep state")) {
      sleep = { milliseconds, line };
      continue;
    }
    if (sleep && line.includes("Wake from")) {
      const durationMilliseconds = milliseconds - sleep.milliseconds;
      if (durationMilliseconds >= minimumSleepMilliseconds) {
        return {
          sleptAt: new Date(sleep.milliseconds).toISOString(),
          wokeAt: new Date(milliseconds).toISOString(),
          durationMilliseconds,
          sleepLog: sleep.line,
          wakeLog: line,
        };
      }
    }
  }
  return null;
}

async function processExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function readLog(path) {
  return readFile(path, "utf8");
}

async function establishedSocketCount(pid, port) {
  try {
    const { stdout } = await execFileAsync("lsof", [
      "-nP",
      "-a",
      "-p",
      String(pid),
      "-iTCP",
    ]);
    return stdout
      .split(/\r?\n/)
      .filter(
        (line) =>
          line.includes(`->127.0.0.1:${port}`) &&
          line.includes("(ESTABLISHED)"),
      ).length;
  } catch (error) {
    if (error?.code === 1) return 0;
    throw error;
  }
}

async function waitForProviderTls(port, timeoutMilliseconds) {
  const deadline = Date.now() + timeoutMilliseconds;
  while (Date.now() < deadline) {
    const ready = await new Promise((resolve) => {
      const socket = tls.connect({
        host: "127.0.0.1",
        port,
        servername: "localhost",
        rejectUnauthorized: false,
      });
      const timer = setTimeout(() => socket.destroy(), 2_000);
      socket.once("secureConnect", () => {
        clearTimeout(timer);
        socket.end();
        resolve(true);
      });
      socket.once("error", () => {
        clearTimeout(timer);
        resolve(false);
      });
    });
    if (ready) return;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error("fictional SMTP provider did not recover after wake");
}

async function submitRecoveryMessage(port, messageId) {
  const socket = tls.connect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    rejectUnauthorized: false,
  });
  let buffered = "";
  let failure;
  socket.on("data", (chunk) => {
    buffered += chunk;
  });
  socket.on("error", (error) => {
    failure = error;
  });
  await Promise.race([
    once(socket, "secureConnect"),
    once(socket, "error").then(([error]) => Promise.reject(error)),
    new Promise((_, reject) =>
      setTimeout(
        () => reject(new Error("SMTP TLS handshake timed out")),
        5_000,
      ),
    ),
  ]);

  const readReply = async (expected) => {
    const deadline = Date.now() + 5_000;
    const lines = [];
    let code;
    while (Date.now() < deadline) {
      if (failure) throw failure;
      const newline = buffered.indexOf("\n");
      if (newline < 0) {
        await new Promise((resolve) => setTimeout(resolve, 10));
        continue;
      }
      const line = buffered.slice(0, newline + 1).replace(/\r?\n$/, "");
      buffered = buffered.slice(newline + 1);
      const match = /^(\d{3})([- ])/.exec(line);
      if (!match) throw new Error(`malformed SMTP reply: ${line}`);
      code ??= match[1];
      if (match[1] !== code) throw new Error(`mixed SMTP reply: ${line}`);
      lines.push(line);
      if (match[2] === " ") {
        if (code !== expected) {
          throw new Error(
            `expected SMTP ${expected}, got ${lines.join(" | ")}`,
          );
        }
        return;
      }
    }
    throw new Error(`SMTP ${expected} reply timed out`);
  };
  const command = async (value, expected) => {
    socket.write(`${value}\r\n`);
    await readReply(expected);
  };

  await readReply("220");
  await command("EHLO localhost", "250");
  await command(
    `AUTH PLAIN ${Buffer.from("\0fictional-reader@localhost\0synthetic-only-password").toString("base64")}`,
    "235",
  );
  await command("MAIL FROM:<wake-fixture@localhost>", "250");
  await command("RCPT TO:<fictional-reader@localhost>", "250");
  await command("DATA", "354");
  const message = [
    "From: Fictional Wake Fixture <wake-fixture@localhost>",
    "To: Fictional Reader <fictional-reader@localhost>",
    "Subject: Fictional mail received after wake",
    `Message-ID: <${messageId}@localhost>`,
    "Date: Sat, 17 Oct 2026 12:00:00 +0000",
    "MIME-Version: 1.0",
    "Content-Type: text/plain; charset=utf-8",
    "",
    "This isolated message proves that realtime mail recovered after macOS wake.",
  ].join("\r\n");
  socket.write(`${message}\r\n.\r\n`);
  await readReply("250");
  await command("QUIT", "221");
  socket.end();
}

async function waitForPowerCycle(
  observedAfterMilliseconds,
  minimumSleepMilliseconds,
  timeoutMilliseconds,
) {
  const deadline = Date.now() + timeoutMilliseconds;
  while (Date.now() < deadline) {
    const { stdout } = await execFileAsync("pmset", ["-g", "log"], {
      maxBuffer: 16 * 1024 * 1024,
    });
    const cycle = parsePowerCycle(
      stdout,
      observedAfterMilliseconds,
      minimumSleepMilliseconds,
    );
    if (cycle) return cycle;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error("no matching macOS sleep and wake transition was observed");
}

function quoteSql(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

async function durableMessageCount(database, messageId) {
  const rfcMessageId = `<${messageId}@localhost>`;
  const { stdout } = await execFileAsync("sqlite3", [
    "-readonly",
    database,
    `SELECT COUNT(*) FROM messages WHERE message_id = ${quoteSql(rfcMessageId)};`,
  ]);
  const count = Number(stdout.trim());
  if (!Number.isInteger(count) || count < 0) {
    throw new Error("SQLite returned invalid durable message evidence");
  }
  return count;
}

export async function observeNativeSleepWake({
  pid,
  appLog,
  database,
  imapPort,
  smtpPort,
  output,
  minimumSleepMilliseconds = 15_000,
  waitMilliseconds = 15 * 60_000,
  recoveryMilliseconds = 120_000,
}) {
  if (!(await processExists(pid)))
    throw new Error("fixture app PID is not running");
  const beforeLog = await readLog(appLog);
  const retriesBefore = countMarker(beforeLog, RETRY_MARKER);
  const arrivalsBefore = countMarker(beforeLog, ARRIVAL_MARKER);
  const socketsBefore = await establishedSocketCount(pid, imapPort);
  if (socketsBefore < 1) {
    throw new Error("fixture app has no established fictional IMAP socket");
  }

  process.stdout.write(
    `READY_FOR_SLEEP pid=${pid} imapSockets=${socketsBefore} retries=${retriesBefore}\n`,
  );
  const observationStartedAt = Date.now();
  const powerCycle = await waitForPowerCycle(
    observationStartedAt,
    minimumSleepMilliseconds,
    waitMilliseconds,
  );
  if (!(await processExists(pid)))
    throw new Error("fixture app stopped during sleep");
  await waitForProviderTls(smtpPort, 30_000);
  const recoveryStartedAt = Date.now();
  const messageId = `dakia-wake-${recoveryStartedAt}-${pid}`;
  await submitRecoveryMessage(smtpPort, messageId);

  let latestLog = beforeLog;
  let socketsAfter = 0;
  let durableRecoveryMessages = 0;
  const deadline = recoveryStartedAt + recoveryMilliseconds;
  while (Date.now() < deadline) {
    if (!(await processExists(pid)))
      throw new Error("fixture app stopped after wake");
    latestLog = await readLog(appLog);
    socketsAfter = await establishedSocketCount(pid, imapPort);
    durableRecoveryMessages = await durableMessageCount(database, messageId);
    if (
      socketsAfter >= 1 &&
      countMarker(latestLog, ARRIVAL_MARKER) > arrivalsBefore &&
      durableRecoveryMessages === 1
    ) {
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  const recoveryElapsedMilliseconds = Date.now() - recoveryStartedAt;
  const arrivalsAfter = countMarker(latestLog, ARRIVAL_MARKER);
  const retriesAfter = countMarker(latestLog, RETRY_MARKER);
  const retries = retryEvidence(
    retriesBefore,
    retriesAfter,
    recoveryElapsedMilliseconds,
  );
  const report = {
    schemaVersion: 1,
    lane: "native-fictional-sleep-wake",
    status:
      socketsAfter >= 1 &&
      arrivalsAfter > arrivalsBefore &&
      durableRecoveryMessages === 1 &&
      retries.bounded
        ? "passed"
        : "failed",
    recordedAt: new Date().toISOString(),
    appPid: pid,
    imapPort,
    smtpPort,
    powerCycle,
    recoveryElapsedMilliseconds,
    imapSocketsBefore: socketsBefore,
    imapSocketsAfter: socketsAfter,
    reconnectRetriesBefore: retriesBefore,
    reconnectRetriesAfter: retriesAfter,
    reconnectRetryDelta: retries.retryDelta,
    reconnectRetryLimit: retries.allowedRetries,
    arrivalsBefore,
    arrivalsAfter,
    durableRecoveryMessages,
    recoveryMessageId: messageId,
    limitations: [
      "The isolated GreenMail and Dakia development fixture resume on the same Mac.",
      "Visible Reader delivery remains a separate native assertion.",
      "This bounded cycle is not a multi-hour network or battery soak.",
    ],
  };
  await writeFile(output, `${JSON.stringify(report, null, 2)}\n`, "utf8");
  if (report.status !== "passed") {
    throw new Error(
      `sleep and wake recovery failed; evidence written to ${output}`,
    );
  }
  return report;
}

function usage() {
  return "Usage: observe-native-sleep-wake.mjs --metadata <path> --output <path>, or --pid <pid> --app-log <path> --database <path> --imap-port <port> --smtp-port <port> --output <path>";
}

async function main(argv) {
  const options = {};
  for (let index = 0; index < argv.length; index += 2) {
    const key = argv[index];
    const value = argv[index + 1];
    if (!value) throw new Error(usage());
    options[key] = value;
  }
  let metadata = {};
  if (options["--metadata"]) {
    metadata = JSON.parse(await readFile(options["--metadata"], "utf8"));
  }
  for (const required of [
    "pid",
    "appLog",
    "database",
    "imapPort",
    "smtpPort",
  ]) {
    if (
      metadata[required] == null &&
      !options[
        `--${required.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)}`
      ]
    ) {
      throw new Error(usage());
    }
  }
  for (const required of ["--output"]) {
    if (!options[required]) throw new Error(usage());
  }
  await observeNativeSleepWake({
    pid: Number(metadata.pid ?? options["--pid"]),
    appLog: metadata.appLog ?? options["--app-log"],
    database: metadata.database ?? options["--database"],
    imapPort: Number(metadata.imapPort ?? options["--imap-port"]),
    smtpPort: Number(metadata.smtpPort ?? options["--smtp-port"]),
    output: options["--output"],
  });
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv.slice(2)).catch((error) => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  });
}
