#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { createServer } from "node:tls";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    cert: { type: "string" },
    key: { type: "string" },
    "port-file": { type: "string" },
  },
});
if (!values.cert || !values.key || !values["port-file"]) {
  throw new Error("Usage: attachment-command-fixture-server.mjs --cert FILE --key FILE --port-file FILE");
}

const unsafeBytes = Buffer.from("fictional attachment bytes\0\xff", "binary");
const safeBytes = Buffer.from("fictional safe attachment bytes\n", "utf8");
const unsafeEncoded = unsafeBytes.toString("base64");
const safeEncoded = safeBytes.toString("base64");
const message = [
  "From: Fictional Provider <provider@localhost>",
  "To: Fictional Reader <fictional-reader@localhost>",
  "Subject: Fictional attachment command artifact",
  "Message-ID: <fictional-attachment-command-artifact@localhost>",
  "MIME-Version: 1.0",
  'Content-Type: multipart/mixed; boundary="fixture-boundary"',
  "",
  "--fixture-boundary",
  'Content-Type: text/plain; charset="utf-8"',
  "",
  "Fictional provider MIME reaches the Reader through the real command path.",
  "--fixture-boundary",
  'Content-Type: application/octet-stream; name="../../quarterly-report.pdf.command"',
  'Content-Disposition: attachment; filename="../../quarterly-report.pdf.command"',
  "Content-Transfer-Encoding: base64",
  "",
  unsafeEncoded,
  "--fixture-boundary",
  'Content-Type: text/plain; name="meeting-notes.txt"',
  'Content-Disposition: attachment; filename="meeting-notes.txt"',
  "Content-Transfer-Encoding: base64",
  "",
  safeEncoded,
  "--fixture-boundary--",
  "",
].join("\r\n");
const messageBytes = Buffer.from(message, "utf8");
const topHeaders = Buffer.from(
  message.slice(0, message.indexOf("\r\n\r\n") + 4),
  "utf8",
);
const unsafeHeaders = Buffer.from(
  'Content-Type: application/octet-stream; name="../../quarterly-report.pdf.command"\r\n' +
    'Content-Disposition: attachment; filename="../../quarterly-report.pdf.command"\r\n' +
    "Content-Transfer-Encoding: base64\r\n\r\n",
  "utf8",
);
const partHeaders = new Map([
  [
    "1",
    Buffer.from('Content-Type: text/plain; charset="utf-8"\r\n\r\n', "utf8"),
  ],
  ["2", unsafeHeaders],
  [
    "3",
    Buffer.from(
      'Content-Type: text/plain; name="meeting-notes.txt"\r\n' +
        'Content-Disposition: attachment; filename="meeting-notes.txt"\r\n' +
        "Content-Transfer-Encoding: base64\r\n\r\n",
      "utf8",
    ),
  ],
]);
const bodyStructure = `(("TEXT" "PLAIN" ("CHARSET" "utf-8") NIL NIL "7BIT" 72 1 NIL NIL NIL) ("APPLICATION" "OCTET-STREAM" ("NAME" "../../quarterly-report.pdf.command") NIL NIL "BASE64" ${unsafeEncoded.length} NIL ("ATTACHMENT" ("FILENAME" "../../quarterly-report.pdf.command")) NIL NIL) ("TEXT" "PLAIN" ("NAME" "meeting-notes.txt") NIL NIL "BASE64" ${safeEncoded.length} NIL ("ATTACHMENT" ("FILENAME" "meeting-notes.txt")) NIL NIL) "MIXED" ("BOUNDARY" "fixture-boundary") NIL NIL)`;

let connectionCount = 0;
const sockets = new Set();
function tagged(tag, line) {
  return `${tag} OK ${line}\r\n`;
}
function literal(prefix, bytes, suffix) {
  return Buffer.concat([
    Buffer.from(`${prefix} {${bytes.length}}\r\n`, "utf8"),
    bytes,
    Buffer.from(suffix, "utf8"),
  ]);
}

