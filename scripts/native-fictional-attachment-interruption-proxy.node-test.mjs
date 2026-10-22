import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { connect as tlsConnect, createServer } from "node:tls";
import { fileURLToPath } from "node:url";
import {
  LiteralTruncator,
  classifyFetchCommand,
  createAttachmentInterruptionOverlay,
  isMutatingImapCommand,
  requestControl,
  responseLiteralHeader,
  validateBinding,
} from "./native-fictional-attachment-interruption-proxy.mjs";

const sha = (value) => createHash("sha256").update(value).digest("hex");
const waitFor = (predicate, timeoutMs = 1_500) =>
  new Promise((resolve, reject) => {
    const deadline = Date.now() + timeoutMs;
    const tick = () =>
      predicate()
        ? resolve()
        : Date.now() >= deadline
          ? reject(new Error("timed out waiting for fixture state"))
          : setTimeout(tick, 10);
    tick();
  });

function testLifetime(t) {
  const clients = new Set();
  const overlays = new Set();
  const backends = new Set();
  t.after(async () => {
    for (const client of clients) client.destroy();
    await Promise.allSettled([...overlays].map((overlay) => overlay.close()));
    await Promise.allSettled([...backends].map((backend) => backend.close()));
  });
  return {
    backend: (backend) => (backends.add(backend), backend),
    connection: (connection) => {
      clients.add(connection.client);
      connection.client.once("close", () => clients.delete(connection.client));
      return connection;
    },
    overlay: (overlay) => (overlays.add(overlay), overlay),
  };
}

function ownedRoot() {
  const root = realpathSync(
    mkdtempSync(join(tmpdir(), "dakia-attachment-interruption-")),
  );
  chmodSync(root, 0o700);
  writeFileSync(
    join(root, ".dakia-native-mail-safety-root"),
    "dakia-native-mail-safety-fixture-v1\n",
    { mode: 0o600 },
  );
  return root;
}

function certificate(root) {
  const directory = join(root, "tls");
  mkdirSync(directory, { mode: 0o700 });
  const key = join(directory, "key.pem");
  const cert = join(directory, "cert.pem");
  execFileSync(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      key,
      "-out",
      cert,
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost",
      "-days",
      "1",
    ],
    { stdio: "ignore" },
  );
  chmodSync(key, 0o600);
  chmodSync(cert, 0o600);
  return { key, cert };
}

function fixture(root, { attachmentPrefix = "C" } = {}) {
  const raw = Buffer.from(`HH${attachmentPrefix}DEF`, "ascii");
  const rawPath = join(root, "fictional-message.eml");
  writeFileSync(rawPath, raw, { mode: 0o600 });
  const binding = {
    schemaVersion: 1,
    accountId: "fictional-account-a",
    email: "fictional-reader@localhost",
    mailbox: "INBOX",
    uid: 3,
    uidValidity: 19,
    messageId: "<fictional-attachment-3@localhost>",
    rawSha256: sha(raw),
    attachment: {
      id: "fictional-attachment:mime-v1:3",
      section: "3",
      rawOffset: 2,
      wirePrefixLength: attachmentPrefix.length,
      wirePrefixSha256: sha(attachmentPrefix),
      decodedSha256: sha("fictional decoded attachment"),
    },
  };
  const artifactPath = join(root, "production-full-fetch-artifact.json");
  const artifact = Buffer.from(`${JSON.stringify(binding)}\n`);
  writeFileSync(artifactPath, artifact, { mode: 0o600 });
  return { binding, rawPath, artifactPath, artifactSha256: sha(artifact) };
}

