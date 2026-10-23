import assert from "node:assert/strict";
import test from "node:test";
import {
  LargeMailboxSession,
  FIXTURE,
  fixtureRaw,
} from "./large-mailbox-imap-fixture.mjs";

function line(session, command) {
  return session.receive(`${command}\r\n`).join("");
}
function authenticated() {
  const session = new LargeMailboxSession();
  line(session, `a LOGIN "${FIXTURE.account}" "${FIXTURE.password}"`);
  line(session, 'b SELECT "INBOX"');
  return session;
}

test("requires the fictional account and exposes no mutating command", () => {
  const session = new LargeMailboxSession();
  assert.match(
    line(session, 'a LOGIN "fictional-reader@localhost" "wrong"'),
    /AUTHENTICATIONFAILED/,
  );
  assert.match(
    line(session, `b LOGIN "${FIXTURE.account}" "${FIXTURE.password}"`),
    /OK LOGIN/,
  );
  assert.match(
    line(session, 'c SELECT "Archive"'),
    /TRYCREATE|UNSUPPORTED|NONEXISTENT/,
  );
  line(session, 'd SELECT "INBOX"');
  assert.match(line(session, "e STORE 1 +FLAGS (\\Seen)"), /UNSUPPORTED/);
  assert.equal(
    session
      .transcript()
      .some((event) =>
        JSON.stringify(event).includes("synthetic-only-password"),
      ),
    false,
  );
});

test("serves exactly sixty descending read-neutral snapshot pages", () => {
  const session = authenticated();
  for (let page = 0; page < 60; page += 1) {
    const end = FIXTURE.messageCount - page * FIXTURE.pageSize;
    const start = end - FIXTURE.pageSize + 1;
    const output = line(session, `p${page} FETCH ${start}:${end} (UID FLAGS)`);
    assert.match(
      output,
      new RegExp(`\\* ${start} FETCH \\(UID ${start} FLAGS \\(\\\\Seen\\)\\)`),
    );
    assert.match(output, new RegExp(`p${page} OK FETCH completed`));
  }
  assert.equal(
    session.transcript().filter((event) => event.kind === "snapshot").length,
    60,
  );
  assert.match(line(session, "done FETCH 1:500 (UID FLAGS)"), /ORDER/);
});

test("keeps Reader MIME fixture bytes and multipart sections deterministic", () => {
  const session = authenticated();
  const metadata = line(
    session,
    "m UID FETCH 2 (FLAGS INTERNALDATE RFC822.SIZE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO SUBJECT MESSAGE-ID CONTENT-TYPE)])",
  );
  assert.match(metadata, /Fictional retained multipart Reader message/);
  assert.match(metadata, /BODYSTRUCTURE \(\("TEXT" "PLAIN"/);
  const body = line(session, "r UID FETCH 2 (BODY.PEEK[1])");
  assert.match(body, /Fictional retained multipart text\./);
  assert.equal(
    fixtureRaw(2).includes("Fictional retained multipart text."),
    true,
  );
});

test("audits every ingress verb including rejected commands without retaining secrets", () => {
  const session = new LargeMailboxSession();
  line(session, "a UID STORE 2 +FLAGS (\\Seen)");
  line(session, `b LOGIN "${FIXTURE.account}" "${FIXTURE.password}"`);
  line(session, 'c SELECT "INBOX"');
  line(session, "d UID STORE 2 +FLAGS (\\Seen)");
  line(session, "e NOOP");
  line(session, "malformed");
  const transcript = session.transcript();
  assert.equal(
    transcript.filter((event) => event.command === "UID STORE").length,
    4,
  );
  assert(
    transcript.some(
      (event) => event.kind === "rejected" && event.command === "NOOP",
    ),
  );
  assert(
    transcript.some(
      (event) => event.kind === "rejected" && event.command === "MALFORMED",
    ),
  );
  assert(!JSON.stringify(transcript).includes(FIXTURE.password));
  assert(!JSON.stringify(transcript).includes(FIXTURE.account));
});

test("partial body response preserves IMAP origin and literal byte size", () => {
  const session = authenticated();
  const response = line(session, "r UID FETCH 2 (BODY.PEEK[]<0.8192>)");
  assert.match(
    response,
    new RegExp(`BODY\\[\\]<0> \\{${Buffer.byteLength(fixtureRaw(2))}\\}`),
  );
  assert.equal(session.transcript().at(-1).kind, "snippet");
});

test("bounds partial commands before a newline arrives", () => {
  const session = authenticated();
  assert.match(
    session.receive("x".repeat(8193)).join(""),
    /exceeds fixture limit/,
  );
  assert.equal(session.transcript().at(-1).command, "OVERSIZED");
});

test("TLS server closes authenticated and incomplete-handshake sockets before audit finalization", async (t) => {
  const { execFileSync } = await import("node:child_process");
  const { mkdtemp, readFile, rm } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { connect: tlsConnect } = await import("node:tls");
  const { connect: netConnect } = await import("node:net");
  const { startLargeMailboxFixture } =
    await import("./large-mailbox-imap-fixture.mjs");
  const dir = await mkdtemp(join(tmpdir(), "dakia-large-mailbox-tls-test-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const keyPath = join(dir, "tls.key");
  const certPath = join(dir, "tls.crt");
  execFileSync(
    "openssl",
    [
      "req",
      "-x509",
      "-newkey",
      "rsa:2048",
      "-nodes",
      "-keyout",
      keyPath,
      "-out",
      certPath,
      "-days",
      "1",
      "-subj",
      "/CN=localhost",
      "-addext",
      "subjectAltName=DNS:localhost,IP:127.0.0.1",
    ],
    { stdio: "ignore" },
  );
  const sessions = [];
  const fixture = await startLargeMailboxFixture({
    keyPath,
    certPath,
    onSession: (session) => sessions.push(session),
  });
  t.after(() => fixture.close());
  const socket = tlsConnect({
    host: "127.0.0.1",
    port: fixture.port,
    servername: "localhost",
    ca: await readFile(certPath),
  });
  socket.on("error", () => {});
  t.after(() => socket.destroy());
  let received = "";
  const loggedIn = new Promise((resolveLoggedIn) =>
    socket.on("data", (data) => {
      received += data.toString();
      if (received.includes("a OK LOGIN")) resolveLoggedIn();
    }),
  );
  await new Promise((r, reject) => {
    socket.once("secureConnect", r);
    socket.once("error", reject);
  });
  socket.write(`a LOGIN "${FIXTURE.account}" "${FIXTURE.password}"\r\n`);
  await loggedIn;
  const incomplete = netConnect({ host: "127.0.0.1", port: fixture.port });
  incomplete.on("error", () => {});
  t.after(() => incomplete.destroy());
  await new Promise((r) => incomplete.once("connect", r));
  const started = Date.now();
  await fixture.close();
  assert(Date.now() - started < 2000);
  assert.equal(sessions.length, 1);
  assert(["TLSv1.2", "TLSv1.3"].includes(sessions[0].tlsProtocol));
  assert.equal(sessions[0].events.at(-1).kind, "login");
  assert(!JSON.stringify(sessions).includes(FIXTURE.password));
});
