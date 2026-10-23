/**
 * A deterministic, loopback-only IMAP/TLS fixture for the large-mailbox
 * smoke. It intentionally implements only read-neutral commands exercised by
 * Dakia's snapshot and Reader paths. It never sends, stores, appends, or
 * connects outside the process.
 */
import { createServer } from "node:tls";
import { readFile, writeFile } from "node:fs/promises";

export const FIXTURE = Object.freeze({
  account: "fictional-reader@localhost",
  password: "synthetic-only-password",
  mailbox: "INBOX",
  uidValidity: 77,
  messageCount: 30_000,
  pageSize: 500,
});

const CRLF = "\r\n";
const retained = Object.freeze({
  1: Object.freeze({
    raw: "Date: Thu, 30 Jul 2026 09:00:00 +0000\r\nFrom: Fixture Sender <sender@fixture.invalid>\r\nTo: Fictional Reader <fictional-reader@localhost>\r\nMessage-ID: <fixture-html@fixture.invalid>\r\nSubject: Fictional retained HTML Reader message\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<p>Fictional retained Reader HTML body.</p>\r\n",
    date: "30-Jul-2026 09:00:00 +0000",
    structure: '("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "7BIT" 45 1)',
  }),
  2: Object.freeze({
    raw: "Date: Thu, 30 Jul 2026 09:01:00 +0000\r\nFrom: Fixture Sender <sender@fixture.invalid>\r\nTo: Fictional Reader <fictional-reader@localhost>\r\nMessage-ID: <fixture-mixed@fixture.invalid>\r\nSubject: Fictional retained multipart Reader message\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=retained-two\r\n\r\n--retained-two\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\nFictional retained multipart text.\r\n--retained-two--\r\n",
    date: "30-Jul-2026 09:01:00 +0000",
    structure:
      '(("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 36 1) "MIXED" ("BOUNDARY" "retained-two") NIL NIL)',
  }),
  3: Object.freeze({
    raw: "Date: Thu, 30 Jul 2026 09:02:00 +0000\r\nFrom: Fixture Sender <sender@fixture.invalid>\r\nTo: Fictional Reader <fictional-reader@localhost>\r\nMessage-ID: <fixture-related@fixture.invalid>\r\nSubject: Fictional retained related Reader message\r\nMIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=retained-three\r\n\r\n--retained-three\r\nContent-Type: text/html; charset=UTF-8\r\n\r\n<p>Fictional retained related body.</p>\r\n--retained-three--\r\n",
    date: "30-Jul-2026 09:02:00 +0000",
    structure:
      '(("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "7BIT" 41 1) "RELATED" ("BOUNDARY" "retained-three") NIL NIL)',
  }),
});

export function fixtureRaw(uid) {
  if (retained[uid]) return retained[uid].raw;
  if (!Number.isInteger(uid) || uid < 4 || uid > FIXTURE.messageCount)
    throw new RangeError("fixture UID is out of bounds");
  return [
    "Date: Thu, 30 Jul 2026 10:00:00 +0000",
    `From: Fixture Sender ${uid} <sender-${uid}@fixture.invalid>`,
    `To: Fictional Reader <${FIXTURE.account}>`,
    `Message-ID: <fixture-${uid}@fixture.invalid>`,
    `Subject: Fictional large mailbox ${uid}`,
    "MIME-Version: 1.0",
    "Content-Type: text/plain; charset=UTF-8",
    "",
    `Fictional large mailbox background body ${uid}.`,
    "",
  ].join(CRLF);
}

function headerFields(raw, names) {
  const wanted = new Set(
    names
      .trim()
      .split(/\s+/)
      .map((name) => name.toLowerCase()),
  );
  const headers = raw.slice(0, raw.indexOf(`${CRLF}${CRLF}`));
  const result = [];
  let field = [];
  for (const line of headers.split(CRLF)) {
    if (/^[ \t]/.test(line) && field.length) field.push(line);
    else {
      if (
        field.length &&
        wanted.has(field[0].slice(0, field[0].indexOf(":")).toLowerCase())
      )
        result.push(...field);
      field = [line];
    }
  }
  if (
    field.length &&
    wanted.has(field[0].slice(0, field[0].indexOf(":")).toLowerCase())
  )
    result.push(...field);
  return `${result.join(CRLF)}${CRLF}${CRLF}`;
}

