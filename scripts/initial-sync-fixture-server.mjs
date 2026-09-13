#!/usr/bin/env node

import { createPrivateKey, X509Certificate } from "node:crypto";
import { writeFileSync } from "node:fs";
import { createServer } from "node:tls";

// Public test-only certificate material. The leaf covers localhost and
// 127.0.0.1. It is the same deterministic CA used by the Rust SMTP tests.
export const fixtureCaDer = Buffer.from(
  "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt",
  "base64",
);
export const fixtureCaPem = new X509Certificate(fixtureCaDer).toString();
const fixtureCertDer = Buffer.from(
  "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=",
  "base64",
);
const fixtureKeyDer = Buffer.from(
  "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L",
  "base64",
);

export function parseArgs(argv) {
  const options = {
    messages: 2_000,
    folderMessages: 0,
    delayMs: 0,
    historyDelayMs: 0,
    sparse: false,
    writeCa: undefined,
    imapPort: 0,
    smtpPort: 0,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--messages") options.messages = Number(argv[++index]);
    else if (value === "--folder-messages")
      options.folderMessages = Number(argv[++index]);
    else if (value === "--delay-ms") options.delayMs = Number(argv[++index]);
    else if (value === "--history-delay-ms")
      options.historyDelayMs = Number(argv[++index]);
    else if (value === "--sparse") options.sparse = true;
    else if (value === "--write-ca") options.writeCa = argv[++index];
    else if (value === "--imap-port") options.imapPort = Number(argv[++index]);
    else if (value === "--smtp-port") options.smtpPort = Number(argv[++index]);
    else throw new Error(`unknown argument: ${value}`);
  }
  if (
    !Number.isInteger(options.messages) ||
    options.messages < 1 ||
    options.messages > 100_000
  )
    throw new Error("--messages must be an integer from 1 to 100000");
  if (
    !Number.isInteger(options.folderMessages) ||
    options.folderMessages < 0 ||
    options.folderMessages > 10_000
  )
    throw new Error("--folder-messages must be an integer from 0 to 10000");
  if (
    !Number.isInteger(options.delayMs) ||
    options.delayMs < 0 ||
    options.delayMs > 30_000
  )
    throw new Error("--delay-ms must be an integer from 0 to 30000");
  if (
    !Number.isInteger(options.historyDelayMs) ||
    options.historyDelayMs < 0 ||
    options.historyDelayMs > 30_000
  )
    throw new Error("--history-delay-ms must be an integer from 0 to 30000");
  if (argv.includes("--write-ca") && !options.writeCa)
    throw new Error("--write-ca requires a path");
  for (const [name, port] of [
    ["--imap-port", options.imapPort],
    ["--smtp-port", options.smtpPort],
  ])
    if (!Number.isInteger(port) || port < 0 || port > 65_535)
      throw new Error(`${name} must be an integer from 0 to 65535`);
  return options;
}

export function fixtureUid(sequence, sparse) {
  return sparse ? sequence * 3 + (sequence % 7 === 0 ? 2 : 0) : sequence;
}

export function parseUidSet(value) {
  const result = [];
  for (const item of value.split(",")) {
    const match = item.match(/^(\d+)(?::(\d+))?$/);
    if (!match) throw new Error(`unsupported UID set: ${value}`);
    const start = Number(match[1]);
    const end = Number(match[2] ?? match[1]);
    const step = start <= end ? 1 : -1;
    for (let uid = start; ; uid += step) {
      result.push(uid);
      if (uid === end) break;
    }
  }
  return result;
}

function tlsOptions() {
  return {
    cert: new X509Certificate(fixtureCertDer).toString(),
    key: createPrivateKey({
      key: fixtureKeyDer,
      format: "der",
      type: "pkcs8",
    }).export({ format: "pem", type: "pkcs8" }),
  };
}

function delayedWrite(socket, value, delayMs, metrics, afterWrite) {
  const bytes = Buffer.byteLength(value);
  metrics.responses += 1;
  metrics.responseBytes += bytes;
  metrics.maxResponseBytes = Math.max(metrics.maxResponseBytes, bytes);
  if (delayMs === 0) socket.write(value, afterWrite);
  else setTimeout(() => socket.write(value, afterWrite), delayMs);
}