async function startBackend({
  cert,
  holdTarget = false,
  holdStallRemainder = false,
  stallTarget = false,
  targetHeader = "* 3 FETCH (UID 3 BODY[3] {4}\r\n",
}) {
  const commands = [];
  const sockets = new Set();
  let releaseTarget;
  const targetGate = new Promise((resolve) => {
    releaseTarget = resolve;
  });
  const server = createServer(
    {
      key: readFileSync(cert.key),
      cert: readFileSync(cert.cert),
      minVersion: "TLSv1.2",
    },
    (socket) => {
      sockets.add(socket);
      socket.once("close", () => sockets.delete(socket));
      socket.write("* OK fictional backend\r\n");
      let buffered = "";
      let account = "unknown";
      socket.on("data", async (chunk) => {
        buffered += chunk.toString("latin1");
        while (buffered.includes("\n")) {
          const end = buffered.indexOf("\n") + 1;
          const line = buffered.slice(0, end);
          buffered = buffered.slice(end);
          commands.push(line);
          const [tag, ...parts] = line.trim().split(/\s+/);
          const command = parts.join(" ");
          if (/^LOGIN\b/i.test(command)) {
            account = /fictional-reader-two@localhost/i.test(command)
              ? "other"
              : "bound";
            socket.write(`${tag} OK authenticated\r\n`);
          } else if (/^(?:SELECT|EXAMINE) "INBOX"$/i.test(command))
            socket.write(
              `* 1 EXISTS\r\n* OK [UIDVALIDITY 19] selected\r\n${tag} OK selected\r\n`,
            );
          else if (/^UID FETCH 3 \(BODY\.PEEK\[3\.MIME\]\)$/i.test(command))
            socket.write(`${tag} OK header\r\n`);
          else if (/^UID FETCH 4 \(BODY\.PEEK\[3\]\)$/i.test(command))
            socket.write(`${tag} OK wrong uid\r\n`);
          else if (/^UID FETCH 3 \(BODY\.PEEK\[3\]\)$/i.test(command)) {
            if (holdTarget && account !== "other") await targetGate;
            if (account === "other")
              socket.write(
                `* 3 FETCH (UID 3 BODY[3] {4}\r\nWXYZ\r\n)\r\n${tag} OK complete\r\n`,
              );
            else if (stallTarget) {
              const prefix = "C12345678901";
              const remainder = "R".repeat(32);
              socket.write(`* 3 FETCH (UID 3 BODY[3] {44}\r\n${prefix}`);
              if (holdStallRemainder) await targetGate;
              socket.write(`${remainder}\r\n)\r\n${tag} OK complete\r\n`);
            } else
              socket.write(
                `${targetHeader}CDEF\r\n)\r\n${tag} OK complete\r\n`,
              );
          } else if (
            /^UID FETCH 3 \(FLAGS INTERNALDATE BODY\.PEEK\[\]\)$/i.test(command)
          ) {
            if (holdTarget && account !== "other") await targetGate;
            socket.write(
              account === "other"
                ? `* 3 FETCH (UID 3 FLAGS () INTERNALDATE "22-Oct-2026 12:00:00 +0000" BODY[] {6}\r\nBBQ123\r\n)\r\n${tag} OK complete\r\n`
                : `* 3 FETCH (UID 3 FLAGS () INTERNALDATE "22-Oct-2026 12:00:00 +0000" BODY[] {6}\r\nHHCDEF\r\n)\r\n${tag} OK complete\r\n`,
            );
          } else socket.write(`${tag} OK forwarded\r\n`);
        }
      });
    },
  );
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return {
    commands,
    socketCount: () => sockets.size,
    closeConnections: () => {
      for (const socket of sockets) socket.destroy();
    },
    port: server.address().port,
    releaseTarget,
    close: () =>
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  };
}

async function openClient(port, certPath, { allowHalfOpen = false } = {}) {
  const client = tlsConnect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    ca: readFileSync(certPath),
    rejectUnauthorized: true,
    minVersion: "TLSv1.2",
    allowHalfOpen,
  });
  let received = Buffer.alloc(0);
  client.on("data", (chunk) => {
    received = Buffer.concat([received, chunk]);
  });
  await new Promise((resolve, reject) => {
    client.once("secureConnect", resolve);
    client.once("error", reject);
  });
  return { client, received: () => received };
}

async function authenticateAndSelect(connection) {
  connection.client.write(
    'A0 LOGIN "fictional-reader@localhost" "not-logged"\r\n',
  );
  await waitFor(() =>
    connection.received().toString("latin1").includes("A0 OK authenticated"),
  );
  connection.client.write('A1 SELECT "INBOX"\r\n');
  await waitFor(() =>
    connection.received().toString("latin1").includes("A1 OK selected"),
  );
}