function literal(sequence, fields, value) {
  return `* ${sequence} FETCH (${fields} {${Buffer.byteLength(value)}}${CRLF}${value})${CRLF}`;
}

function tagged(tag, status, message) {
  return `${tag} ${status} ${message}${CRLF}`;
}

export class LargeMailboxSession {
  #buffer = "";
  #authenticated = false;
  #selected = false;
  #nextPageStart = FIXTURE.messageCount - FIXTURE.pageSize + 1;
  #events = [];

  greeting() {
    return `* OK Dakia fictional ${FIXTURE.messageCount}-message IMAP fixture ready${CRLF}`;
  }
  transcript() {
    return [...this.#events];
  }
  receive(chunk) {
    this.#buffer += chunk;
    const replies = [];
    while (this.#buffer.includes(CRLF)) {
      const index = this.#buffer.indexOf(CRLF);
      const line = this.#buffer.slice(0, index);
      this.#buffer = this.#buffer.slice(index + CRLF.length);
      if (line.length > 8192) {
        this.#record("rejected", { command: "OVERSIZED" });
        replies.push(`* BYE [ALERT] command exceeds fixture limit${CRLF}`);
      } else {
        const routed = this.#route(line);
        if (routed.some((reply) => /^\S+ (?:BAD|NO) /m.test(reply)))
          this.#record("rejected", { command: commandVerb(line) });
        replies.push(...routed);
      }
    }
    if (this.#buffer.length > 8192) {
      this.#buffer = "";
      this.#record("rejected", { command: "OVERSIZED" });
      replies.push(`* BYE [ALERT] command exceeds fixture limit${CRLF}`);
    }
    return replies;
  }
  #record(kind, values = {}) {
    this.#events.push(Object.freeze({ kind, ...values }));
  }
  #route(line) {
    this.#record("command", { command: commandVerb(line) });
    const parsed = /^(\S+)\s+(.+)$/.exec(line);
    if (!parsed) return [`* BAD malformed command${CRLF}`];
    const [, tag, command] = parsed;
    if (/^CAPABILITY$/i.test(command))
      return [
        `* CAPABILITY IMAP4rev1${CRLF}`,
        tagged(tag, "OK", "CAPABILITY completed"),
      ];
    if (/^LOGOUT$/i.test(command))
      return [
        `* BYE logging out${CRLF}`,
        tagged(tag, "OK", "LOGOUT completed"),
      ];
    const login = /^LOGIN\s+"([^"]+)"\s+"([^"]+)"$/i.exec(command);
    if (login) {
      if (login[1] === FIXTURE.account && login[2] === FIXTURE.password) {
        this.#authenticated = true;
        this.#record("login");
        return [tagged(tag, "OK", "LOGIN completed")];
      }
      this.#record("login-rejected");
      return [
        tagged(tag, "NO", "[AUTHENTICATIONFAILED] invalid fixture credentials"),
      ];
    }
    if (!this.#authenticated)
      return [tagged(tag, "NO", "[AUTHENTICATIONFAILED] authenticate first")];
    if (/^LIST\s+""\s+"\*"$/i.test(command))
      return [
        `* LIST (\\HasNoChildren) "/" "INBOX"${CRLF}`,
        tagged(tag, "OK", "LIST completed"),
      ];
    if (/^SELECT\s+(?:INBOX|"INBOX")$/i.test(command)) {
      this.#selected = true;
      this.#record("select");
      return [
        `* ${FIXTURE.messageCount} EXISTS${CRLF}`,
        `* OK [UIDVALIDITY ${FIXTURE.uidValidity}] stable fictional mailbox${CRLF}`,
        `* OK [UIDNEXT ${FIXTURE.messageCount + 1}] next UID${CRLF}`,
        `* FLAGS (\\Seen)${CRLF}`,
        tagged(tag, "OK", "[READ-ONLY] SELECT completed"),
      ];
    }
    if (!this.#selected)
      return [tagged(tag, "NO", "[TRYCREATE] select INBOX first")];
    if (/^FETCH\s+/i.test(command)) return this.#snapshot(tag, command);
    if (/^UID\s+FETCH\s+/i.test(command)) return this.#uidFetch(tag, command);
    return [tagged(tag, "BAD", "[UNSUPPORTED] read-only fixture command")];
  }
  #snapshot(tag, command) {
    const match = /^FETCH\s+(\d+):(\d+)\s+\(UID FLAGS\)$/i.exec(command);
    if (!match)
      return [
        tagged(
          tag,
          "BAD",
          "[UNSUPPORTED] only bounded UID/FLAGS snapshots are available",
        ),
      ];
    const start = Number(match[1]);
    const end = Number(match[2]);
    if (
      end - start + 1 !== FIXTURE.pageSize ||
      start !== this.#nextPageStart ||
      end !== start + FIXTURE.pageSize - 1
    )
      return [
        tagged(
          tag,
          "BAD",
          "[ORDER] expected descending 500-message snapshot page",
        ),
      ];
    this.#nextPageStart = start === 1 ? 0 : start - FIXTURE.pageSize;
    this.#record("snapshot", { start, end });
    return Array.from({ length: FIXTURE.pageSize }, (_, offset) => {
      const uid = start + offset;
      return `* ${uid} FETCH (UID ${uid} FLAGS (\\Seen))${CRLF}`;
    }).concat(tagged(tag, "OK", "FETCH completed"));
  }
  #uidFetch(tag, command) {
    const match = /^UID\s+FETCH\s+([0-9,]+)\s+\((.+)\)$/i.exec(command);
    if (!match)
      return [tagged(tag, "BAD", "[UNSUPPORTED] malformed UID FETCH")];
    const uids = match[1].split(",").map(Number);
    const fields = match[2];
    if (
      !uids.length ||
      uids.length > FIXTURE.pageSize ||
      uids.some(
        (uid) =>
          !Number.isInteger(uid) || uid < 1 || uid > FIXTURE.messageCount,
      )
    )
      return [
        tagged(tag, "NO", "[NONEXISTENT] UID set is outside the fixture"),
      ];
    const header =
      /^FLAGS INTERNALDATE(?: RFC822\.SIZE)?(?: BODYSTRUCTURE)? BODY\.PEEK\[HEADER\.FIELDS \((.+)\)\]$/i.exec(
        fields,
      );
    if (header) {
      this.#record("metadata", { uids });
      return uids
        .map((uid) => {
          const raw = fixtureRaw(uid);
          const known = retained[uid];
          const structure =
            known?.structure ??
            `("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" ${Buffer.byteLength(raw.split(`${CRLF}${CRLF}`)[1])} 1)`;
          const date = known?.date ?? "30-Jul-2026 10:00:00 +0000";
          const response = `UID ${uid} FLAGS (\\Seen) INTERNALDATE "${date}"${/RFC822\.SIZE/i.test(fields) ? ` RFC822.SIZE ${Buffer.byteLength(raw)}` : ""}${/BODYSTRUCTURE/i.test(fields) ? ` BODYSTRUCTURE ${structure}` : ""} BODY[HEADER.FIELDS (${header[1]})]`;
          return literal(uid, response, headerFields(raw, header[1]));
        })
        .concat(tagged(tag, "OK", "UID FETCH completed"));
    }
    const section = /^BODY\.PEEK\[([0-9]+(?:\.MIME)?)\]$/i.exec(fields);
    if (section && uids.length === 1) {
      const uid = uids[0];
      const value =
        uid === 2 && section[1] === "1"
          ? "Fictional retained multipart text.\r\n"
          : uid === 2 && section[1] === "1.MIME"
            ? "Content-Type: text/plain; charset=UTF-8\r\n\r\n"
            : uid === 3 && section[1] === "1"
              ? "<p>Fictional retained related body.</p>\r\n"
              : uid === 3 && section[1] === "1.MIME"
                ? "Content-Type: text/html; charset=UTF-8\r\n\r\n"
                : null;
      if (value !== null) {
        this.#record("section", { uid, section: section[1] });
        return [
          literal(uid, `UID ${uid} BODY[${section[1]}]`, value),
          tagged(tag, "OK", "UID FETCH completed"),
        ];
      }
    }
    if (/^BODY\.PEEK\[\](?:<0\.8192>)?$/i.test(fields)) {
      const partial = /<0\.8192>$/.test(fields);
      this.#record(partial ? "snippet" : "full-body-peek", { uids });
      return uids
        .map((uid) =>
          literal(
            uid,
            `UID ${uid} BODY[]${partial ? "<0>" : ""}`,
            partial
              ? Buffer.from(fixtureRaw(uid)).subarray(0, 8192).toString("utf8")
              : fixtureRaw(uid),
          ),
        )
        .concat(tagged(tag, "OK", "UID FETCH completed"));
    }
    return [
      tagged(
        tag,
        "BAD",
        "[UNSUPPORTED] only read-neutral metadata and Reader fetches are available",
      ),
    ];
  }
}

