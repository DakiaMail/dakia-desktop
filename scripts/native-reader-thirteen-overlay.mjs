#!/usr/bin/env node
import { createHash } from "node:crypto";
import {
  appendFileSync,
  chmodSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { createServer, connect as tlsConnect } from "node:tls";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

export const FIXTURES = Object.freeze([
  [
    4,
    "competing-nested-alternatives",
    "crates/dakia-core/testdata/mime/competing-nested-alternatives.eml",
    "9ff77dfb2ed340808df5d023a7154b7f58b8e2d6c2220c1208337c03dfce8596",
  ],
  [
    5,
    "attached-message-rfc822",
    "crates/dakia-core/testdata/mime/attached-message-rfc822.eml",
    "a6d8cf9ff97a4612f3cacfb420c1efe00c9441721779d2fe5c09f6613c216304",
  ],
  [
    6,
    "multipart-attachment-container",
    "crates/dakia-core/testdata/mime/multipart-attachment-container.eml",
    "01246fb8935fabc95c63339eb328e5762ceff9df7b1d3ff50f93986bec6d5b53",
  ],
  [
    7,
    "empty-alternative-leaf",
    "crates/dakia-core/testdata/mime/empty-alternative-leaf.eml",
    "51e1fababdf7db6f771d2115f569c737adec938fb353c963d4e2729c9ca74607",
  ],
  [
    8,
    "truncated-multipart-lf",
    "crates/dakia-core/testdata/mime/truncated-multipart-lf.eml",
    "e9644bc93393af94322680b3751032350daf5bcdbb5d479ee574ebb0c9006094",
  ],
  [
    9,
    "charset-matrix",
    "crates/dakia-core/testdata/mime/charset-matrix.eml",
    "3a254be99ee64bd5263d2feb90c96907a5f647ccc1b215a68dc50aaa6ae29414",
  ],
  [
    10,
    "format-flowed-delsp",
    "crates/dakia-core/testdata/mime/format-flowed-delsp.eml",
    "4a82e24f2495f9fdf226c12136e02ed73aec6b90d7bb4312bed60874d95701a8",
  ],
  [
    11,
    "international-reader-semantics",
    "crates/dakia-core/testdata/mime/international-reader-semantics.eml",
    "3fbdb8273819c0a20a585c36a45d15bd0bc89f7708a2293dfe0e74181c14629b",
  ],
  [
    12,
    "ambiguous-inline-identities",
    "crates/dakia-core/testdata/mime/ambiguous-inline-identities.eml",
    "061751183e39b29b0b241ec7e9ae0568e90cb1b087502edcffb97500492e3364",
  ],
  [
    13,
    "provider-signature-inline",
    "crates/dakia-core/tests/fixtures/provider-signature-inline.eml",
    "2bf3c6cf6c1aefcf5103697405a6592df8760786e45e7b0d2ff3c9692055e4ce",
  ],
  [
    14,
    "filename-parameters-and-encodings",
    "crates/dakia-core/testdata/mime/filename-parameters-and-encodings.eml",
    "77095fc2d7516b0ab1c62782778579112749ea2dc802fea5e4e7fb591dae5b5e",
  ],
  [
    15,
    "linkedin-inline-content-id",
    "crates/dakia-core/testdata/mime/linkedin-inline-content-id.eml",
    "d302e38e64c864df1a91fc72d60c54945ff81cf93a8158f88d167bdc927c4437",
  ],
  [
    16,
    "standards-unknown-transfer-encoding",
    "crates/dakia-core/testdata/mime/standards-unknown-transfer-encoding.eml",
    "087ad977f7b58e4dd0a306814bc2f03ef36866ffe78c27ad11c60f82ba6386ab",
  ],
]);

const SELECTED_HEADERS = new Set([
  "date",
  "from",
  "to",
  "cc",
  "bcc",
  "reply-to",
  "subject",
  "message-id",
  "in-reply-to",
  "references",
  "content-type",
  "content-transfer-encoding",
  "content-id",
  "content-location",
  "content-disposition",
  "list-id",
  "list-unsubscribe",
  "list-unsubscribe-post",
  "precedence",
  "auto-submitted",
]);
const CONTRADICTORY_BODYSTRUCTURE =
  '(("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 10 1)("TEXT" "HTML" ("CHARSET" "us-ascii") NIL NIL "7BIT" 10 1) "ALTERNATIVE" ("BOUNDARY" "native-reader-child"))("APPLICATION" "OCTET-STREAM" ("NAME" "ignored.bin") NIL NIL "BASE64" 8 NIL ("ATTACHMENT" ("FILENAME" "ignored.bin")) NIL) "MIXED" ("BOUNDARY" "native-reader-parent")';
const REPEATED_PARENT_MIME = Buffer.from(
  'Content-Type: multipart/alternative;\r\n boundary="native-reader-child"',
);

export const sha256 = (bytes) =>
  createHash("sha256").update(bytes).digest("hex");

function splitHeaderLines(raw) {
  const text = raw.toString("latin1");
  const separator = /\r\n\r\n|\n\n|\r\r/.exec(text);
  if (!separator) throw new Error("fixture has no header/body separator");
  return text.slice(0, separator.index).split(/\r\n|\n|\r/);
}

export function selectedHeaders(raw) {
  const fields = [];
  let current = [];
  for (const line of splitHeaderLines(raw)) {
    if (/^[ \t]/.test(line)) {
      if (current.length === 0)
        throw new Error("fixture starts with a folded header continuation");
      current.push(line);
      continue;
    }
    if (current.length) fields.push(current);
    current = [line];
  }
  if (current.length) fields.push(current);
  const selected = fields.filter((lines) => {
    const colon = lines[0].indexOf(":");
    if (colon <= 0)
      throw new Error("fixture contains a malformed top-level header");
    return SELECTED_HEADERS.has(lines[0].slice(0, colon).toLowerCase());
  });
  const names = selected.map((lines) =>
    lines[0].slice(0, lines[0].indexOf(":")).toLowerCase(),
  );
  if (!names.includes("subject"))
    throw new Error("fixture has no Subject header");
  return Buffer.from(`${selected.flat().join("\r\n")}\r\n\r\n`, "latin1");
}

export function loadFixtures(sourceRoot) {
  const fixtures = new Map();
  for (const [uid, id, relativePath, expectedSha256] of FIXTURES) {
    const path = join(sourceRoot, relativePath);
    const raw = readFileSync(path);
    if (sha256(raw) !== expectedSha256)
      throw new Error(`fixture digest changed: ${id}`);
    const headers = selectedHeaders(raw);
    fixtures.set(uid, {
      uid,
      id,
      relativePath,
      path,
      raw,
      headers,
      sha256: expectedSha256,
    });
  }
  return fixtures;
}

function decodeImapAtom(value) {
  if (value.startsWith('"') && value.endsWith('"'))
    return value.slice(1, -1).replace(/\\([\\"])/g, "$1");
  return value;
}

export function createSessionRoute() {
  return { account: null, mailbox: null, pending: new Map() };
}

export function trackClientRoute(line, route) {
  const login = /^(\S+)\s+LOGIN\s+("(?:\\.|[^"])*"|\S+)\s+/i.exec(line);
  if (login) {
    route.pending.set(login[1], {
      action: "login",
      account: decodeImapAtom(login[2]).toLowerCase(),
    });
    return;
  }
  const select =
    /^(\S+)\s+(SELECT|EXAMINE)\s+("(?:\\.|[^"])*"|[^\s()]+)(?:\s+\([^\r\n]*\))?\s*\r?\n$/i.exec(
      line,
    );
  if (select) {
    route.pending.set(select[1], {
      action: "select",
      mailbox: decodeImapAtom(select[3]),
    });
    return;
  }
  const deselect = /^(\S+)\s+(CLOSE|UNSELECT)\s*\r?\n$/i.exec(line);
  if (deselect) {
    route.pending.set(deselect[1], { action: "deselect" });
    return;
  }
  const logout = /^(\S+)\s+LOGOUT\b/i.exec(line);
  if (logout) route.pending.set(logout[1], { action: "logout" });
}

export function trackBackendRoute(line, route) {
  const status = /^(\S+)\s+(OK|NO|BAD)\b/i.exec(line);
  if (!status) return;
  const pending = route.pending.get(status[1]);
  if (!pending) return;
  route.pending.delete(status[1]);
  if (status[2].toUpperCase() !== "OK") return;
  if (pending.action === "login") {
    route.account = pending.account;
    route.mailbox = null;
  } else if (pending.action === "select") {
    route.mailbox = pending.mailbox;
  } else if (pending.action === "deselect") {
    route.mailbox = null;
  } else if (pending.action === "logout") {
    route.account = null;
    route.mailbox = null;
  }
}

export function isFixtureRoute(route) {
  return (
    route?.account === "fictional-reader@localhost" &&
    route?.mailbox?.toUpperCase() === "INBOX"
  );
}

export function classifyCommand(line, fixtures, route) {
  const match = /^(\S+)\s+UID FETCH (\d+) \((.*)\)\r?\n$/i.exec(line);
  if (!match)
    return { action: "forward", summary: summarizeForwardedCommand(line) };
  const [, tag, uidText, fields] = match;
  const uid = Number(uidText);
  if (!fixtures.has(uid) || !isFixtureRoute(route))
    return { action: "forward", summary: summarizeForwardedCommand(line) };
  if (/^FLAGS INTERNALDATE BODY\.PEEK\[\]$/i.test(fields))
    return { action: "raw", tag, uid };
  const metadata =
    /^FLAGS INTERNALDATE BODYSTRUCTURE BODY\.PEEK\[(HEADER\.FIELDS \(.*\))\]$/i.exec(
      fields,
    );
  if (metadata)
    return { action: "metadata", tag, uid, headerSection: metadata[1] };
  const mime = /^BODY\.PEEK\[((?:\d+\.)*\d+)\.MIME\]$/i.exec(fields);
  if (mime) return { action: "mime", tag, uid, section: mime[1] };
  return { action: "forward", summary: summarizeForwardedCommand(line) };
}

export function summarizeForwardedCommand(line) {
  const match = /^(\S+)\s+([A-Z]+)/i.exec(line.trimEnd());
  if (!match)
    return { kind: "unparsed", bytes: Buffer.byteLength(line, "binary") };
  const [, tag, verb] = match;
  return { kind: "command", tag, verb: verb.toUpperCase() };
}

export function syntheticResponse(event, fixtures) {
  const fixture = fixtures.get(event.uid);
  if (!fixture)
    throw new Error("synthetic response targets an unknown fixture UID");
  if (event.action === "raw") {
    return Buffer.concat([
      Buffer.from(
        `* ${event.uid} FETCH (UID ${event.uid} FLAGS () INTERNALDATE "22-Oct-2026 10:00:00 +0000" BODY[]<0> {${fixture.raw.length}}\r\n`,
      ),
      fixture.raw,
      Buffer.from(`\r\n)\r\n${event.tag} OK exact registered fixture raw\r\n`),
    ]);
  }
  if (event.action === "metadata") {
    return Buffer.concat([
      Buffer.from(
        `* ${event.uid} FETCH (UID ${event.uid} FLAGS () INTERNALDATE "22-Oct-2026 10:00:00 +0000" BODYSTRUCTURE ${CONTRADICTORY_BODYSTRUCTURE} BODY[${event.headerSection}] {${fixture.headers.length}}\r\n`,
      ),
      fixture.headers,
      Buffer.from(
        `)\r\n${event.tag} OK exact headers with deliberately unsupported selective topology\r\n`,
      ),
    ]);
  }
  if (event.action === "mime") {
    return Buffer.concat([
      Buffer.from(
        `* ${event.uid} FETCH (UID ${event.uid} BODY[${event.section}.MIME] {${REPEATED_PARENT_MIME.length}}\r\n`,
      ),
      REPEATED_PARENT_MIME,
      Buffer.from(
        `\r\n)\r\n${event.tag} OK repeated parent MIME forces production full-raw fallback\r\n`,
      ),
    ]);
  }
  throw new Error(`unsupported synthetic action: ${event.action}`);
}

function parseArgs(argv) {
  const values = new Map();
  for (let index = 0; index < argv.length; index += 2)
    values.set(argv[index], argv[index + 1]);
  for (const key of [
    "--root",
    "--source-root",
    "--backend-port",
    "--certificate",
    "--key",
    "--ca",
  ]) {
    if (!values.get(key)) throw new Error(`missing ${key}`);
  }
  return Object.fromEntries(
    [...values].map(([key, value]) => [
      key.slice(2).replaceAll("-", "_"),
      value,
    ]),
  );
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const root = resolve(args.root);
  if (
    readFileSync(join(root, ".dakia-native-mail-safety-root"), "utf8") !==
    "dakia-native-mail-safety-fixture-v1\n"
  )
    throw new Error("owned root sentinel is missing");
  const fixtures = loadFixtures(resolve(args.source_root));
  const transcript = join(root, "native-reader-thirteen-imap-transcript.jsonl");
  writeFileSync(transcript, "", { flag: "wx", mode: 0o600 });
  const log = (value) =>
    appendFileSync(
      transcript,
      `${JSON.stringify({ at: new Date().toISOString(), ...value })}\n`,
    );
  const sockets = new Set();
  const server = createServer(
    {
      cert: readFileSync(args.certificate),
      key: readFileSync(args.key),
      minVersion: "TLSv1.2",
    },
    (client) => {
      sockets.add(client);
      const backend = tlsConnect({
        host: "127.0.0.1",
        port: Number(args.backend_port),
        servername: "localhost",
        ca: readFileSync(args.ca),
        rejectUnauthorized: true,
        minVersion: "TLSv1.2",
      });
      sockets.add(backend);
      const route = createSessionRoute();
      let pending = "";
      let backendPending = "";
      backend.on("data", (chunk) => {
        backendPending += chunk.toString("binary");
        while (backendPending.includes("\n")) {
          const end = backendPending.indexOf("\n") + 1;
          const line = backendPending.slice(0, end);
          backendPending = backendPending.slice(end);
          trackBackendRoute(line, route);
        }
        client.write(chunk);
      });
      client.on("data", (chunk) => {
        pending += chunk.toString("binary");
        while (pending.includes("\n")) {
          const end = pending.indexOf("\n") + 1;
          const line = pending.slice(0, end);
          pending = pending.slice(end);
          trackClientRoute(line, route);
          const event = classifyCommand(line, fixtures, route);
          if (event.action === "forward") {
            backend.write(Buffer.from(line, "binary"));
            log({ direction: "client", action: "forward", ...event.summary });
          } else {
            client.write(syntheticResponse(event, fixtures));
            const fixture = fixtures.get(event.uid);
            log({
              direction: "client",
              action: event.action,
              uid: event.uid,
              fixtureId: fixture.id,
              fixtureSha256: fixture.sha256,
              fixtureAccount: true,
              mailbox: "INBOX",
            });
          }
        }
      });
      const close = () => {
        client.destroy();
        backend.destroy();
        sockets.delete(client);
        sockets.delete(backend);
      };
      client.on("error", close);
      backend.on("error", close);
      client.on("close", close);
      backend.on("close", close);
    },
  );
  await new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolvePromise);
  });
  const metadata = {
    schemaVersion: 1,
    mode: "native-reader-thirteen-inner-tls",
    pid: process.pid,
    port: server.address().port,
    backendPort: Number(args.backend_port),
    transcript,
    fixtures: [...fixtures.values()].map(
      ({ uid, id, relativePath, sha256: digest }) => ({
        uid,
        id,
        relativePath,
        sha256: digest,
      }),
    ),
  };
  const metadataPath = join(root, "native-reader-thirteen-overlay.json");
  writeFileSync(metadataPath, `${JSON.stringify(metadata, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
  chmodSync(metadataPath, 0o600);
  for (const signal of ["SIGINT", "SIGTERM"])
    process.once(signal, () => {
      for (const socket of sockets) socket.destroy();
      server.close(() => process.exit(0));
    });
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  main().catch((error) => {
    console.error(error.stack || error);
    process.exit(1);
  });
}