function mailboxName(command) {
  return command.match(/(?:SELECT|EXAMINE)\s+"?([^"\s]+)"?/i)?.[1] ?? "INBOX";
}

function statusMailboxName(command) {
  return command.match(/^STATUS\s+"?([^"\s]+)"?/i)?.[1] ?? "INBOX";
}

function appendMailboxName(command) {
  return command.match(/^APPEND\s+"?([^"\s]+)"?/i)?.[1];
}

function folderSlug(folder) {
  return folder.toLowerCase().replace(/[^a-z0-9]+/g, "-");
}

function headerFor(folder, uid) {
  const slug = folderSlug(folder);
  const inbox = folder === "INBOX";
  return [
    "Date: Wed, 30 Jul 2026 10:00:00 +0000",
    `From: Fixture Sender ${uid} <sender-${uid}@example.test>`,
    "To: Reader <reader@example.test>",
    inbox
      ? `Subject: Fictional acceptance message ${uid}`
      : `Subject: Fictional ${folder} acceptance message ${uid}`,
    inbox
      ? `Message-ID: <fixture-${uid}@example.test>`
      : `Message-ID: <fixture-${slug}-${uid}@example.test>`,
    "Content-Type: text/plain; charset=utf-8",
    "",
    "",
  ].join("\r\n");
}

function bodyFor(folder, uid) {
  return folder === "INBOX"
    ? `This is fictional message ${uid}.\r\n`
    : `This is fictional ${folder} message ${uid}.\r\n`;
}

const fixtureFolders = ["INBOX", "Sent", "Archive", "Drafts", "Spam", "Trash"];

function canonicalFolder(value) {
  return fixtureFolders.find(
    (folder) => folder.toLowerCase() === value.toLowerCase(),
  );
}

function createMailboxState(options) {
  return new Map(
    fixtureFolders.map((folder) => [
      folder,
      {
        generated:
          folder === "INBOX" ? options.messages : options.folderMessages,
        appended: [],
        flags: new Map(),
      },
    ]),
  );
}

function generatedUid(sequence, options) {
  return fixtureUid(sequence, options.sparse);
}

function mailboxUids(mailboxes, folder, options) {
  const mailbox = mailboxes.get(folder);
  if (!mailbox) return [];
  return [
    ...Array.from({ length: mailbox.generated }, (_, index) =>
      generatedUid(index + 1, options),
    ),
    ...mailbox.appended.map(({ uid }) => uid),
  ];
}

function mailboxCount(mailboxes, folder) {
  const mailbox = mailboxes.get(folder);
  return mailbox ? mailbox.generated + mailbox.appended.length : 0;
}

function mailboxUidBySequence(mailboxes, folder, sequence, options) {
  const mailbox = mailboxes.get(folder);
  if (!mailbox || sequence < 1) return undefined;
  if (sequence <= mailbox.generated) return generatedUid(sequence, options);
  return mailbox.appended[sequence - mailbox.generated - 1]?.uid;
}

function mailboxUidNext(mailboxes, folder, options) {
  const mailbox = mailboxes.get(folder);
  if (!mailbox) return 1;
  const lastGenerated = mailbox.generated
    ? generatedUid(mailbox.generated, options)
    : 0;
  return Math.max(lastGenerated, mailbox.appended.at(-1)?.uid ?? 0) + 1;
}

function canonicalFlag(flag) {
  const systemFlags = new Map([
    ["\\seen", "\\Seen"],
    ["\\answered", "\\Answered"],
    ["\\flagged", "\\Flagged"],
    ["\\deleted", "\\Deleted"],
    ["\\draft", "\\Draft"],
  ]);
  return systemFlags.get(flag.toLowerCase()) ?? flag;
}

function mailboxFlags(mailboxes, folder, uid) {
  const flags = mailboxes.get(folder)?.flags.get(uid) ?? new Set();
  return `(${[...flags].join(" ")})`;
}

function messageIdFromRaw(raw) {
  return raw.match(/^Message-ID:\s*(.+)$/im)?.[1]?.trim();
}

function boundedUids(uids, command) {
  const range = command.match(/\bUID\s+(\d+):(\d+)\b/i);
  if (!range) return uids;
  const low = Number(range[1]);
  const high = Number(range[2]);
  return uids.filter((uid) => uid >= low && uid <= high);
}

