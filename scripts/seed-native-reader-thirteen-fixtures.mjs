#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { connect } from "node:tls";
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { FIXTURES, loadFixtures } from "./native-reader-thirteen-overlay.mjs";

const DEFAULT_TIMEOUT_MS = 5_000;

export function smtpDataBytes(raw) {
  const normalized = raw.toString("latin1").replace(/\r\n|\r|\n/g, "\r\n");
  return Buffer.from(`${normalized.replace(/^\./gm, "..")}\r\n.\r\n`, "latin1");
}

export function createReplyReader(socket, timeoutMs = DEFAULT_TIMEOUT_MS) {
  let buffered = "";
  let pending;
  let terminalError;
  const settleTerminal = (error) => {
    if (!terminalError) terminalError = error;
    if (pending) {
      const { reject, timer } = pending;
      pending = undefined;
      clearTimeout(timer);
      reject(terminalError);
    }
  };
  const drain = () => {
    if (!pending) return;
    if (terminalError) return settleTerminal(terminalError);
    const index = buffered.indexOf("\r\n");
    if (index < 0) return;
    const value = buffered.slice(0, index + 2);
    buffered = buffered.slice(index + 2);
    const { resolve: resolvePending, timer } = pending;
    pending = undefined;
    clearTimeout(timer);
    resolvePending(value);
  };
  socket.on("data", (chunk) => {
    buffered += chunk;
    drain();
  });
  socket.on("error", (error) => settleTerminal(error));
  socket.on("close", () =>
    settleTerminal(
      new Error("SMTP connection closed before the expected reply"),
    ),
  );
  const line = () =>
    new Promise((resolvePending, reject) => {
      if (pending)
        throw new Error("SMTP reply reader received concurrent reads");
      if (terminalError) return reject(terminalError);
      const timer = setTimeout(() => {
        if (pending) {
          pending = undefined;
          reject(new Error("SMTP reply timed out"));
        }
      }, timeoutMs);
      pending = { resolve: resolvePending, reject, timer };
      drain();
    });
  const expect = async (code) => {
    let value;
    do {
      value = await line();
      if (!value.startsWith(code))
        throw new Error(`expected SMTP ${code}, got ${value.trim()}`);
    } while (value[3] === "-");
  };
  return { expect };
}

export function waitForSecureConnect(socket, timeoutMs = DEFAULT_TIMEOUT_MS) {
  return new Promise((resolvePromise, reject) => {
    let settled = false;
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.off("secureConnect", onSecureConnect);
      socket.off("error", onError);
      socket.off("close", onClose);
      callback(value);
    };
    const onSecureConnect = () => finish(resolvePromise);
    const onError = (error) => finish(reject, error);
    const onClose = () =>
      finish(reject, new Error("SMTP connection closed during TLS handshake"));
    const timer = setTimeout(
      () => finish(reject, new Error("SMTP TLS handshake timed out")),
      timeoutMs,
    );
    socket.once("secureConnect", onSecureConnect);
    socket.once("error", onError);
    socket.once("close", onClose);
  });
}

async function main() {
  const [sourceRoot, portText, caPath] = process.argv.slice(2);
  const port = Number(portText);
  if (
    !sourceRoot ||
    !Number.isInteger(port) ||
    port < 1 ||
    port > 65535 ||
    !caPath
  )
    throw new Error(
      "usage: seed-native-reader-thirteen-fixtures.mjs SOURCE_ROOT SMTP_PORT CA",
    );
  const fixtures = loadFixtures(resolve(sourceRoot));
  const socket = connect({
    host: "127.0.0.1",
    port,
    servername: "localhost",
    ca: await readFile(caPath),
    rejectUnauthorized: true,
    minVersion: "TLSv1.2",
  });
  socket.setEncoding("utf8");
  const { expect } = createReplyReader(socket);
  const command = async (value, code) => {
    socket.write(`${value}\r\n`);
    await expect(code);
  };
  await waitForSecureConnect(socket);
  await expect("220");
  await command("EHLO native-reader-thirteen.localhost", "250");
  await command(
    `AUTH PLAIN ${Buffer.from("\0fictional-reader@localhost\0synthetic-only-password").toString("base64")}`,
    "235",
  );
  for (const [uid] of FIXTURES) {
    const fixture = fixtures.get(uid);
    await command("MAIL FROM:<fixture@example.test>", "250");
    await command("RCPT TO:<fictional-reader@localhost>", "250");
    await command("DATA", "354");
    socket.write(smtpDataBytes(fixture.raw));
    await expect("250");
  }
  await command("QUIT", "221");
  socket.destroy();
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
