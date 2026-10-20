#!/usr/bin/env node
/**
 * Debug-only loopback IMAP and SMTP fixture for native desktop verification.
 *
 * It never makes an outbound connection. It accepts only `.test` SMTP envelope
 * recipients, serves TLS with the public test-only key below, and prints a
 * single machine-readable `ready` line containing both ports and the CA DER.
 *
 * Usage:
 *   node scripts/native-mail-fixture.mjs
 *   DAKIA_NATIVE_MAIL_FIXTURE=1 \
 *   DAKIA_NATIVE_MAIL_FIXTURE_CA_DER_BASE64=<ready.caDerBase64> npm run dev
 *
 * This script is not product configuration. The Rust client accepts this CA
 * only in debug builds, only with the explicit environment flag, and only for
 * an exact loopback account host.
 */
import { once } from "node:events";
import { fileURLToPath } from "node:url";
import tls from "node:tls";

// These are the deliberately public localhost test credentials also used by
// crates/dakia-core/src/mail.rs. They exist only for deterministic loopback
// TLS transcripts and must never be used outside this fixture.
export const NATIVE_FIXTURE_CA_DER_BASE64 =
  "MIIBoDCCAUWgAwIBAgIUQltW8tjmRLr4QjHNjnIXOGd4oqcwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEowlFdKeMeRMDaJroLiqhOMAQ1dKYMuoX/SXdgSSY0fIcL7K4mv7z8Xqg5iLrw84NQxGZt36GLxNaGfSLmCR6nqNjMGEwHQYDVR0OBBYEFKzJ36GY9x0+2bor86BZVX+U3mOLMB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgKEMAoGCCqGSM49BAMCA0kAMEYCIQD5sjoNPPW9m+gCspyKyj9AOdgwZiavQhgDeIvu5hzVgQIhALJpuju+3/idyBTJ1qGomBG4aRuIO9cHhLwuMtAVtMvt";
const CERT_DER_BASE64 =
  "MIIBxjCCAWygAwIBAgIUM95kwE13FWtKVQEkqO3f8DeMFAgwCgYIKoZIzj0EAwIwHTEbMBkGA1UEAwwSRGFraWEgU01UUCB0ZXN0IENBMB4XDTI2MDczMDE2MDE1NFoXDTM2MDcyNzE2MDE1NFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElFRQ2c4qt7037iUsrzdKiDrS/euRkQ3z5uCpfrYsFVhe3g4ffc5IBLZDWSEUP0EJvyEOOg5KL1by1ZGYC/d+S6OBkjCBjzAMBgNVHRMBAf8EAjAAMA4GA1UdDwEB/wQEAwIHgDATBgNVHSUEDDAKBggrBgEFBQcDATAaBgNVHREEEzARgglsb2NhbGhvc3SHBH8AAAEwHQYDVR0OBBYEFM3UTgErLg6p5+pv13yFNths9Lb9MB8GA1UdIwQYMBaAFKzJ36GY9x0+2bor86BZVX+U3mOLMAoGCCqGSM49BAMCA0gAMEUCIEmgwMiWttP7OvYXRkvPm/5c64vpxLLtT+Jg6E4g+OnYAiEAp742aGep2AEwIRP9YXI8RLjhaseLGUQT7R4AWFwnYZE=";
const KEY_DER_BASE64 =
  "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg8ZClgJ8kAl4AVnA0D9d0PXx2siCJiOmjud/vD1NKSqehRANCAASUVFDZziq3vTfuJSyvN0qIOtL965GRDfPm4Kl+tiwVWF7eDh99zkgEtkNZIRQ/QQm/IQ46DkovVvLVkZgL935L";

const LOOPBACK_HOSTS = new Set(["127.0.0.1", "::1", "localhost"]);
const CONNECTION_TIMEOUT_MS = 35_000;
const CLOSE_TIMEOUT_MS = 5_000;

function pem(label, derBase64) {
  const lines = derBase64.match(/.{1,64}/g)?.join("\n") ?? "";
  return `-----BEGIN ${label}-----\n${lines}\n-----END ${label}-----\n`;
}

function requireLoopbackHost(host) {
  if (!LOOPBACK_HOSTS.has(host)) {
    throw new Error("native mail fixture may bind only 127.0.0.1, ::1, or localhost");
  }
}

function socketWriter(socket) {
  return (value) => socket.write(value, "utf8");
}

function appendData(buffer, chunk) {
  return buffer.length === 0 ? Buffer.from(chunk) : Buffer.concat([buffer, chunk]);
}

function validTestRecipient(address) {
  const parts = address.trim().split("@");
  return (
    parts.length === 2 &&
    parts[0].length > 0 &&
    /^[a-z0-9.-]+\.test$/i.test(parts[1])
  );
}