function generatedSequenceForUid(uid, count, sparse) {
  if (!sparse) return uid >= 1 && uid <= count ? uid : undefined;
  const candidate = Math.floor(uid / 3);
  for (const sequence of [candidate - 1, candidate, candidate + 1])
    if (
      sequence >= 1 &&
      sequence <= count &&
      fixtureUid(sequence, true) === uid
    )
      return sequence;
  return undefined;
}

function mailboxEntryByUid(mailboxes, folder, uid, options) {
  const mailbox = mailboxes.get(folder);
  if (!mailbox) return undefined;
  const appended = mailbox.appended.find((entry) => entry.uid === uid);
  if (appended) return appended;
  const sequence = generatedSequenceForUid(
    uid,
    mailbox.generated,
    options.sparse,
  );
  if (!sequence) return undefined;
  const header = headerFor(folder, uid);
  const body = bodyFor(folder, uid);
  return { uid, folder, header, body, raw: `${header}${body}` };
}

function partialLiteral(value, command) {
  const length = Number(command.match(/<0\.(\d+)>/i)?.[1]);
  if (!Number.isInteger(length)) return value;
  return Buffer.from(value).subarray(0, length).toString();
}

function splitMimeEntity(raw) {
  const separator = raw.match(/\r?\n\r?\n/);
  if (separator?.index === undefined) return { header: "", body: raw };
  const bodyStart = separator.index + separator[0].length;
  return {
    header: `${raw.slice(0, separator.index)}\r\n\r\n`,
    body: raw.slice(bodyStart),
  };
}

function mimeHeaderValue(header, name) {
  const unfolded = header.replace(/\r?\n[ \t]+/g, " ");
  return unfolded.match(new RegExp(`^${name}:\\s*(.+)$`, "im"))?.[1]?.trim();
}

function mimeContentType(header) {
  const value = mimeHeaderValue(header, "Content-Type") ?? "text/plain";
  const mediaType = value.match(/^([^/;\s]+)\/([^;\s]+)/);
  const boundary = value.match(/\bboundary\s*=\s*(?:"([^"]+)"|([^;\s]+))/i);
  const charset = value.match(/\bcharset\s*=\s*(?:"([^"]+)"|([^;\s]+))/i);
  return {
    type: mediaType?.[1]?.toUpperCase() ?? "TEXT",
    subtype: mediaType?.[2]?.toUpperCase() ?? "PLAIN",
    boundary: boundary?.[1] ?? boundary?.[2],
    charset: charset?.[1] ?? charset?.[2],
  };
}

function parseMultipartChildren(body, boundary) {
  if (!boundary || boundary.length > 200) return [];
  const marker = `--${boundary}`;
  const children = [];
  for (const segment of body.split(marker).slice(1)) {
    if (segment.startsWith("--")) break;
    const raw = segment.replace(/^\r?\n/, "").replace(/\r?\n$/, "");
    if (raw) children.push(parseMimeEntity(raw));
    if (children.length >= 16) break;
  }
  return children;
}

function parseMimeEntity(raw) {
  const { header, body } = splitMimeEntity(raw);
  const contentType = mimeContentType(header);
  const transferEncoding =
    mimeHeaderValue(header, "Content-Transfer-Encoding")?.toUpperCase() ??
    "7BIT";
  const children =
    contentType.type === "MULTIPART"
      ? parseMultipartChildren(body, contentType.boundary)
      : [];
  return { raw, header, body, contentType, transferEncoding, children };
}

function imapQuoted(value) {
  return `"${String(value).replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`;
}

function bodyStructure(node) {
  const { type, subtype, boundary, charset } = node.contentType;
  if (type === "MULTIPART" && node.children.length) {
    const parameters = boundary
      ? `(${imapQuoted("BOUNDARY")} ${imapQuoted(boundary)})`
      : "NIL";
    return `(${node.children.map(bodyStructure).join(" ")} ${imapQuoted(subtype)} ${parameters} NIL NIL)`;
  }
  const parameters = charset
    ? `(${imapQuoted("CHARSET")} ${imapQuoted(charset)})`
    : "NIL";
  const lines = node.body.split(/\r?\n/).length;
  return `(${imapQuoted(type)} ${imapQuoted(subtype)} ${parameters} NIL NIL ${imapQuoted(node.transferEncoding)} ${Buffer.byteLength(node.body)} ${lines})`;
}