export function commandVerb(line) {
  const tokens = line.trim().split(/\s+/);
  const first = tokens[1]?.toUpperCase();
  // No arguments or tags are retained. LOGIN credentials never enter evidence.
  const verb =
    first === "UID" ? `UID ${tokens[2]?.toUpperCase() ?? "MALFORMED"}` : first;
  return /^[A-Z]+(?: [A-Z]+)?$/.test(verb ?? "") ? verb : "MALFORMED";
}

export function createLargeMailboxTlsServer({
  key,
  cert,
  onSession = () => {},
}) {
  if (!key || !cert)
    throw new TypeError("fixture requires TLS key and certificate PEM values");
  const sockets = new Set();
  const server = createServer(
    { key, cert, minVersion: "TLSv1.2" },
    (socket) => {
      sockets.add(socket);
      const session = new LargeMailboxSession();
      const tlsProtocol = socket.getProtocol();
      socket.write(session.greeting());
      socket.on("data", (data) => {
        for (const response of session.receive(data.toString("utf8")))
          socket.write(response);
      });
      socket.on("error", () => {});
      socket.once("close", () => {
        sockets.delete(socket);
        onSession({ tlsProtocol, events: session.transcript() });
      });
    },
  );
  // Also own sockets which have not completed the TLS handshake.
  server.on("connection", (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
  });
  server.closeOwnedConnections = () => {
    for (const socket of sockets) socket.destroy();
  };
  return server;
}