async function authenticateAsOtherAndExamine(connection) {
  connection.client.write(
    'B0 LOGIN "fictional-reader-two@localhost" "synthetic-only-password"\r\n',
  );
  await waitFor(() =>
    connection.received().toString("latin1").includes("B0 OK authenticated"),
  );
  connection.client.write('B1 EXAMINE "INBOX"\r\n');
  await waitFor(() =>
    connection.received().toString("latin1").includes("B1 OK selected"),
  );
  connection.client.write(
    'B2 UID SEARCH HEADER MESSAGE-ID "<fictional-attachment-3@localhost>"\r\n',
  );
  await waitFor(() =>
    connection.received().toString("latin1").includes("B2 OK forwarded"),
  );
  connection.client.write("B3 UID FETCH 3 (UID FLAGS BODY.PEEK[])\r\n");
  await waitFor(() =>
    connection.received().toString("latin1").includes("B3 OK forwarded"),
  );
}

async function overlayFor(root, backendPort, cert, material, options = {}) {
  return createAttachmentInterruptionOverlay({
    rootPath: root,
    backendPort,
    certificate: cert.cert,
    key: cert.key,
    ca: cert.cert,
    accountId: material.binding.accountId,
    bindingArtifactPath: material.artifactPath,
    bindingArtifactSha256: material.artifactSha256,
    fixtureRawPath: material.rawPath,
    ...options,
  });
}

test("binding, target selection, and mutation verbs are narrow", () => {
  const material = fixture(ownedRoot());
  const { binding } = material;
  assert.deepEqual(validateBinding(binding), binding);
  assert.throws(
    () => validateBinding({ ...binding, email: "not-an-email" }),
    /email/,
  );
  assert.equal(
    classifyFetchCommand("A1 UID FETCH 3 (BODY.PEEK[3])\r\n", binding).kind,
    "single",
  );
  assert.equal(
    classifyFetchCommand("A2 UID FETCH 4 (BODY.PEEK[3])\r\n", binding).kind,
    "other",
  );
  for (const verb of [
    "STORE",
    "APPEND",
    "COPY",
    "MOVE",
    "EXPUNGE",
    "DELETE",
    "CREATE",
    "RENAME",
    "SUBSCRIBE",
    "UNSUBSCRIBE",
    "CLOSE",
  ])
    assert.equal(isMutatingImapCommand(`A4 ${verb} x\r\n`), true, verb);
});

test("literal framing verifies attachment bytes and never truncates a header", () => {
  assert.equal(
    responseLiteralHeader(Buffer.from("* 3 FETCH (BODY[3] {4}\r\n")).length,
    4,
  );
  const truncator = new LiteralTruncator({
    attachmentOffset: 0,
    attachmentPrefixLength: 1,
    attachmentPrefixSha256: sha("A"),
  });
  assert.deepEqual(
    truncator.consume(Buffer.from("* 3 FETCH (UID 3 BODY[3] {4}\r\n")),
    { forward: Buffer.alloc(0), close: false },
  );
  assert.match(
    truncator.consume(Buffer.from("ABCD")).forward.toString("latin1"),
    /\{4\}\r\nA$/,
  );
  assert.throws(
    () =>
      new LiteralTruncator({
        attachmentOffset: 0,
        attachmentPrefixLength: 1,
        attachmentPrefixSha256: "0".repeat(64),
      }).consume(Buffer.from("* 3 FETCH (UID 3 BODY[3] {4}\r\nABCD")),
    /bound attachment prefix/,
  );
});