function parseSmtpEnvelopeRecipient(command) {
  const match = /^RCPT\s+TO:\s*<([^<>\r\n]+)>\s*$/i.exec(command);
  return match?.[1] ?? null;
}

function createSmtpConnection(socket, fixture) {
  const write = socketWriter(socket);
  let buffer = Buffer.alloc(0);
  let inData = false;
  let loginStep = 0;
  const recipients = [];
  write("220 localhost Dakia native fixture ready\r\n");

  socket.on("data", (chunk) => {
    buffer = appendData(buffer, chunk);
    while (buffer.length > 0) {
      if (inData) {
        const terminator = buffer.indexOf("\r\n.\r\n");
        if (terminator < 0) return;
        const bytes = terminator + 2;
        fixture.smtpDataBytes.push(bytes);
        fixture.log({ type: "smtp-data", bytes, recipients: [...recipients] });
        buffer = buffer.subarray(terminator + 5);
        inData = false;
        write("250 2.0.0 queued as native-fixture\r\n");
        continue;
      }
      const lineEnd = buffer.indexOf("\r\n");
      if (lineEnd < 0) return;
      const command = buffer.subarray(0, lineEnd).toString("utf8");
      buffer = buffer.subarray(lineEnd + 2);
      if (loginStep === 1) {
        loginStep = 2;
        write("334 UGFzc3dvcmQ6\r\n");
        continue;
      }
      if (loginStep === 2) {
        loginStep = 0;
        write("235 2.7.0 authenticated\r\n");
        continue;
      }
      if (/^EHLO\s+/i.test(command) || /^HELO\s+/i.test(command)) {
        write(
          "250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 SIZE 52428800\r\n",
        );
      } else if (/^AUTH\s+PLAIN(?:\s|$)/i.test(command)) {
        write("235 2.7.0 authenticated\r\n");
      } else if (/^AUTH\s+LOGIN(?:\s|$)/i.test(command)) {
        loginStep = /\s+LOGIN\s+\S+/i.test(command) ? 2 : 1;
        write(loginStep === 1 ? "334 VXNlcm5hbWU6\r\n" : "334 UGFzc3dvcmQ6\r\n");
      } else if (/^MAIL\s+FROM:/i.test(command)) {
        write("250 2.1.0 sender accepted\r\n");
      } else if (/^RCPT\s+TO:/i.test(command)) {
        const recipient = parseSmtpEnvelopeRecipient(command);
        if (!recipient || !validTestRecipient(recipient)) {
          fixture.violations.push(`refused non-.test SMTP recipient: ${recipient ?? command}`);
          fixture.log({ type: "smtp-recipient-refused", recipient: recipient ?? null });
          write("550 5.1.1 native fixture accepts only .test recipients\r\n");
        } else {
          recipients.push(recipient);
          fixture.acceptedRecipients.push(recipient);
          fixture.log({ type: "smtp-recipient", recipient });
          write("250 2.1.5 recipient accepted\r\n");
        }
      } else if (/^DATA$/i.test(command)) {
        if (recipients.length === 0) {
          write("554 5.5.1 no accepted recipients\r\n");
        } else {
          inData = true;
          write("354 send message\r\n");
        }
      } else if (/^RSET$/i.test(command)) {
        recipients.length = 0;
        write("250 2.0.0 reset\r\n");
      } else if (/^QUIT$/i.test(command)) {
        write("221 2.0.0 bye\r\n");
        socket.end();
      } else {
        write("250 2.0.0 ok\r\n");
      }
    }
  });
}

function imapTaggedOk(write, tag, text = "completed") {
  write(`${tag} OK ${text}\r\n`);
}

