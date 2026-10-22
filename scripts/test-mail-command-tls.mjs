import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { stopFixture } from "./fixture-process-lifecycle.mjs";

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

async function waitForPort(portFile, server) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (server.exitCode !== null)
      throw new Error("fictional mail-action IMAP server exited");
    try {
      const port = Number((await readFile(portFile, "utf8")).trim());
      if (Number.isInteger(port) && port > 0 && port < 65_536) return port;
    } catch {
      // The owned fixture has not published its loopback port yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("timed out waiting for fictional mail-action IMAP server");
}

async function startNoSmtpProbe() {
  let connectionCount = 0;
  const sockets = new Set();
  const listener = createServer((socket) => {
    connectionCount += 1;
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    socket.destroy();
  });
  listener.listen(0, "127.0.0.1");
  await once(listener, "listening");
  const address = listener.address();
  if (!address || typeof address === "string") {
    throw new Error("fictional no-SMTP probe did not bind a loopback port");
  }
  return {
    port: address.port,
    connectionCount: () => connectionCount,
    async close() {
      for (const socket of sockets) socket.destroy();
      if (!listener.listening) return;
      const closed = once(listener, "close");
      listener.close();
      await closed;
    },
  };
}

const root = await mkdtemp(join(tmpdir(), "dakia-mail-command-tls-"));
const nonce = randomUUID();
const profile = join(root, "profile");
const downloads = join(root, "Downloads");
const tls = join(root, "tls");
const portFile = join(root, "imap-port");
const transcript = join(root, "imap-transcript.jsonl");
const artifact = join(root, "tauri-mail-command.json");
let server;
let smtpProbe;