function mimeOnlyHeader(header) {
  const fields = [];
  for (const line of header.replace(/\r?\n\r?\n$/, "").split(/\r?\n/)) {
    if (/^[ \t]/.test(line) && fields.length) fields.at(-1).push(line);
    else fields.push([line]);
  }
  return `${fields
    .filter(([line]) => /^(?:MIME-Version|Content-[^:]+):/i.test(line))
    .flat()
    .join("\r\n")}\r\n\r\n`;
}

function mimeSection(node, section) {
  const path = section
    .replace(/\.MIME$/i, "")
    .split(".")
    .map(Number);
  let current = node;
  if (
    current.contentType.type !== "MULTIPART" &&
    path.length === 1 &&
    path[0] === 1
  )
    path.length = 0;
  for (const part of path) {
    if (!Number.isInteger(part) || part < 1 || part > current.children.length)
      return undefined;
    current = current.children[part - 1];
  }
  return section.toUpperCase().endsWith(".MIME")
    ? mimeOnlyHeader(current.header)
    : current.body;
}

function fetchResponse({ command, entry, flags, sequence, headerLiteralName }) {
  const upper = command.toUpperCase();
  const tokens = [`UID ${entry.uid}`];
  const mime = parseMimeEntity(entry.raw);
  if (/\bFLAGS\b/i.test(command)) tokens.push(`FLAGS ${flags}`);
  if (/\bINTERNALDATE\b/i.test(command))
    tokens.push('INTERNALDATE "30-Jul-2026 10:00:00 +0000"');
  if (/\bRFC822\.SIZE\b/i.test(command))
    tokens.push(`RFC822.SIZE ${Buffer.byteLength(entry.raw)}`);
  if (/\bBODYSTRUCTURE\b/i.test(command))
    tokens.push(`BODYSTRUCTURE ${bodyStructure(mime)}`);

  let literalName;
  let literal;
  if (/BODY\.PEEK\[HEADER\.FIELDS/i.test(command)) {
    literalName = headerLiteralName;
    literal = entry.header;
  } else if (/BODY\.PEEK\[\](?:<0\.\d+>)?/i.test(command)) {
    literalName = upper.includes("<0.") ? "BODY[]<0>" : "BODY[]";
    literal = partialLiteral(entry.raw, command);
  } else {
    const section = command.match(/BODY\.PEEK\[([^\]]+)\](?:<0\.\d+>)?/i)?.[1];
    if (section) {
      literalName = `BODY[${section}]${upper.includes("<0.") ? "<0>" : ""}`;
      const sectionValue = mimeSection(mime, section);
      if (sectionValue !== undefined)
        literal = partialLiteral(sectionValue, command);
    }
  }
  if (literal !== undefined)
    tokens.push(`${literalName} {${Buffer.byteLength(literal)}}\r\n${literal}`);
  return `* ${sequence} FETCH (${tokens.join(" ")})\r\n`;
}

