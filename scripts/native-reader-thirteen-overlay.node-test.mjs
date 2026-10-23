#!/usr/bin/env node
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import {
  FIXTURES,
  classifyCommand,
  createSessionRoute,
  isFixtureRoute,
  loadFixtures,
  selectedHeaders,
  sha256,
  summarizeForwardedCommand,
  syntheticResponse,
  trackBackendRoute,
  trackClientRoute,
} from "./native-reader-thirteen-overlay.mjs";
import {
  createReplyReader,
  smtpDataBytes,
  waitForSecureConnect,
} from "./seed-native-reader-thirteen-fixtures.mjs";

const sourceRoot = resolve(process.argv[2] ?? process.cwd());
const fixtures = loadFixtures(sourceRoot);
assert.equal(fixtures.size, 13);
assert.deepEqual(
  [...fixtures.keys()],
  Array.from({ length: 13 }, (_, index) => index + 4),
);

function authenticatedInboxRoute(account = "fictional-reader@localhost") {
  const route = createSessionRoute();
  trackClientRoute(`L1 LOGIN "${account}" "fictional secret"\r\n`, route);
  trackBackendRoute("L1 OK authenticated\r\n", route);
  trackClientRoute('S1 EXAMINE "INBOX" (CONDSTORE)\r\n', route);
  trackBackendRoute("S1 OK selected\r\n", route);
  return route;
}

const fixtureRoute = authenticatedInboxRoute();
assert.equal(isFixtureRoute(fixtureRoute), true);

for (const [uid, expectedId, expectedPath, expectedSha] of FIXTURES) {
  const fixture = fixtures.get(uid);
  assert.equal(fixture.id, expectedId);
  assert.equal(fixture.relativePath, expectedPath);
  assert.equal(fixture.sha256, expectedSha);
  assert.equal(sha256(await readFile(fixture.path)), expectedSha);
  assert.equal(selectedHeaders(fixture.raw).compare(fixture.headers), 0);
  assert.match(fixture.headers.toString("latin1"), /(?:^|\r\n)Subject:/i);
  assert.equal(
    fixture.headers.includes(Buffer.from("Dakia native reader", "latin1")),
    false,
    "fixture headers must not be renamed for the harness",
  );

  const rawEvent = classifyCommand(
    `A${uid} UID FETCH ${uid} (FLAGS INTERNALDATE BODY.PEEK[])\r\n`,
    fixtures,
    fixtureRoute,
  );
  assert.deepEqual(rawEvent, { action: "raw", tag: `A${uid}`, uid });
  const rawResponse = syntheticResponse(rawEvent, fixtures);
  const literal = Buffer.from(`{${fixture.raw.length}}\r\n`);
  const start = rawResponse.indexOf(literal) + literal.length;
  assert(start >= literal.length);
  assert.equal(
    rawResponse
      .subarray(start, start + fixture.raw.length)
      .compare(fixture.raw),
    0,
  );
  assert.equal(
    sha256(rawResponse.subarray(start, start + fixture.raw.length)),
    expectedSha,
  );

  const headerSection =
    "HEADER.FIELDS (DATE FROM TO SUBJECT MESSAGE-ID CONTENT-TYPE)";
  const metadataEvent = classifyCommand(
    `B${uid} UID FETCH ${uid} (FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[${headerSection}])\r\n`,
    fixtures,
    fixtureRoute,
  );
  assert.deepEqual(metadataEvent, {
    action: "metadata",
    tag: `B${uid}`,
    uid,
    headerSection,
  });
  const metadataResponse = syntheticResponse(metadataEvent, fixtures);
  assert(metadataResponse.includes(fixture.headers));
  assert(metadataResponse.includes(Buffer.from(`BODY[${headerSection}]`)));
  assert(
    metadataResponse.includes(Buffer.from('BOUNDARY" "native-reader-parent')),
  );

  for (const section of ["1", "1.1", "2.3.4"]) {
    const mimeEvent = classifyCommand(
      `C${uid} UID FETCH ${uid} (BODY.PEEK[${section}.MIME])\r\n`,
      fixtures,
      fixtureRoute,
    );
    assert.deepEqual(mimeEvent, {
      action: "mime",
      tag: `C${uid}`,
      uid,
      section,
    });
    assert(
      syntheticResponse(mimeEvent, fixtures).includes(
        Buffer.from("repeated parent MIME forces production full-raw fallback"),
      ),
    );
  }

  const wire = smtpDataBytes(fixture.raw).toString("latin1");
  const rawSubject = fixture.headers
    .toString("latin1")
    .match(/(?:^|\r\n)(Subject:.*(?:\r\n[ \t].*)*)/i)?.[1];
  assert(
    rawSubject && wire.includes(rawSubject),
    `${expectedId} SMTP placeholder changed its Subject header`,
  );
  assert.equal(
    wire.includes("Message-ID: <dakia-native"),
    false,
    "seeder must not invent fixture Message-IDs",
  );
  assert(wire.endsWith("\r\n.\r\n"));
}

assert.deepEqual(
  classifyCommand(
    "Z UID FETCH 3 (FLAGS INTERNALDATE BODY.PEEK[])\r\n",
    fixtures,
  ).action,
  "forward",
);