try {
  await Promise.all([mkdir(profile), mkdir(downloads), mkdir(tls)]);
  await Promise.all([
    writeFile(
      join(root, ".dakia-native-mail-safety-root"),
      "dakia-native-mail-safety-fixture-v1\n",
      { mode: 0o600 },
    ),
    writeFile(
      join(profile, ".dakia-native-mail-safety-profile"),
      "dakia-native-mail-safety-fixture-v1\n",
      { mode: 0o600 },
    ),
    writeFile(join(root, ".dakia-mail-command-tls"), `${nonce}\n`, {
      mode: 0o600,
    }),
  ]);
  await run(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-days",
      "1",
      "-subj",
      "/CN=Dakia mail command fixture CA",
      "-addext",
      "basicConstraints=critical,CA:true",
      "-addext",
      "keyUsage=critical,keyCertSign,cRLSign",
      "-keyout",
      join(tls, "ca.key"),
      "-out",
      join(tls, "ca.crt"),
    ],
    process.env,
  );
  await run(
    "openssl",
    [
      "req",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-subj",
      "/CN=localhost",
      "-keyout",
      join(tls, "server.key"),
      "-out",
      join(tls, "server.csr"),
    ],
    process.env,
  );
  await writeFile(
    join(tls, "server.ext"),
    [
      "[server]",
      "basicConstraints=critical,CA:false",
      "keyUsage=critical,digitalSignature,keyEncipherment",
      "extendedKeyUsage=serverAuth",
      "subjectAltName=DNS:localhost,IP:127.0.0.1",
      "",
    ].join("\n"),
  );
  await run(
    "openssl",
    [
      "x509",
      "-req",
      "-in",
      join(tls, "server.csr"),
      "-CA",
      join(tls, "ca.crt"),
      "-CAkey",
      join(tls, "ca.key"),
      "-CAcreateserial",
      "-days",
      "1",
      "-extfile",
      join(tls, "server.ext"),
      "-extensions",
      "server",
      "-out",
      join(tls, "server.crt"),
    ],
    process.env,
  );
  await run(
    "openssl",
    [
      "x509",
      "-in",
      join(tls, "ca.crt"),
      "-outform",
      "DER",
      "-out",
      join(tls, "ca.der"),
    ],
    process.env,
  );

  server = spawn(
    "node",
    [
      "scripts/mail-command-tls-fixture-server.mjs",
      "--cert",
      join(tls, "server.crt"),
      "--key",
      join(tls, "server.key"),
      "--port-file",
      portFile,
      "--transcript",
      transcript,
    ],
    { stdio: "inherit", env: process.env },
  );
  const port = await waitForPort(portFile, server);
  smtpProbe = await startNoSmtpProbe();
  const env = {
    ...process.env,
    DAKIA_MAIL_COMMAND_TLS_ROOT: root,
    DAKIA_MAIL_COMMAND_TLS_NONCE: nonce,
    DAKIA_MAIL_COMMAND_TLS_ARTIFACT: artifact,
    DAKIA_MAIL_COMMAND_TLS_TRANSCRIPT: transcript,
    DAKIA_NATIVE_MAIL_SAFETY_FIXTURE: "1",
    DAKIA_NATIVE_MAIL_SAFETY_ROOT: root,
    DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR: profile,
    DAKIA_NATIVE_MAIL_SAFETY_DOWNLOADS_DIR: downloads,
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_HOST: "127.0.0.1",
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_PORT: String(port),
    DAKIA_NATIVE_MAIL_SAFETY_IMAP_SERVER_NAME: "localhost",
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_HOST: "127.0.0.1",
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_PORT: String(smtpProbe.port),
    DAKIA_NATIVE_MAIL_SAFETY_SMTP_SERVER_NAME: "localhost",
    DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH: join(tls, "ca.der"),
    DAKIA_NATIVE_MAIL_SAFETY_PASSWORD: "synthetic-only-password",
  };
  delete env.DAKIA_NATIVE_MAIL_SAFETY_SECOND_ACCOUNT;
  await run(
    "cargo",
    [
      "test",
      "-p",
      "dakia-desktop",
      "--lib",
      "--locked",
      "--features",
      "native-mail-safety-fixture",
      "mail_command_tls_tests::production_tauri_mail_actions_use_only_the_exact_fictional_tls_locator",
      "--",
      "--exact",
    ],
    env,
  );
  if (smtpProbe.connectionCount() !== 0) {
    throw new Error("mail command TLS artifact unexpectedly contacted SMTP");
  }

  // The fixture serializes command records before each wire reply.  Stop and
  // await it before inspecting the transcript so a successful test cannot
  // be paired with a partially flushed oracle.
  await stopFixture(server);
  server = undefined;

  const evidence = JSON.parse(await readFile(artifact, "utf8"));
  const recorded = (await readFile(transcript, "utf8"))
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  if (
    evidence.nonce !== nonce ||
    evidence.staleRejected !== true ||
    evidence.staleSessionsBefore !== 8 ||
    evidence.staleSessionsBefore !== evidence.staleSessionsAfter ||
    evidence.providerFailureRetainedSource !== true
  ) {
    throw new Error(
      `Tauri mail-command evidence is incomplete: ${JSON.stringify(evidence)}`,
    );
  }
  const sessions = new Map();
  for (const event of recorded) {
    if (event.event === "opened") sessions.set(event.session, []);
    else if (event.event === "command") {
      const commands = sessions.get(event.session);
      if (!commands)
        throw new Error(`command appeared before session ${event.session}`);
      commands.push(event.command);
    } else {
      throw new Error(`unexpected transcript event: ${JSON.stringify(event)}`);
    }
  }
  const expectedSessions = [
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'SELECT "INBOX"',
      "UID STORE 11 +FLAGS.SILENT (\\Seen)",
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'SELECT "INBOX"',
      "UID STORE 12 +FLAGS.SILENT (\\Flagged)",
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'SELECT "INBOX"',
      "UID FETCH 12 (FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED)])",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'LIST "" "*"',
      'SELECT "INBOX"',
      'UID MOVE 21 "Archive"',
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'LIST "" "*"',
      'SELECT "INBOX"',
      'UID MOVE 22 "Spam"',
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'LIST "" "*"',
      'SELECT "Spam"',
      'UID MOVE 23 "INBOX"',
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'LIST "" "*"',
      'SELECT "INBOX"',
      'UID MOVE 24 "Trash"',
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      "CAPABILITY",
      'LIST "" "*"',
      'SELECT "INBOX"',
      "UID STORE 25 +FLAGS.SILENT (\\Deleted)",
      "UID EXPUNGE 25",
      "LOGOUT",
    ],
    [
      "CAPABILITY",
      'LOGIN "fictional-reader@localhost" "synthetic-only-password"',
      'LIST "" "*"',
      'SELECT "INBOX"',
      'UID MOVE 99 "Trash"',
      "LOGOUT",
    ],
  ];
  if (
    JSON.stringify([...sessions.values()]) !== JSON.stringify(expectedSessions)
  ) {
    throw new Error(
      `fictional provider sessions did not match the exact ordered transcript: ${JSON.stringify([...sessions.values()])}`,
    );
  }
  if (sessions.size !== expectedSessions.length) {
    throw new Error(
      "stale command opened an unexpected fictional provider session",
    );
  }
  if (smtpProbe.connectionCount() !== 0) {
    throw new Error("mail command TLS artifact unexpectedly contacted SMTP");
  }
} finally {
  await stopFixture(server);
  await smtpProbe?.close();
  if (process.env.DAKIA_KEEP_MAIL_COMMAND_TLS_ARTIFACTS === "1") {
    process.stderr.write(`Retained mail-command TLS artifacts: ${root}\n`);
  } else {
    await rm(root, { recursive: true, force: true });
  }
}