function imapServer(options, events, metrics, mailboxes) {
  return createServer(tlsOptions(), (socket) => {
    socket.setEncoding("utf8");
    delayedWrite(
      socket,
      "* OK Dakia fictional IMAP fixture ready\r\n",
      0,
      metrics,
    );
    let buffer = "";
    let append = null;
    let idleTag = null;
    let selectedFolder = "INBOX";
    const reply = (value, afterWrite) =>
      delayedWrite(socket, value, options.delayMs, metrics, afterWrite);

    socket.on("data", (chunk) => {
      buffer += chunk;
      while (true) {
        if (append) {
          if (Buffer.byteLength(buffer) < append.bytes + 2) return;
          const payload = buffer.slice(0, append.bytes);
          buffer = buffer.slice(append.bytes + 2);
          const mailbox = mailboxes.get(append.folder);
          const uid = mailboxUidNext(mailboxes, append.folder, options);
          const headerEnd = payload.search(/\r?\n\r?\n/);
          const header =
            headerEnd >= 0 ? payload.slice(0, headerEnd + 4) : payload;
          mailbox.appended.push({
            uid,
            folder: append.folder,
            header,
            body: headerEnd >= 0 ? payload.slice(headerEnd + 4) : "",
            raw: payload,
          });
          if (append.flags.length)
            mailbox.flags.set(uid, new Set(append.flags.map(canonicalFlag)));
          events.push({
            protocol: "imap",
            event: "append",
            bytes: Buffer.byteLength(payload),
            folder: append.folder,
            uid,
            messageId: messageIdFromRaw(payload) ?? null,
          });
          reply(`${append.tag} OK [APPENDUID 4242 ${uid}] APPEND complete\r\n`);
          append = null;
          continue;
        }
        const newline = buffer.indexOf("\r\n");
        if (newline < 0) return;
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 2);
        events.push({
          protocol: "imap",
          event: "command",
          line: line
            .replace(/^(\S+\s+LOGIN\s+).+$/i, "$1[redacted]")
            .replace(/^(\S+\s+AUTHENTICATE\s+).+$/i, "$1[redacted]"),
        });
        if (line === "DONE" && idleTag) {
          reply(`${idleTag} OK IDLE complete\r\n`);
          idleTag = null;
          continue;
        }
        const commandMatch = line.match(/^(\S+)\s+([\s\S]+)$/);
        if (!commandMatch) continue;
        const [, tag, command] = commandMatch;
        metrics.commands += 1;
        const upper = command.toUpperCase();
        if (upper.startsWith("LOGIN ") || upper.startsWith("AUTHENTICATE "))
          reply(`${tag} OK authenticated\r\n`);
        else if (upper === "CAPABILITY")
          reply(
            `* CAPABILITY IMAP4rev1 IDLE UIDPLUS\r\n${tag} OK capability\r\n`,
          );
        else if (upper.startsWith("LIST "))
          reply(
            `* LIST (\\HasNoChildren \\Inbox) "/" "INBOX"\r\n* LIST (\\HasNoChildren \\Sent) "/" "Sent"\r\n* LIST (\\HasNoChildren \\Archive) "/" "Archive"\r\n* LIST (\\HasNoChildren \\Drafts) "/" "Drafts"\r\n* LIST (\\HasNoChildren \\Junk) "/" "Spam"\r\n* LIST (\\HasNoChildren \\Trash) "/" "Trash"\r\n${tag} OK listed\r\n`,
          );
        else if (upper.startsWith("SELECT ") || upper.startsWith("EXAMINE ")) {
          const folder = canonicalFolder(mailboxName(command));
          if (!folder) reply(`${tag} NO no such mailbox\r\n`);
          else {
            selectedFolder = folder;
            const count = mailboxCount(mailboxes, folder);
            const next = mailboxUidNext(mailboxes, folder, options);
            reply(
              `* ${count} EXISTS\r\n* OK [UIDVALIDITY 4242] stable fixture\r\n* OK [UIDNEXT ${next}] next\r\n${tag} OK selected\r\n`,
            );
          }
        } else if (upper.startsWith("STATUS ")) {
          const folder = canonicalFolder(statusMailboxName(command));
          if (!folder) reply(`${tag} NO no such mailbox\r\n`);
          else {
            const next = mailboxUidNext(mailboxes, folder, options);
            reply(
              `* STATUS "${folder}" (UIDVALIDITY 4242 UIDNEXT ${next})\r\n${tag} OK status\r\n`,
            );
          }
        } else if (/^FETCH\s+/i.test(command)) {
          const match = command.match(/^FETCH\s+(\d+):(\d+)/i);
          if (!match) reply(`${tag} BAD unsupported FETCH\r\n`);
          else {
            const start = Number(match[1]);
            const end = Number(match[2]);
            const count = mailboxCount(mailboxes, selectedFolder);
            let response = "";
            for (
              let sequence = Math.max(1, start);
              sequence <= Math.min(end, count);
              sequence += 1
            ) {
              const uid = mailboxUidBySequence(
                mailboxes,
                selectedFolder,
                sequence,
                options,
              );
              const entry = mailboxEntryByUid(
                mailboxes,
                selectedFolder,
                uid,
                options,
              );
              response += fetchResponse({
                command,
                entry,
                flags: mailboxFlags(mailboxes, selectedFolder, uid),
                sequence,
                headerLiteralName:
                  "BODY[HEADER.FIELDS (DATE FROM TO CC BCC SUBJECT MESSAGE-ID IN-REPLY-TO REFERENCES LIST-ID LIST-UNSUBSCRIBE LIST-UNSUBSCRIBE-POST X-AUTO-RESPONSE-SUPPRESS AUTO-SUBMITTED PRECEDENCE FEEDBACK-ID X-FEEDBACK-ID X-SES-OUTGOING CONTENT-TYPE CONTENT-TRANSFER-ENCODING MIME-VERSION X-GM-MSGID X-GM-THRID X-GM-LABELS)]",
              });
            }
            reply(`${response}${tag} OK fetched\r\n`);
          }
        } else if (/^UID SEARCH/i.test(command)) {
          let uids = boundedUids(
            mailboxUids(mailboxes, selectedFolder, options),
            command,
          );
          const messageIdMatch = command.match(
            /HEADER\s+"?MESSAGE-ID"?\s+(?:"([^"]+)"|(\S+))/i,
          );
          const messageId = messageIdMatch?.[1] ?? messageIdMatch?.[2];
          if (messageId)
            uids = uids.filter((uid) => {
              const entry = mailboxEntryByUid(
                mailboxes,
                selectedFolder,
                uid,
                options,
              );
              return messageIdFromRaw(entry.raw) === messageId;
            });
          delayedWrite(
            socket,
            `* SEARCH ${uids.join(" ")}\r\n${tag} OK searched\r\n`,
            messageId
              ? options.delayMs
              : options.historyDelayMs || options.delayMs,
            metrics,
          );
        } else if (/^UID STORE/i.test(command)) {
          const match = command.match(
            /^UID STORE\s+(\S+)\s+([+-])FLAGS(?:\.SILENT)?\s+\(([^)]*)\)/i,
          );
          if (!match) reply(`${tag} BAD unsupported UID STORE\r\n`);
          else {
            const membership = new Set(
              mailboxUids(mailboxes, selectedFolder, options),
            );
            const flags = match[3].trim() ? match[3].trim().split(/\s+/) : [];
            for (const uid of parseUidSet(match[1])) {
              if (!membership.has(uid)) continue;
              const mailbox = mailboxes.get(selectedFolder);
              const current = new Set(mailbox.flags.get(uid) ?? []);
              for (const flag of flags) {
                if (match[2] === "+") current.add(canonicalFlag(flag));
                else
                  for (const existing of current)
                    if (existing.toLowerCase() === flag.toLowerCase())
                      current.delete(existing);
              }
              if (current.size) mailbox.flags.set(uid, current);
              else mailbox.flags.delete(uid);
              events.push({
                protocol: "imap",
                event: "store",
                folder: selectedFolder,
                uid,
                operation: match[2],
                flags: [...current],
              });
            }
            reply(`${tag} OK stored\r\n`);
          }
        } else if (/^UID FETCH/i.test(command)) {
          const set = command.match(/^UID FETCH\s+(\S+)/i)?.[1];
          if (!set) reply(`${tag} BAD missing UID set\r\n`);
          else {
            let response = "";
            for (const uid of parseUidSet(set)) {
              const entry = mailboxEntryByUid(
                mailboxes,
                selectedFolder,
                uid,
                options,
              );
              if (!entry) continue;
              const sequence =
                mailboxUids(mailboxes, selectedFolder, options).indexOf(uid) +
                1;
              response += fetchResponse({
                command,
                entry,
                flags: mailboxFlags(mailboxes, selectedFolder, uid),
                sequence,
                headerLiteralName: "BODY[HEADER]",
              });
            }
            reply(`${response}${tag} OK fetched\r\n`);
          }
        } else if (upper === "IDLE") {
          idleTag = tag;
          reply("+ idling\r\n");
        } else if (upper.startsWith("APPEND ")) {
          const bytes = Number(command.match(/\{(\d+)\}$/)?.[1]);
          const folder = canonicalFolder(appendMailboxName(command) ?? "");
          const flags =
            command
              .match(/^APPEND\s+"?[^"\s]+"?\s+\(([^)]*)\)/i)?.[1]
              ?.trim()
              .split(/\s+/)
              .filter(Boolean) ?? [];
          if (!folder) reply(`${tag} NO no such mailbox\r\n`);
          else if (!Number.isInteger(bytes))
            reply(`${tag} BAD missing literal\r\n`);
          else {
            append = { tag, bytes, folder, flags };
            reply("+ continue\r\n");
          }
        } else if (upper === "LOGOUT") {
          reply(`* BYE fixture closing\r\n${tag} OK logout\r\n`, () =>
            socket.end(),
          );
        } else reply(`${tag} OK fixture accepted\r\n`);
      }
    });
  });
}