const fixtureFetch = "R1 UID FETCH 4 (FLAGS INTERNALDATE BODY.PEEK[])\r\n";
assert.equal(
  classifyCommand(fixtureFetch, fixtures, createSessionRoute()).action,
  "forward",
  "a fixture-looking fetch must be forwarded before authentication",
);
assert.equal(
  classifyCommand(
    fixtureFetch,
    fixtures,
    authenticatedInboxRoute("fictional-reader-two@localhost"),
  ).action,
  "forward",
  "the other fictional account must not receive account A fixture bytes",
);
const wrongMailbox = authenticatedInboxRoute();
trackClientRoute('S2 SELECT "Archive"\r\n', wrongMailbox);
trackBackendRoute("S2 OK selected\r\n", wrongMailbox);
assert.equal(
  classifyCommand(fixtureFetch, fixtures, wrongMailbox).action,
  "forward",
  "fixture injection must be limited to INBOX",
);
const failedLogin = createSessionRoute();
trackClientRoute(
  'L2 LOGIN "fictional-reader@localhost" "fictional secret"\r\n',
  failedLogin,
);
trackBackendRoute("L2 NO rejected\r\n", failedLogin);
trackClientRoute("S3 SELECT INBOX\r\n", failedLogin);
trackBackendRoute("S3 OK selected\r\n", failedLogin);
assert.equal(isFixtureRoute(failedLogin), false);
assert.equal(
  classifyCommand(fixtureFetch, fixtures, failedLogin).action,
  "forward",
  "a failed LOGIN must never authorize fixture injection",
);
for (const verb of ["CLOSE", "UNSELECT"]) {
  const deselected = authenticatedInboxRoute();
  trackClientRoute(`D1 ${verb}\r\n`, deselected);
  trackBackendRoute("D1 OK deselected\r\n", deselected);
  assert.equal(isFixtureRoute(deselected), false);
  assert.equal(
    classifyCommand(fixtureFetch, fixtures, deselected).action,
    "forward",
  );
}
assert.equal(
  isFixtureRoute(createSessionRoute()),
  false,
  "a new connection must not inherit a previous connection's route",
);
assert.deepEqual(
  classifyCommand("Z UID STORE 4 +FLAGS (\\Seen)\r\n", fixtures).action,
  "forward",
);
const auth = summarizeForwardedCommand(
  "A1 LOGIN fictional-reader@localhost synthetic-only-password\r\n",
);
assert.deepEqual(auth, { kind: "command", tag: "A1", verb: "LOGIN" });
assert.equal(JSON.stringify(auth).includes("synthetic-only-password"), false);
const authenticate = summarizeForwardedCommand(
  "A2 AUTHENTICATE PLAIN AGZha2U=\r\n",
);
assert.deepEqual(authenticate, {
  kind: "command",
  tag: "A2",
  verb: "AUTHENTICATE",
});
assert.equal(JSON.stringify(authenticate).includes("AGZha2U="), false);
assert.throws(
  () => selectedHeaders(Buffer.from("Subject: no separator")),
  /header\/body separator/,
);
assert.throws(
  () => selectedHeaders(Buffer.from(" folded\nSubject: invalid\n\nbody")),
  /folded header continuation/,
);
assert.throws(
  () => selectedHeaders(Buffer.from("From: x\n\nbody")),
  /no Subject/,
);

class FakeSocket extends EventEmitter {
  write() {}
}
const multilineSocket = new FakeSocket();
const multilineReader = createReplyReader(multilineSocket, 100);
const multiline = multilineReader.expect("250");
multilineSocket.emit("data", "250-first\r\n250 done\r\n");
await multiline;
const errorSocket = new FakeSocket();
const errorReply = createReplyReader(errorSocket, 100).expect("220");
errorSocket.emit("error", new Error("fictional socket failure"));
await assert.rejects(errorReply, /fictional socket failure/);
const closeSocket = new FakeSocket();
const closeReply = createReplyReader(closeSocket, 100).expect("220");
closeSocket.emit("close");
await assert.rejects(closeReply, /closed before the expected reply/);
await assert.rejects(
  createReplyReader(new FakeSocket(), 5).expect("220"),
  /SMTP reply timed out/,
);
const handshakeSuccessSocket = new FakeSocket();
const handshakeSuccess = waitForSecureConnect(handshakeSuccessSocket, 100);
handshakeSuccessSocket.emit("secureConnect");
await handshakeSuccess;
const handshakeErrorSocket = new FakeSocket();
const handshakeError = waitForSecureConnect(handshakeErrorSocket, 100);
handshakeErrorSocket.emit("error", new Error("fictional TLS failure"));
await assert.rejects(handshakeError, /fictional TLS failure/);
await assert.rejects(
  waitForSecureConnect(new FakeSocket(), 5),
  /SMTP TLS handshake timed out/,
);

console.log(
  "native Reader thirteen-fixture overlay: 13 exact raw literals, faithful headers, fallback commands, invalid headers, credential-redacted logging, and bounded SMTP failure handling passed",
);