const server = createServer(
  {
    cert: await readFile(values.cert),
    key: await readFile(values.key),
    minVersion: "TLSv1.2",
  },
  (socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    const connection = ++connectionCount;
    let buffered = "";
    socket.write("* OK fictional attachment artifact IMAP ready\r\n");
    socket.on("data", (chunk) => {
      buffered += chunk.toString("utf8");
      for (;;) {
        const newline = buffered.indexOf("\r\n");
        if (newline < 0) return;
        const line = buffered.slice(0, newline);
        buffered = buffered.slice(newline + 2);
        const firstSpace = line.indexOf(" ");
        if (firstSpace < 0) {
          socket.destroy(new Error(`malformed IMAP command: ${line}`));
          return;
        }
        const tag = line.slice(0, firstSpace);
        const command = line.slice(firstSpace + 1);
        if (/^CAPABILITY$/i.test(command)) {
          socket.write(`* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\n${tagged(tag, "capability")}`);
        } else if (/^LOGIN /i.test(command)) {
          socket.write(tagged(tag, "authenticated"));
        } else if (/^SELECT "INBOX"$/i.test(command)) {
          socket.write(`* 1 EXISTS\r\n* OK [UIDVALIDITY 77] selected\r\n${tagged(tag, "selected")}`);
        } else if (/^UID FETCH 42 \(.*BODYSTRUCTURE/i.test(command)) {
          socket.write(
            literal(
              `* 42 FETCH (UID 42 FLAGS () INTERNALDATE "17-Oct-2026 12:00:00 +0000" BODYSTRUCTURE ${bodyStructure} BODY[HEADER]`,
              topHeaders,
              `\r\n)\r\n${tagged(tag, "metadata")}`,
            ),
          );
        } else if (/^UID FETCH 42 \(BODY\.PEEK\[[123]\.MIME\]\)$/i.test(command)) {
          const section = command.match(/BODY\.PEEK\[([123])\.MIME\]/i)?.[1];
          const headers = section ? partHeaders.get(section) : undefined;
          if (!section || !headers) {
            socket.destroy(new Error(`unsupported MIME header section: ${command}`));
            return;
          }
          socket.write(
            literal(
              `* 42 FETCH (UID 42 BODY[${section}.MIME]`,
              headers,
              `\r\n)\r\n${tagged(tag, "attachment headers")}`,
            ),
          );
        } else if (/^UID FETCH 42 \(BODY\.PEEK\[2\]\)$/i.test(command)) {
          socket.write(
            literal(
              "* 42 FETCH (UID 42 BODY[2]",
              Buffer.from(unsafeEncoded, "ascii"),
              `\r\n)\r\n${tagged(tag, "attachment bytes")}`,
            ),
          );
        } else if (/^UID FETCH 42 \(.*BODY\.PEEK\[\]/i.test(command)) {
          socket.write(
            literal(
              "* 42 FETCH (UID 42 FLAGS () INTERNALDATE \"17-Oct-2026 12:00:00 +0000\" BODY[]",
              messageBytes,
              `\r\n)\r\n${tagged(tag, "full message")}`,
            ),
          );
        } else if (/^LOGOUT$/i.test(command)) {
          socket.end(`* BYE fictional artifact complete\r\n${tagged(tag, "logout")}`);
        } else {
          socket.destroy(new Error(`unexpected artifact IMAP command on connection ${connection}: ${command}`));
          return;
        }
      }
    });
  },
);
server.on("error", (error) => {
  process.stderr.write(`${error.stack || error}\n`);
  process.exitCode = 1;
});
server.listen({ host: "127.0.0.1", port: 0 }, async () => {
  const address = server.address();
  await (await import("node:fs/promises")).writeFile(values["port-file"], `${address.port}\n`, { mode: 0o600 });
});
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const socket of sockets) socket.destroy();
    server.close(() => process.exit(0));
  });
}