test("sealed artifact, raw fixture, and TLS paths reject a symlinked parent", async () => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const outside = realpathSync(
    mkdtempSync(join(tmpdir(), "dakia-attachment-outside-")),
  );
  chmodSync(outside, 0o700);
  const copiedArtifact = join(outside, "artifact.json");
  const copiedRaw = join(outside, "raw.eml");
  const copiedCert = join(outside, "cert.pem");
  writeFileSync(copiedArtifact, readFileSync(material.artifactPath), {
    mode: 0o600,
  });
  writeFileSync(copiedRaw, readFileSync(material.rawPath), { mode: 0o600 });
  writeFileSync(copiedCert, readFileSync(cert.cert), { mode: 0o600 });
  symlinkSync(outside, join(root, "escaped-parent"));
  const base = {
    rootPath: root,
    backendPort: 9,
    certificate: cert.cert,
    key: cert.key,
    ca: cert.cert,
    accountId: material.binding.accountId,
    bindingArtifactSha256: material.artifactSha256,
  };
  await assert.rejects(
    () =>
      createAttachmentInterruptionOverlay({
        ...base,
        bindingArtifactPath: join(root, "escaped-parent", "artifact.json"),
        fixtureRawPath: material.rawPath,
      }),
    /symbolic link/,
  );
  await assert.rejects(
    () =>
      createAttachmentInterruptionOverlay({
        ...base,
        bindingArtifactPath: material.artifactPath,
        fixtureRawPath: join(root, "escaped-parent", "raw.eml"),
      }),
    /symbolic link/,
  );
  await assert.rejects(
    () =>
      createAttachmentInterruptionOverlay({
        ...base,
        certificate: join(root, "escaped-parent", "cert.pem"),
        bindingArtifactPath: material.artifactPath,
        fixtureRawPath: material.rawPath,
      }),
    /symbolic link/,
  );
});

test("a multiple-body response cannot select the wrong literal", async (t) => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(
    await startBackend({
      cert,
      targetHeader: "* 3 FETCH (UID 3 BODY[3] NIL BODY[9] {4}\r\n",
    }),
  );
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material),
  );
  const connection = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(connection);
  requestControl(root, "arm-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed,
  );
  connection.client.write("A2 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await new Promise((resolve) => connection.client.once("close", resolve));
  const state = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  );
  assert.equal(state.attempts.length, 0);
  assert.match(state.lastCommandError, /UID and body section/);
  assert.doesNotMatch(connection.received().toString("latin1"), /BODY\[9\]/);
  await overlay.close();
  await backend.close();
});

test("selective save delivers its prefix exactly once and retry passes through", async (t) => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(await startBackend({ cert }));
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material),
  );
  const first = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(first);
  requestControl(root, "arm-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.kind === "single",
  );
  first.client.write("A2 UID FETCH 3 (BODY.PEEK[3.MIME])\r\n");
  await waitFor(() =>
    first.received().toString("latin1").includes("A2 OK header"),
  );
  first.client.write("A3 UID FETCH 4 (BODY.PEEK[3])\r\n");
  await waitFor(() =>
    first.received().toString("latin1").includes("A3 OK wrong uid"),
  );
  first.client.write("A4 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await new Promise((resolve) => first.client.once("close", resolve));
  const literal = "* 3 FETCH (UID 3 BODY[3] {4}\r\n";
  const text = first.received().toString("latin1");
  assert.equal(text.slice(text.indexOf(literal)), `${literal}C`);
  assert.equal(
    JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
      .attempts.length,
    1,
  );
  if (process.env.DAKIA_FIXTURE_INTENTIONAL_FAILURE === "1")
    assert.fail("intentional cleanup sentinel");
  const retry = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(retry);
  retry.client.write("A5 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(() => retry.received().toString("latin1").includes("CDEF"));
  retry.client.destroy();
  await overlay.close();
  await backend.close();
});

test("save all reaches the verified attachment offset before close", async (t) => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(await startBackend({ cert }));
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material),
  );
  const connection = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(connection);
  requestControl(root, "arm-all");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.kind === "all",
  );
  connection.client.write(
    "A7 UID FETCH 3 (FLAGS INTERNALDATE BODY.PEEK[])\r\n",
  );
  await new Promise((resolve) => connection.client.once("close", resolve));
  const literal =
    '* 3 FETCH (UID 3 FLAGS () INTERNALDATE "22-Oct-2026 12:00:00 +0000" BODY[] {6}\r\n';
  const text = connection.received().toString("latin1");
  assert.equal(text.slice(text.indexOf(literal)), `${literal}HHC`);
  assert.equal(
    JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
      .attempts[0].deliveredLiteralBytes,
    3,
  );
  await overlay.close();
  await backend.close();
});