export async function startLargeMailboxFixture({
  keyPath,
  certPath,
  port = 0,
  onSession,
} = {}) {
  if (!keyPath || !certPath)
    throw new TypeError("fixture requires --key and --cert paths");
  const server = createLargeMailboxTlsServer({
    key: await readFile(keyPath, "utf8"),
    cert: await readFile(certPath, "utf8"),
    onSession,
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen({ host: "127.0.0.1", port }, resolve);
  });
  const address = server.address();
  let closing;
  return Object.freeze({
    port: address.port,
    close: () =>
      (closing ??= new Promise((resolve, reject) => {
        const timer = setTimeout(
          () => reject(new Error("TLS fixture shutdown deadline exceeded")),
          2000,
        );
        server.close((error) => {
          clearTimeout(timer);
          error ? reject(error) : resolve();
        });
        server.closeOwnedConnections();
      })),
  });
}

function parseArgs(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    const value = argv[index + 1];
    if (
      !/^(--key|--cert|--ready|--audit)$/.test(name) ||
      !value ||
      values.has(name)
    )
      throw new TypeError(
        "expected --key PATH --cert PATH --ready PATH --audit PATH",
      );
    values.set(name, value);
  }
  if (values.size !== 4)
    throw new TypeError(
      "expected --key PATH --cert PATH --ready PATH --audit PATH",
    );
  return Object.fromEntries(values);
}

if (process.argv[1] && new URL(import.meta.url).pathname === process.argv[1]) {
  const args = parseArgs(process.argv.slice(2));
  const sessions = [];
  const fixture = await startLargeMailboxFixture({
    keyPath: args["--key"],
    certPath: args["--cert"],
    onSession: (session) => sessions.push(session),
  });
  await writeFile(
    args["--ready"],
    `${JSON.stringify({ host: "127.0.0.1", port: fixture.port, account: FIXTURE.account })}\n`,
    { mode: 0o600, flag: "wx" },
  );
  let finishing = false;
  const shutdown = async () => {
    if (finishing) return;
    finishing = true;
    try {
      await fixture.close();
      await writeFile(
        args["--audit"],
        `${JSON.stringify({ schemaVersion: 2, fixture: "fictional-30000", sessions })}\n`,
        { mode: 0o600, flag: "wx" },
      );
    } catch {
      process.exitCode = 1;
    }
  };
  for (const signal of ["SIGINT", "SIGTERM"]) process.once(signal, shutdown);
}