function smtpServer(options, events, metrics) {
  return createServer(tlsOptions(), (socket) => {
    socket.setEncoding("utf8");
    delayedWrite(
      socket,
      "220 localhost Dakia fictional SMTP fixture\r\n",
      0,
      metrics,
    );
    let buffer = "";
    let data = false;
    let message = "";
    const reply = (value, afterWrite) =>
      delayedWrite(socket, value, options.delayMs, metrics, afterWrite);
    socket.on("data", (chunk) => {
      buffer += chunk;
      let newline;
      while ((newline = buffer.indexOf("\r\n")) >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 2);
        if (data) {
          if (line === ".") {
            events.push({
              protocol: "smtp",
              event: "accepted",
              bytes: Buffer.byteLength(message),
            });
            data = false;
            message = "";
            reply("250 2.0.0 queued as fictional-fixture\r\n");
          } else message += `${line}\r\n`;
          continue;
        }
        events.push({
          protocol: "smtp",
          event: "command",
          line: line.replace(/AUTH .*/i, "AUTH [redacted]"),
        });
        metrics.commands += 1;
        const upper = line.toUpperCase();
        if (upper.startsWith("EHLO") || upper.startsWith("HELO"))
          reply("250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n");
        else if (upper.startsWith("AUTH "))
          reply("235 2.7.0 authenticated\r\n");
        else if (upper.startsWith("MAIL FROM:"))
          reply("250 2.1.0 sender accepted\r\n");
        else if (upper.startsWith("RCPT TO:"))
          reply("250 2.1.5 recipient accepted\r\n");
        else if (upper === "DATA") {
          data = true;
          reply("354 send message\r\n");
        } else if (upper === "QUIT") {
          reply("221 2.0.0 bye\r\n", () => socket.end());
        } else reply("250 2.0.0 accepted\r\n");
      }
    });
  });
}