test("a stalled single attachment records client EOF while another account passes through and mutations remain blocked", async (t) => {
  const root = ownedRoot();
  const material = fixture(root, { attachmentPrefix: "C12345678901" });
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(
    await startBackend({ cert, stallTarget: true, holdStallRemainder: true }),
  );
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material, {
      stallDeadlineMs: 1_000,
    }),
  );
  const bound = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(bound);
  requestControl(root, "arm-stall-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.stall === true,
  );
  bound.client.write("A2 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.pending?.deliveredLiteralBytes === 12,
  );
  const literal = "* 3 FETCH (UID 3 BODY[3] {44}\r\n";
  await waitFor(() =>
    bound.received().toString("latin1").includes(`${literal}C12345678901`),
  );
  const pending = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  ).stall.pending;
  assert.equal(pending.announcedLiteralBytes, 44);
  assert.equal(pending.deliveredLiteralBytes, 12);
  assert.equal(pending.withheldLiteralBytes, 32);
  assert.ok(Date.parse(pending.stalledAt));
  assert.ok(pending.protocolCounts.clientCommands >= 3);
  assert.ok(pending.protocolCounts.backendChunks >= 1);

  const other = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAsOtherAndExamine(other);
  other.client.write("B4 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(() =>
    other.received().toString("latin1").includes("B4 OK complete"),
  );
  assert.ok(
    other.received().toString("latin1").includes("BODY[3] {4}\r\nWXYZ"),
  );
  other.client.write("B5 UID STORE 3 +FLAGS (\\Seen)\r\n");
  await new Promise((resolve) => other.client.once("close", resolve));
  assert.equal(
    backend.commands.filter((line) => /UID STORE/i.test(line)).length,
    0,
  );

  bound.client.end();
  await new Promise((resolve) => bound.client.once("close", resolve));
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.closed.length === 1,
  );
  await waitFor(() => backend.socketCount() === 0);
  const state = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  );
  const closed = state.stall.closed[0];
  assert.equal(state.stall.pending, null);
  assert.equal(closed.outcome, "client-eof");
  assert.equal(closed.error, null);
  assert.equal(closed.announcedLiteralBytes, 44);
  assert.equal(closed.deliveredLiteralBytes, 12);
  assert.ok(Date.parse(closed.closedAt));
  assert.equal(closed.protocolCounts.clientEofs, 1);
});

test("a stalled attachment deadline is recorded as a fixture deadline instead of client EOF", async (t) => {
  const root = ownedRoot();
  const material = fixture(root, { attachmentPrefix: "C12345678901" });
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(await startBackend({ cert, stallTarget: true }));
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material, {
      stallDeadlineMs: 25,
    }),
  );
  const bound = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(bound);
  requestControl(root, "arm-stall-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.stall === true,
  );
  bound.client.write("A2 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await new Promise((resolve) => bound.client.once("close", resolve));
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.closed.length === 1,
  );
  await waitFor(() => backend.socketCount() === 0);
  const state = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  );
  const closed = state.stall.closed[0];
  assert.equal(closed.outcome, "fixture-deadline");
  assert.equal(
    closed.error,
    "fixture stall deadline elapsed before client EOF",
  );
  assert.match(state.lastCommandError, /fixture stall deadline/);
  assert.notEqual(closed.outcome, "client-eof");
});

test("an upstream close while stalled is recorded as a server error", async (t) => {
  const root = ownedRoot();
  const material = fixture(root, { attachmentPrefix: "C12345678901" });
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(
    await startBackend({ cert, stallTarget: true, holdStallRemainder: true }),
  );
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material, {
      stallDeadlineMs: 1_000,
    }),
  );
  const bound = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(bound);
  requestControl(root, "arm-stall-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.stall === true,
  );
  bound.client.write("A2 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.pending,
  );
  backend.closeConnections();
  await new Promise((resolve) => bound.client.once("close", resolve));
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.closed.length === 1,
  );
  const closed = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  ).stall.closed[0];
  assert.equal(closed.outcome, "server-error");
  assert.match(closed.error, /upstream TLS connection closed/);
  assert.equal(closed.protocolCounts.backendEofs, 1);
});

