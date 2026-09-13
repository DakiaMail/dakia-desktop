import assert from "node:assert/strict";
import { connect } from "node:tls";
import test from "node:test";
import {
  fixtureCaDer,
  fixtureCaPem,
  fixtureUid,
  parseArgs,
  parseUidSet,
  startFixtureServers,
} from "./initial-sync-fixture-server.mjs";

test("fixture options are bounded and default to the 2000-message acceptance size", () => {
  assert.deepEqual(parseArgs([]), {
    messages: 2_000,
    folderMessages: 0,
    delayMs: 0,
    historyDelayMs: 0,
    sparse: false,
    writeCa: undefined,
    imapPort: 0,
    smtpPort: 0,
  });
  assert.deepEqual(
    parseArgs([
      "--messages",
      "30000",
      "--folder-messages",
      "25",
      "--delay-ms",
      "25",
      "--history-delay-ms",
      "5000",
      "--sparse",
      "--imap-port",
      "1143",
      "--smtp-port",
      "1465",
    ]),
    {
      messages: 30_000,
      folderMessages: 25,
      delayMs: 25,
      historyDelayMs: 5_000,
      sparse: true,
      writeCa: undefined,
      imapPort: 1_143,
      smtpPort: 1_465,
    },
  );
  assert.throws(() => parseArgs(["--messages", "100001"]), /1 to 100000/);
  assert.throws(() => parseArgs(["--folder-messages", "10001"]), /0 to 10000/);
  assert.throws(() => parseArgs(["--delay-ms", "30001"]), /0 to 30000/);
  assert.throws(() => parseArgs(["--history-delay-ms", "30001"]), /0 to 30000/);
  assert.throws(() => parseArgs(["--imap-port", "65536"]), /0 to 65535/);
});

test("sparse fixture UIDs are deterministic, increasing, and not sequence numbers", () => {
  const uids = Array.from({ length: 30 }, (_, index) =>
    fixtureUid(index + 1, true),
  );
  assert.equal(new Set(uids).size, uids.length);
  assert.ok(uids.every((uid, index) => index === 0 || uid > uids[index - 1]));
  assert.notDeepEqual(
    uids,
    Array.from({ length: 30 }, (_, index) => index + 1),
  );
});

test("UID sets preserve requested order for singletons and ranges", () => {
  assert.deepEqual(parseUidSet("9,3:5,8:7"), [9, 3, 4, 5, 8, 7]);
  assert.throws(() => parseUidSet("1:*"), /unsupported UID set/);
});

test("fixture CA is a real bounded DER certificate, not a secret", () => {
  assert.equal(fixtureCaDer[0], 0x30);
  assert.ok(fixtureCaDer.length > 300 && fixtureCaDer.length < 2_000);
});

function tlsConversation(port, commands, completion) {
  return new Promise((resolve, reject) => {
    const socket = connect({
      host: "127.0.0.1",
      port,
      ca: fixtureCaPem,
      servername: "localhost",
      rejectUnauthorized: true,
    });
    let transcript = "";
    let sent = false;
    socket.setEncoding("utf8");
    socket.on("error", reject);
    socket.on("data", (chunk) => {
      transcript += chunk;
      if (!sent) {
        sent = true;
        socket.write(commands);
      }
      if (completion.test(transcript)) {
        socket.end();
        resolve(transcript);
      }
    });
  });
}

