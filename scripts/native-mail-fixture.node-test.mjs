import assert from "node:assert/strict";
import test from "node:test";
import tls from "node:tls";
import { once } from "node:events";
import {
  NATIVE_FIXTURE_CA_DER_BASE64,
  startNativeMailFixture,
} from "./native-mail-fixture.mjs";

function caPem() {
  const lines = NATIVE_FIXTURE_CA_DER_BASE64.match(/.{1,64}/g).join("\n");
  return `-----BEGIN CERTIFICATE-----\n${lines}\n-----END CERTIFICATE-----\n`;
}

async function connect(port) {
  const socket = tls.connect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    ca: caPem(),
    rejectUnauthorized: true,
  });
  const lines = [];
  const waiters = [];
  let buffer = "";
  socket.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    while (buffer.includes("\r\n")) {
      const end = buffer.indexOf("\r\n");
      const line = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      const waiter = waiters.shift();
      if (waiter) waiter(line);
      else lines.push(line);
    }
  });
  await once(socket, "secureConnect");
  return {
    socket,
    async readLine() {
      if (lines.length > 0) return lines.shift();
      return new Promise((resolve) => waiters.push(resolve));
    },
    write(value) {
      socket.write(value);
    },
  };
}

async function readThrough(client, pattern) {
  for (;;) {
    const line = await client.readLine();
    if (pattern.test(line)) return line;
  }
}

test("native loopback fixture accepts SMTP and IMAP APPEND without egress", async () => {
  const fixture = await startNativeMailFixture({ log: false });
  try {
    const imap = await connect(fixture.imapPort);
    assert.match(await imap.readLine(), /^\* OK/);
    imap.write('A1 LOGIN "sender@example.test" "secret"\r\n');
    await readThrough(imap, /^A1 OK/);
    imap.write('A2 APPEND "Sent" (\\Seen) {5}\r\n');
    assert.equal(await imap.readLine(), "+ ready for APPEND");
    imap.write("hello\r\n");
    await readThrough(imap, /^A2 OK \[APPENDUID/);
    imap.write("A3 LOGOUT\r\n");
    await readThrough(imap, /^A3 OK/);

    const smtp = await connect(fixture.smtpPort);
    assert.match(await smtp.readLine(), /^220 /);
    smtp.write("EHLO localhost\r\n");
    await readThrough(smtp, /^250 SIZE/);
    smtp.write("AUTH PLAIN AHNlbmRlckBleGFtcGxlLnRlc3QAc2VjcmV0\r\n");
    assert.match(await smtp.readLine(), /^235 /);
    smtp.write("MAIL FROM:<sender@example.test>\r\n");
    assert.match(await smtp.readLine(), /^250 /);
    smtp.write("RCPT TO:<recipient@example.test>\r\n");
    assert.match(await smtp.readLine(), /^250 /);
    smtp.write("DATA\r\n");
    assert.match(await smtp.readLine(), /^354 /);
    smtp.write("Subject: Fixture\r\n\r\nhello\r\n.\r\n");
    assert.match(await smtp.readLine(), /^250 /);
    smtp.write("QUIT\r\n");
    assert.match(await smtp.readLine(), /^221 /);

    assert.deepEqual(fixture.acceptedRecipients, ["recipient@example.test"]);
    assert.deepEqual(fixture.appendedBytes, [5]);
    assert.equal(fixture.violations.length, 0);
  } finally {
    await fixture.close();
  }
});

test("native loopback fixture refuses a non-test SMTP envelope recipient", async () => {
  const fixture = await startNativeMailFixture({ log: false });
  try {
    const smtp = await connect(fixture.smtpPort);
    await smtp.readLine();
    smtp.write("EHLO localhost\r\n");
    await readThrough(smtp, /^250 SIZE/);
    smtp.write("MAIL FROM:<sender@example.test>\r\n");
    await smtp.readLine();
    smtp.write("RCPT TO:<outside@example.com>\r\n");
    assert.match(await smtp.readLine(), /^550 /);
    assert.match(fixture.violations[0], /outside@example\.com/);
    smtp.socket.destroy();
  } finally {
    await fixture.close();
  }
});