test("explicit fixture shutdown records fixture-closed before destroying stalled sockets", async (t) => {
  const root = ownedRoot();
  const material = fixture(root, { attachmentPrefix: "C12345678901" });
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(
    await startBackend({ cert, stallTarget: true, holdStallRemainder: true }),
  );
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material, {
      stallDeadlineMs: 1_000,
    }),
  );
  const bound = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(bound);
  requestControl(root, "arm-stall-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed?.stall === true,
  );
  bound.client.write("A2 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .stall.pending,
  );
  const clientClosed = new Promise((resolve) =>
    bound.client.once("close", resolve),
  );
  await overlay.close();
  await clientClosed;
  await waitFor(() => backend.socketCount() === 0);
  const closed = JSON.parse(
    readFileSync(join(root, "attachment-interruption-state.json")),
  ).stall.closed[0];
  assert.equal(closed.outcome, "fixture-closed");
  assert.match(closed.error, /fixture closed/);
  assert.notEqual(closed.outcome, "server-error");
});

for (const [kind, control, command, expectedBody] of [
  [
    "single",
    "arm-single",
    "B4 UID FETCH 3 (BODY.PEEK[3])\r\n",
    "BODY[3] {4}\r\nWXYZ",
  ],
  [
    "all",
    "arm-all",
    "B4 UID FETCH 3 (FLAGS INTERNALDATE BODY.PEEK[])\r\n",
    "BODY[] {6}\r\nBBQ123",
  ],
]) {
  test(`an authenticated other account passes through an armed ${kind} fetch without consuming it`, async (t) => {
    const root = ownedRoot();
    const material = fixture(root);
    const cert = certificate(root);
    const life = testLifetime(t);
    const backend = life.backend(await startBackend({ cert }));
    const overlay = life.overlay(
      await overlayFor(root, backend.port, cert, material),
    );
    const other = life.connection(await openClient(overlay.port, cert.cert));
    await authenticateAsOtherAndExamine(other);
    requestControl(root, control);
    await waitFor(
      () =>
        JSON.parse(
          readFileSync(join(root, "attachment-interruption-state.json")),
        ).armed?.kind === kind,
    );
    const stateBefore = readFileSync(
      join(root, "attachment-interruption-state.json"),
      "utf8",
    );
    const transcriptBefore = readFileSync(
      join(root, "attachment-interruption-transcript.jsonl"),
      "utf8",
    );
    other.client.write(command);
    await waitFor(() =>
      other.received().toString("latin1").includes(`B4 OK complete`),
    );
    assert.ok(other.received().toString("latin1").includes(expectedBody));
    assert.equal(
      readFileSync(join(root, "attachment-interruption-state.json"), "utf8"),
      stateBefore,
    );
    assert.equal(
      readFileSync(
        join(root, "attachment-interruption-transcript.jsonl"),
        "utf8",
      ),
      transcriptBefore,
    );

    const bound = life.connection(await openClient(overlay.port, cert.cert));
    await authenticateAndSelect(bound);
    const boundCommand = command.replace(/^B4/, "A4");
    bound.client.write(boundCommand);
    await new Promise((resolve) => bound.client.once("close", resolve));
    const attempt = JSON.parse(
      readFileSync(join(root, "attachment-interruption-state.json")),
    ).attempts;
    assert.equal(attempt.length, 1);
    assert.equal(attempt[0].kind, kind);

    const mutation = life.connection(await openClient(overlay.port, cert.cert));
    await authenticateAsOtherAndExamine(mutation);
    mutation.client.write("B5 UID STORE 3 +FLAGS (\\Seen)\r\n");
    await new Promise((resolve) => mutation.client.once("close", resolve));
    assert.equal(
      backend.commands.filter((line) => /UID STORE/i.test(line)).length,
      0,
    );
  });
}

for (const [name, prepare] of [
  ["unauthenticated", async () => {}],
  [
    "bound but not selected",
    async (connection) => {
      connection.client.write(
        'A0 LOGIN "fictional-reader@localhost" "synthetic-only-password"\r\n',
      );
      await waitFor(() =>
        connection
          .received()
          .toString("latin1")
          .includes("A0 OK authenticated"),
      );
    },
  ],
]) {
  test(`an armed fetch from an ${name} session fails closed`, async (t) => {
    const root = ownedRoot();
    const material = fixture(root);
    const cert = certificate(root);
    const life = testLifetime(t);
    const backend = life.backend(await startBackend({ cert }));
    const overlay = life.overlay(
      await overlayFor(root, backend.port, cert, material),
    );
    const connection = life.connection(
      await openClient(overlay.port, cert.cert),
    );
    await prepare(connection);
    requestControl(root, "arm-single");
    await waitFor(
      () =>
        JSON.parse(
          readFileSync(join(root, "attachment-interruption-state.json")),
        ).armed?.kind === "single",
    );
    connection.client.write("A1 UID FETCH 3 (BODY.PEEK[3])\r\n");
    await new Promise((resolve) => connection.client.once("close", resolve));
    const state = JSON.parse(
      readFileSync(join(root, "attachment-interruption-state.json")),
    );
    assert.equal(state.armed, null);
    assert.match(state.lastCommandError, /mailbox epoch were verified/);
    assert.equal(
      backend.commands.filter((line) => line.includes("BODY.PEEK[3])")).length,
      0,
    );
  });
}

