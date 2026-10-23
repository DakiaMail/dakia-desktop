#!/usr/bin/env node

// A deliberately small TLS IMAP provider for the production Tauri mail-action
// command test.  It records every received command and rejects anything that
// is not part of the declared fictional transcript.

import { appendFile, readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:tls";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    cert: { type: "string" },
    key: { type: "string" },
    "port-file": { type: "string" },
    transcript: { type: "string" },
  },
});
if (!values.cert || !values.key || !values["port-file"] || !values.transcript) {
  throw new Error(
    "Usage: mail-command-tls-fixture-server.mjs --cert FILE --key FILE --port-file FILE --transcript FILE",
  );
}

let transcriptWrites = Promise.resolve();
function record(entry) {
  transcriptWrites = transcriptWrites.then(() =>
    appendFile(values.transcript, `${JSON.stringify(entry)}\n`, {
      mode: 0o600,
    }),
  );
  return transcriptWrites;
}

function tagged(socket, tag, status, detail) {
  socket.write(`${tag} ${status} ${detail}\r\n`);
}

const sockets = new Set();
let nextSession = 0;
const server = createServer(
  {
    cert: await readFile(values.cert),
    key: await readFile(values.key),
    minVersion: "TLSv1.2",
  },
  (socket) => {
    let buffered = "";
    let stage = "capability";
    const session = ++nextSession;
    let serial = record({ event: "opened", session });
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    serial = serial.then(() =>
      socket.write("* OK fictional mail-action IMAP ready\r\n"),
    );
    socket.on("data", (chunk) => {
      serial = serial.then(async () => {
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
          await record({ event: "command", session, command, stage });

          if (stage === "capability" && /^CAPABILITY$/i.test(command)) {
            socket.write(
              `* CAPABILITY IMAP4rev1 AUTH=PLAIN MOVE UIDPLUS\r\n${tag} OK capability\r\n`,
            );
            stage = "login";
          } else if (
            stage === "login" &&
            /^LOGIN "fictional-reader@localhost" "synthetic-only-password"$/i.test(
              command,
            )
          ) {
            tagged(socket, tag, "OK", "authenticated");
            stage = "select-or-delete-capability";
          } else if (
            stage === "select-or-delete-capability" &&
            /^CAPABILITY$/i.test(command)
          ) {
            socket.write(
              `* CAPABILITY IMAP4rev1 AUTH=PLAIN MOVE UIDPLUS\r\n${tag} OK capability\r\n`,
            );
            stage = "delete-select";
          } else if (
            stage === "select-or-delete-capability" &&
            /^LIST "" "\*"$/i.test(command)
          ) {
            // Production MOVE discovers the mailbox family before SELECT so
            // it can reject an absent or ambiguous destination without first
            // changing the selected mailbox. Keep this authoritative response
            // explicit rather than allowing an unbounded LIST transcript.
            socket.write(
              `* LIST (\\HasNoChildren) "/" "INBOX"\r\n* LIST (\\HasNoChildren \\Archive) "/" "Archive"\r\n* LIST (\\HasNoChildren \\Junk) "/" "Spam"\r\n* LIST (\\HasNoChildren \\Trash) "/" "Trash"\r\n${tag} OK listed\r\n`,
            );
            stage = "move-select";
          } else if (
            stage === "delete-select" &&
            /^LIST "" "\*"$/i.test(command)
          ) {
            // Permanent deletion also proves the source mailbox is selectable
            // before issuing UID STORE and UID EXPUNGE.
            socket.write(
              `* LIST (\\HasNoChildren) "/" "INBOX"\r\n* LIST (\\HasNoChildren \\Archive) "/" "Archive"\r\n* LIST (\\HasNoChildren \\Junk) "/" "Spam"\r\n* LIST (\\HasNoChildren \\Trash) "/" "Trash"\r\n${tag} OK listed\r\n`,
            );
            stage = "delete-select-after-list";
          } else if (
            (stage === "select-or-delete-capability" ||
              stage === "delete-select-after-list" ||
              stage === "move-select") &&
            /^SELECT "(?:INBOX|Spam)"$/i.test(command)
          ) {
            socket.write(
              `* 64 EXISTS\r\n* OK [UIDVALIDITY 77] selected\r\n${tag} OK selected\r\n`,
            );
            stage =
              stage === "delete-select-after-list"
                ? "delete-store"
                : stage === "move-select"
                  ? "operation-move"
                  : "operation";
          } else if (
            stage === "operation" &&
            /^UID STORE (11|12) [+-]FLAGS\.SILENT \(\\(?:Seen|Flagged)\)$/i.test(
              command,
            )
          ) {
            tagged(socket, tag, "OK", "stored");
            stage = "logout";
          } else if (
            stage === "operation" &&
            /^UID FETCH 12 \(FLAGS INTERNALDATE BODYSTRUCTURE BODY\.PEEK\[HEADER\.FIELDS \(DATE FROM TO CC BCC REPLY-TO SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES CONTENT-TYPE CONTENT-TRANSFER-ENCODING CONTENT-ID CONTENT-LOCATION CONTENT-DISPOSITION LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST PRECEDENCE AUTO-SUBMITTED\)\]\)$/i.test(
              command,
            )
          ) {
            // Starring preserves the current product behavior of attempting a
            // best-effort hydration after its committed flag update.  It is
            // deliberately rejected here: the command must still succeed,
            // and this transcript must remain complete and bounded.
            tagged(socket, tag, "NO", "fictional hydration unavailable");
            stage = "logout";
          } else if (
            stage === "operation-move" &&
            /^UID MOVE (21|22|23|24) "(?:Archive|Spam|INBOX|Trash)"$/i.test(
              command,
            )
          ) {
            const uid = Number(command.split(" ")[2]);
            tagged(socket, tag, "OK", `[COPYUID 77 ${uid} ${uid + 100}] moved`);
            stage = "logout";
          } else if (
            stage === "operation-move" &&
            /^UID MOVE 99 "Trash"$/i.test(command)
          ) {
            tagged(socket, tag, "NO", "fictional provider rejected this move");
            stage = "logout";
          } else if (
            stage === "delete-store" &&
            /^UID STORE 25 \+FLAGS\.SILENT \(\\Deleted\)$/i.test(command)
          ) {
            tagged(socket, tag, "OK", "deleted flag stored");
            stage = "delete-expunge";
          } else if (
            stage === "delete-expunge" &&
            /^UID EXPUNGE 25$/i.test(command)
          ) {
            tagged(socket, tag, "OK", "expunged exact UID");
            stage = "logout";
          } else if (stage === "logout" && /^LOGOUT$/i.test(command)) {
            socket.end(
              `* BYE fictional mail-action complete\r\n${tag} OK logout\r\n`,
            );
            stage = "done";
          } else {
            socket.destroy(
              new Error(
                `unexpected fictional IMAP command at ${stage}: ${command}`,
              ),
            );
            return;
          }
        }
      });
      serial.catch((error) => socket.destroy(error));
    });
  },
);
server.on("error", (error) => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});
server.listen({ host: "127.0.0.1", port: 0 }, async () => {
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("fixture has no TCP address");
  await writeFile(values["port-file"], `${address.port}\n`, { mode: 0o600 });
});
let closing = false;
async function close(signal) {
  if (closing) return;
  closing = true;
  for (const socket of sockets) socket.destroy();
  await new Promise((resolve) => server.close(resolve));
  await transcriptWrites;
  process.exit(0);
}
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    void close(signal).catch((error) => {
      process.stderr.write(`${error.stack ?? error}\n`);
      process.exit(1);
    });
  });
}
