#!/usr/bin/env node

import { createHash, createPublicKey, verify } from "node:crypto";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

function decodeOuterBase64(value, label) {
  if (typeof value !== "string" || !/^[A-Za-z0-9+/]+={0,2}$/.test(value.trim())) {
    throw new Error(`${label} must be a base64 minisign value.`);
  }
  return Buffer.from(value.trim(), "base64").toString("utf8");
}

function decodeRecord(value, length, marker, label) {
  const record = Buffer.from(value, "base64");
  if (
    record.length !== length ||
    !record.subarray(0, 2).equals(Buffer.from(marker))
  ) {
    throw new Error(`Invalid ${label}.`);
  }
  return record;
}

export function verifyUpdaterSignature({ archive, signature, publicKey }) {
  const publicLines = decodeOuterBase64(publicKey, "updater public key")
    .trimEnd()
    .split("\n");
  if (publicLines.length !== 2 || !publicLines[0].startsWith("untrusted comment:")) {
    throw new Error("Invalid updater public key.");
  }
  const publicRecord = decodeRecord(
    publicLines[1],
    42,
    "Ed",
    "updater public key",
  );

  const signatureLines = decodeOuterBase64(signature, "updater signature")
    .trimEnd()
    .split("\n");
  if (
    signatureLines.length !== 4 ||
    !signatureLines[0].startsWith("untrusted comment:") ||
    !signatureLines[2].startsWith("trusted comment: ")
  ) {
    throw new Error("Invalid updater signature.");
  }
  const signatureRecord = decodeRecord(
    signatureLines[1],
    74,
    "ED",
    "updater signature",
  );
  const globalSignature = Buffer.from(signatureLines[3], "base64");
  if (globalSignature.length !== 64) {
    throw new Error("Invalid updater signature.");
  }
  if (!publicRecord.subarray(2, 10).equals(signatureRecord.subarray(2, 10))) {
    throw new Error("Updater signature key does not match the embedded public key.");
  }

  const spki = Buffer.concat([
    Buffer.from("302a300506032b6570032100", "hex"),
    publicRecord.subarray(10),
  ]);
  const key = createPublicKey({ key: spki, format: "der", type: "spki" });
  const digest = createHash("blake2b512").update(archive).digest();
  if (!verify(null, digest, key, signatureRecord.subarray(10))) {
    throw new Error("Updater archive signature verification failed.");
  }
  const trustedComment = Buffer.from(signatureLines[2].slice(17));
  const globalMessage = Buffer.concat([
    signatureRecord.subarray(10),
    trustedComment,
  ]);
  if (!verify(null, globalMessage, key, globalSignature)) {
    throw new Error("Updater trusted-comment signature verification failed.");
  }
}

async function main() {
  const [archivePath, signaturePath, tauriConfigPath] = process.argv.slice(2);
  if (!archivePath || !signaturePath || !tauriConfigPath) {
    throw new Error(
      "Usage: verify-updater-signature.mjs archive signature tauri.conf.json",
    );
  }
  const [archive, signature, tauriConfig] = await Promise.all([
    readFile(archivePath),
    readFile(signaturePath, "utf8"),
    readFile(tauriConfigPath, "utf8").then(JSON.parse),
  ]);
  verifyUpdaterSignature({
    archive,
    signature,
    publicKey: tauriConfig?.plugins?.updater?.pubkey,
  });
  process.stdout.write(`Verified updater signature: ${archivePath}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error.message);
    process.exit(1);
  });
}