function createImapConnection(socket, fixture) {
  const write = socketWriter(socket);
  let buffer = Buffer.alloc(0);
  let append = null;
  let idleTag = null;
  write("* OK Dakia native fixture ready\r\n");

  socket.on("data", (chunk) => {
    buffer = appendData(buffer, chunk);
    while (buffer.length > 0) {
      if (append) {
        if (buffer.length < append.length) return;
        const message = buffer.subarray(0, append.length);
        buffer = buffer.subarray(append.length);
        if (buffer.subarray(0, 2).equals(Buffer.from("\r\n"))) {
          buffer = buffer.subarray(2);
        }
        fixture.appendedBytes.push(message.length);
        fixture.log({ type: "imap-append", bytes: message.length });
        imapTaggedOk(write, append.tag, "[APPENDUID 1 1] appended");
        append = null;
        continue;
      }
      const lineEnd = buffer.indexOf("\r\n");
      if (lineEnd < 0) return;
      const line = buffer.subarray(0, lineEnd).toString("utf8");
      buffer = buffer.subarray(lineEnd + 2);
      if (idleTag && /^DONE$/i.test(line)) {
        imapTaggedOk(write, idleTag, "IDLE completed");
        idleTag = null;
        continue;
      }
      const match = /^(\S+)\s+(.+)$/.exec(line);
      if (!match) {
        write("* BAD malformed command\r\n");
        continue;
      }
      const [, tag, command] = match;
      const verb = command.split(/\s+/, 1)[0].toUpperCase();
      const literal = /\{(\d+)\+?\}$/.exec(command);
      if (verb === "APPEND" && literal) {
        append = { tag, length: Number(literal[1]) };
        write("+ ready for APPEND\r\n");
      } else if (verb === "LOGIN" || verb === "AUTHENTICATE") {
        imapTaggedOk(write, tag, "authenticated");
      } else if (verb === "CAPABILITY") {
        write("* CAPABILITY IMAP4rev1 UIDPLUS IDLE\r\n");
        imapTaggedOk(write, tag, "capability");
      } else if (verb === "LIST") {
        write("* LIST (\\HasNoChildren \\Inbox) \"/\" \"INBOX\"\r\n");
        write("* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n");
        imapTaggedOk(write, tag, "list");
      } else if (verb === "SELECT" || verb === "EXAMINE") {
        write("* FLAGS (\\Seen \\Answered \\Flagged \\Draft)\r\n");
        write("* 0 EXISTS\r\n");
        write("* OK [UIDVALIDITY 1] stable\r\n");
        write("* OK [UIDNEXT 1] next\r\n");
        imapTaggedOk(write, tag, "selected");
      } else if (verb === "STATUS") {
        write("* STATUS \"INBOX\" (UIDVALIDITY 1 UIDNEXT 1)\r\n");
        imapTaggedOk(write, tag, "status");
      } else if (verb === "IDLE") {
        idleTag = tag;
        write("+ idling\r\n");
      } else if (verb === "LOGOUT") {
        write("* BYE closing\r\n");
        imapTaggedOk(write, tag, "logout");
        socket.end();
      } else if (/^(UID|FETCH|NOOP|CHECK|CLOSE|EXPUNGE|STORE|COPY|MOVE)$/i.test(verb)) {
        if (/^UID\s+SEARCH\b/i.test(command)) write("* SEARCH\r\n");
        imapTaggedOk(write, tag);
      } else {
        imapTaggedOk(write, tag);
      }
    }
  });
}

async function listen(server, host) {
  server.listen({ host, port: 0, exclusive: true });
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("fixture did not expose a TCP port");
  return address.port;
}

async function closeServer(server) {
  if (!server.listening) return;
  let timer;
  try {
    await Promise.race([
      new Promise((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("fixture close timed out")),
          CLOSE_TIMEOUT_MS,
        );
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

export async function startNativeMailFixture({ host = "127.0.0.1", log = true } = {}) {
  requireLoopbackHost(host);
  const sockets = new Set();
  const fixture = {
    acceptedRecipients: [],
    appendedBytes: [],
    smtpDataBytes: [],
    violations: [],
    log(event) {
      if (log) process.stdout.write(`${JSON.stringify(event)}\n`);
    },
  };
  const options = {
    key: pem("PRIVATE KEY", KEY_DER_BASE64),
    cert: pem("CERTIFICATE", CERT_DER_BASE64),
  };
  const track = (handler) => (socket) => {
    sockets.add(socket);
    socket.setTimeout(CONNECTION_TIMEOUT_MS, () => socket.destroy());
    socket.once("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    handler(socket, fixture);
  };
  const imap = tls.createServer(options, track(createImapConnection));
  const smtp = tls.createServer(options, track(createSmtpConnection));
  const [imapPort, smtpPort] = await Promise.all([listen(imap, host), listen(smtp, host)]);
  return {
    ...fixture,
    host,
    imapPort,
    smtpPort,
    caDerBase64: NATIVE_FIXTURE_CA_DER_BASE64,
    async close() {
      for (const socket of sockets) socket.destroy();
      await Promise.all([closeServer(imap), closeServer(smtp)]);
    },
  };
}

function printHelp() {
  process.stdout.write(
    "Starts an isolated TLS IMAP/SMTP fixture on loopback and prints JSON connection details.\n",
  );
}

async function runCli() {
  if (process.argv.includes("--help") || process.argv.includes("-h")) {
    printHelp();
    return;
  }
  const fixture = await startNativeMailFixture();
  process.stdout.write(
    `${JSON.stringify({
      type: "ready",
      host: fixture.host,
      imapPort: fixture.imapPort,
      smtpPort: fixture.smtpPort,
      caDerBase64: fixture.caDerBase64,
    })}\n`,
  );
  let closing = false;
  const close = async () => {
    if (closing) return;
    closing = true;
    await fixture.close();
  };
  process.once("SIGINT", () => void close().then(() => process.exit(0)));
  process.once("SIGTERM", () => void close().then(() => process.exit(0)));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  runCli().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
}