test("a bound second fetch cannot consume an in-flight attempt while an authenticated other account passes through", async (t) => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(await startBackend({ cert, holdTarget: true }));
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material),
  );
  const first = life.connection(
    await openClient(overlay.port, cert.cert, { allowHalfOpen: true }),
  );
  await authenticateAndSelect(first);
  requestControl(root, "arm-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .armed,
  );
  first.client.write("A4 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .inFlight,
  );
  const other = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAsOtherAndExamine(other);
  other.client.write("B4 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await waitFor(() =>
    other.received().toString("latin1").includes("B4 OK complete"),
  );
  assert.ok(
    other.received().toString("latin1").includes("BODY[3] {4}\r\nWXYZ"),
  );
  const second = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(second);
  second.client.write("A5 UID FETCH 3 (BODY.PEEK[3])\r\n");
  await new Promise((resolve) => second.client.once("close", resolve));
  assert.equal(
    backend.commands.filter((line) => line.includes("BODY.PEEK[3])")).length,
    2,
  );
  backend.releaseTarget();
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .attempts.length === 1,
  );
  await Promise.race([
    overlay.close(),
    new Promise((_, reject) =>
      setTimeout(
        () => reject(new Error("overlay close hung on a held peer")),
        1_000,
      ),
    ),
  ]);
  await backend.close();
});

test("the read-only window begins at arm and excludes credentials from the transcript", async (t) => {
  const root = ownedRoot();
  const material = fixture(root);
  const cert = certificate(root);
  const life = testLifetime(t);
  const backend = life.backend(await startBackend({ cert }));
  const overlay = life.overlay(
    await overlayFor(root, backend.port, cert, material),
  );
  const beforeArm = life.connection(await openClient(overlay.port, cert.cert));
  beforeArm.client.write("A0 UID STORE 3 +FLAGS (\\Seen)\r\n");
  await waitFor(() =>
    backend.commands.some((line) => line.includes("UID STORE")),
  );
  beforeArm.client.destroy();
  const guarded = life.connection(await openClient(overlay.port, cert.cert));
  await authenticateAndSelect(guarded);
  requestControl(root, "arm-single");
  await waitFor(
    () =>
      JSON.parse(readFileSync(join(root, "attachment-interruption-state.json")))
        .readOnlyWindow,
  );
  guarded.client.write("A2 RENAME old new\r\n");
  await new Promise((resolve) => guarded.client.once("close", resolve));
  assert.equal(
    backend.commands.filter((line) => /RENAME/i.test(line)).length,
    0,
  );
  assert.doesNotMatch(
    readFileSync(
      join(root, "attachment-interruption-transcript.jsonl"),
      "utf8",
    ),
    /not-logged|LOGIN/i,
  );
  assert.equal(
    existsSync(join(root, "attachment-interruption-command.json")),
    false,
  );
  await overlay.close();
  await backend.close();
});

test("an intentional assertion failure exits the TLS child without a hanging fixture", () => {
  const startedAt = Date.now();
  const { NODE_TEST_CONTEXT: _nodeTestContext, ...childEnvironment } =
    process.env;
  const result = spawnSync(
    process.execPath,
    [
      "--test",
      "--test-name-pattern=selective save",
      fileURLToPath(import.meta.url),
    ],
    {
      env: { ...childEnvironment, DAKIA_FIXTURE_INTENTIONAL_FAILURE: "1" },
      encoding: "utf8",
      timeout: 4_000,
    },
  );
  assert.equal(result.status, 1, result.stdout || result.stderr);
  assert.equal(result.signal, null);
  assert.ok(Date.now() - startedAt < 4_000);
});