async function listen(server, port = 0) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  return server.address().port;
}

export async function startFixtureServers(options) {
  options = {
    folderMessages: 0,
    historyDelayMs: 0,
    imapPort: 0,
    smtpPort: 0,
    ...options,
  };
  const events = [];
  const metrics = {
    imap: { commands: 0, responses: 0, responseBytes: 0, maxResponseBytes: 0 },
    smtp: { commands: 0, responses: 0, responseBytes: 0, maxResponseBytes: 0 },
  };
  const mailboxes = createMailboxState(options);
  const imap = imapServer(options, events, metrics.imap, mailboxes);
  const smtp = smtpServer(options, events, metrics.smtp);
  const [imapPort, smtpPort] = await Promise.all([
    listen(imap, options.imapPort),
    listen(smtp, options.smtpPort),
  ]);
  return {
    imapPort,
    smtpPort,
    events,
    metrics,
    close: async () => {
      await Promise.all(
        [imap, smtp].map(
          (server) =>
            new Promise((resolve, reject) =>
              server.close((error) => (error ? reject(error) : resolve())),
            ),
        ),
      );
    },
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.writeCa) writeFileSync(options.writeCa, fixtureCaDer);
  const fixture = await startFixtureServers(options);
  process.stdout.write(
    `${JSON.stringify({ ready: true, imapPort: fixture.imapPort, smtpPort: fixture.smtpPort, ...options, writeCa: options.writeCa ?? null })}\n`,
  );
  const stop = () => {
    process.stderr.write(
      `${JSON.stringify({ metrics: fixture.metrics, events: fixture.events })}\n`,
    );
    fixture.close().catch((error) => {
      process.stderr.write(`${error.stack ?? error}\n`);
      process.exitCode = 1;
    });
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
}

if (process.argv[1] && new URL(import.meta.url).pathname === process.argv[1])
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