test(
  "TLS fixture completes delayed IMAP and SMTP authentication probes before close",
  { timeout: 5_000 },
  async () => {
    const fixture = await startFixtureServers({
      messages: 10,
      delayMs: 25,
      sparse: true,
      writeCa: undefined,
    });
    try {
      const imap = await tlsConversation(
        fixture.imapPort,
        'A1 LOGIN "reader@example.test" "secret"\r\nA2 CAPABILITY\r\nA3 EXAMINE "INBOX"\r\nA4 FETCH 8:10 (UID FLAGS)\r\nA5 UID FETCH 30 (BODYSTRUCTURE)\r\nA6 STATUS "INBOX" (UIDVALIDITY UIDNEXT)\r\nA7 LOGOUT\r\n',
        /A7 OK logout/,
      );
      assert.match(imap, /\* 10 EXISTS/);
      assert.match(imap, /UIDVALIDITY 4242/);
      assert.match(imap, /UIDNEXT 31/);
      assert.match(imap, /UID 24 FLAGS/);
      assert.match(imap, /UID 30 FLAGS/);
      assert.match(imap, /BODYSTRUCTURE \("TEXT" "PLAIN"/);
      assert.ok(
        fixture.events.some(
          ({ protocol, line }) =>
            protocol === "imap" && line === "A1 LOGIN [redacted]",
        ),
      );
      assert.equal(
        fixture.events.some(({ line }) => line?.includes("secret")),
        false,
      );

      const smtp = await tlsConversation(
        fixture.smtpPort,
        "EHLO localhost\r\nAUTH PLAIN fixture\r\nQUIT\r\n",
        /221 2\.0\.0 bye/,
      );
      assert.match(smtp, /235 2\.7\.0 authenticated/);
      assert.ok(
        fixture.events.some(
          ({ protocol, line }) =>
            protocol === "smtp" && line === "AUTH [redacted]",
        ),
      );
      assert.deepEqual(
        {
          imapCommands: fixture.metrics.imap.commands,
          smtpCommands: fixture.metrics.smtp.commands,
        },
        { imapCommands: 7, smtpCommands: 3 },
      );
      assert.ok(fixture.metrics.imap.responseBytes > 0);
      assert.ok(fixture.metrics.smtp.responseBytes > 0);
      assert.ok(fixture.metrics.imap.maxResponseBytes > 0);
    } finally {
      await fixture.close();
    }
  },
);

test(
  "selected folders bound UID results and retain an appended Sent artifact",
  { timeout: 5_000 },
  async () => {
    const fixture = await startFixtureServers({
      messages: 4,
      folderMessages: 2,
      delayMs: 0,
      historyDelayMs: 0,
      sparse: false,
      writeCa: undefined,
    });
    const appended = [
      "Date: Wed, 30 Jul 2026 11:00:00 +0000",
      "From: Reader <reader@example.test>",
      "To: Recipient <recipient@example.test>",
      "Subject: Native fixture Sent artifact",
      "Message-ID: <native-sent@example.test>",
      "MIME-Version: 1.0",
      "Content-Type: multipart/mixed;",
      ' boundary="fixture-outer-boundary"',
      "",
      "--fixture-outer-boundary",
      "Content-Type: multipart/alternative;",
      ' boundary="fixture-alternative-boundary"',
      "",
      "--fixture-alternative-boundary",
      "Content-Type: text/plain; charset=utf-8",
      "Content-Transfer-Encoding: quoted-printable",
      "",
      "Retained fictional Sent content.",
      "--fixture-alternative-boundary",
      "Content-Type: text/html; charset=utf-8",
      "Content-Transfer-Encoding: quoted-printable",
      "",
      "<p>Retained fictional Sent content.</p>",
      "--fixture-alternative-boundary--",
      "--fixture-outer-boundary--",
    ].join("\r\n");
    try {
      const transcript = await tlsConversation(
        fixture.imapPort,
        [
          'A1 LOGIN "reader@example.test" "secret"',
          'A2 EXAMINE "Sent"',
          "A3 FETCH 1:2 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])",
          "A4 UID SEARCH ALL",
          "A5 UID FETCH 999 (BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
          `A6 APPEND "Sent" (\\Seen) {${Buffer.byteLength(appended)}}`,
          appended,
          'A7 EXAMINE "Sent"',
          'A8 UID SEARCH HEADER "Message-ID" "<native-sent@example.test>"',
          'A8B UID SEARCH HEADER "Message-ID" "<missing@example.test>"',
          "A9 UID FETCH 3 (FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])",
          "A9B UID FETCH 3 (UID BODYSTRUCTURE)",
          "A9C UID FETCH 3 (BODY.PEEK[1.1])",
          "A9D UID FETCH 3 (BODY.PEEK[1.1.MIME])",
          'A10 EXAMINE "INBOX"',
          "A11 FETCH 1:1 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])",
          'A12 STATUS "Archive" (UIDVALIDITY UIDNEXT)',
          "A13 LOGOUT",
          "",
        ].join("\r\n"),
        /A13 OK logout/,
      );
      assert.match(transcript, /\* 2 EXISTS/);
      assert.match(transcript, /Fictional Sent acceptance message 1/);
      assert.match(transcript, /<fixture-sent-1@example\.test>/);
      assert.match(transcript, /\* SEARCH 1 2\r\nA4 OK searched/);
      assert.doesNotMatch(transcript, /UID 999/);
      assert.match(transcript, /A6 OK \[APPENDUID 4242 3\] APPEND complete/);
      assert.match(transcript, /\* 3 EXISTS/);
      assert.match(transcript, /\* SEARCH 3\r\nA8 OK searched/);
      assert.match(transcript, /\* SEARCH \r\nA8B OK searched/);
      assert.match(transcript, /Message-ID: <native-sent@example\.test>/);
      assert.match(transcript, /UID 3 FLAGS \(\\Seen\)/);
      assert.match(
        transcript,
        /BODYSTRUCTURE \(\(\("TEXT" "PLAIN" \("CHARSET" "utf-8"\)[\s\S]+"TEXT" "HTML" \("CHARSET" "utf-8"\)[\s\S]+"ALTERNATIVE"[\s\S]+"MIXED"/,
      );
      assert.match(
        transcript,
        /BODY\[1\.1\] \{\d+\}\r\nRetained fictional Sent content\./,
      );
      assert.match(
        transcript,
        /BODY\[1\.1\.MIME\] \{\d+\}\r\nContent-Type: text\/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable/,
      );
      assert.match(transcript, /<fixture-1@example\.test>/);
      assert.match(
        transcript,
        /STATUS "Archive" \(UIDVALIDITY 4242 UIDNEXT 3\)/,
      );
      assert.ok(
        fixture.events.some(
          (event) =>
            event.event === "append" &&
            event.folder === "Sent" &&
            event.uid === 3 &&
            event.messageId === "<native-sent@example.test>",
        ),
      );
    } finally {
      await fixture.close();
    }
  },
);

test(
  "UID STORE flags persist in the selected mailbox and return through both fetch forms",
  { timeout: 5_000 },
  async () => {
    const fixture = await startFixtureServers({
      messages: 3,
      folderMessages: 1,
      delayMs: 0,
      historyDelayMs: 0,
      sparse: false,
      writeCa: undefined,
    });
    try {
      const transcript = await tlsConversation(
        fixture.imapPort,
        [
          'A1 LOGIN "reader@example.test" "secret"',
          'A2 SELECT "INBOX"',
          "A3 UID STORE 2 +FLAGS.SILENT (\\Seen)",
          "A4 UID FETCH 2 (FLAGS)",
          "A5 FETCH 2:2 (UID FLAGS)",
          'A6 SELECT "Sent"',
          "A7 UID FETCH 1 (FLAGS)",
          'A8 SELECT "INBOX"',
          "A9 UID STORE 2 -FLAGS (\\Seen)",
          "A10 UID FETCH 2 (FLAGS)",
          "A11 LOGOUT",
          "",
        ].join("\r\n"),
        /A11 OK logout/,
      );
      assert.match(transcript, /A3 OK stored/);
      assert.match(transcript, /UID 2 FLAGS \(\\Seen\)[\s\S]+A4 OK fetched/);
      assert.match(
        transcript,
        /\* 2 FETCH \(UID 2 FLAGS \(\\Seen\)[\s\S]+A5 OK fetched/,
      );
      assert.match(
        transcript,
        /A6 OK selected[\s\S]+UID 1 FLAGS \(\)[\s\S]+A7 OK fetched/,
      );
      assert.match(transcript, /A9 OK stored/);
      assert.match(
        transcript,
        /A9 OK stored[\s\S]+UID 2 FLAGS \(\)[\s\S]+A10 OK fetched/,
      );
      assert.deepEqual(
        fixture.events
          .filter(({ event }) => event === "store")
          .map(({ folder, uid, operation, flags }) => ({
            folder,
            uid,
            operation,
            flags,
          })),
        [
          { folder: "INBOX", uid: 2, operation: "+", flags: ["\\Seen"] },
          { folder: "INBOX", uid: 2, operation: "-", flags: [] },
        ],
      );
    } finally {
      await fixture.close();
    }
  },
);

test(
  "FETCH returns only requested metadata and supports reader body sections",
  { timeout: 5_000 },
  async () => {
    const fixture = await startFixtureServers({
      messages: 2_000,
      folderMessages: 0,
      delayMs: 0,
      historyDelayMs: 0,
      sparse: false,
      writeCa: undefined,
    });
    try {
      const transcript = await tlsConversation(
        fixture.imapPort,
        [
          'A1 LOGIN "reader@example.test" "secret"',
          'A2 SELECT "INBOX"',
          "A3 FETCH 1:2000 (UID FLAGS)",
          "A4 UID FETCH 2 (UID BODYSTRUCTURE)",
          "A5 UID FETCH 2 (BODY.PEEK[])",
          "A6 UID FETCH 2 (BODY.PEEK[1])",
          "A7 UID FETCH 2 (BODY.PEEK[1.MIME])",
          "A8 UID FETCH 2 (BODY.PEEK[1]<0.4>)",
          "A9 LOGOUT",
          "",
        ].join("\r\n"),
        /A9 OK logout/,
      );
      const flagsOnly = transcript.match(
        /A2 OK selected\r\n([\s\S]+?)A3 OK fetched/,
      )?.[1];
      assert.ok(flagsOnly);
      assert.doesNotMatch(flagsOnly, /\{\d+\}|INTERNALDATE|BODY\[/);
      assert.match(flagsOnly, /\* 2000 FETCH \(UID 2000 FLAGS \(\)\)/);

      const structure = transcript.match(
        /A3 OK fetched\r\n([\s\S]+?)A4 OK fetched/,
      )?.[1];
      assert.ok(structure);
      assert.match(structure, /UID 2 BODYSTRUCTURE \("TEXT" "PLAIN"/);
      assert.doesNotMatch(structure, /\{\d+\}|BODY\[/);

      assert.match(
        transcript,
        /A4 OK fetched[\s\S]+BODY\[\] \{\d+\}[\s\S]+Message-ID: <fixture-2@example\.test>[\s\S]+This is fictional message 2\./,
      );
      assert.match(
        transcript,
        /A5 OK fetched[\s\S]+BODY\[1\] \{\d+\}\r\nThis is fictional message 2\./,
      );
      assert.match(
        transcript,
        /A6 OK fetched[\s\S]+BODY\[1\.MIME\] \{\d+\}\r\nContent-Type: text\/plain; charset=utf-8/,
      );
      assert.match(
        transcript,
        /A7 OK fetched[\s\S]+BODY\[1\]<0> \{4\}\r\nThis\)/,
      );
    } finally {
      await fixture.close();
    }
  },
);
