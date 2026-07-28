#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const SEMVER =
  /^(?:v)?(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const PLATFORM = "darwin-aarch64";

function isTauriMinisignSignature(value) {
  if (
    typeof value !== "string" ||
    !/^[A-Za-z0-9+/]+={0,2}$/.test(value) ||
    value.length % 4 !== 0
  ) {
    return false;
  }
  let decoded;
  try {
    decoded = Buffer.from(value, "base64").toString("utf8");
  } catch {
    return false;
  }
  const lines = decoded.trimEnd().split("\n");
  const signatureRecord = Buffer.from(lines[1] ?? "", "base64");
  const trustedSignature = Buffer.from(lines[3] ?? "", "base64");
  return (
    lines.length === 4 &&
    lines[0].startsWith("untrusted comment:") &&
    /^[A-Za-z0-9+/]+={0,2}$/.test(lines[1]) &&
    signatureRecord.length === 74 &&
    signatureRecord.subarray(0, 2).equals(Buffer.from("ED")) &&
    lines[2].startsWith("trusted comment:") &&
    /^[A-Za-z0-9+/]+={0,2}$/.test(lines[3]) &&
    trustedSignature.length === 64
  );
}

function minisignKeyId(value, label, marker, recordLength, lineCount) {
  if (
    typeof value !== "string" ||
    !/^[A-Za-z0-9+/]+={0,2}$/.test(value) ||
    value.length % 4 !== 0
  ) {
    throw new Error(`${label} must be a base64 minisign value.`);
  }
  const lines = Buffer.from(value, "base64")
    .toString("utf8")
    .trimEnd()
    .split("\n");
  const key = Buffer.from(lines[1] ?? "", "base64");
  if (
    lines.length !== lineCount ||
    !/^[A-Za-z0-9+/]+={0,2}$/.test(lines[1] ?? "") ||
    key.length !== recordLength ||
    !key.subarray(0, 2).equals(Buffer.from(marker))
  ) {
    throw new Error(`Invalid ${label}.`);
  }
  // Minisign records begin with a two-byte algorithm/type marker. Public keys
  // use "Ed", while signatures use "ED"; the following eight bytes are the
  // shared key identifier.
  return key.subarray(2, 10).toString("hex");
}

export function validateManifest(manifest, updaterPublicKey) {
  if (!manifest || typeof manifest !== "object") {
    throw new Error("Updater manifest must be a JSON object.");
  }
  const versionMatch =
    typeof manifest.version === "string"
      ? manifest.version.match(SEMVER)
      : null;
  const invalidNumericPrerelease = versionMatch?.[1]
    ?.split(".")
    .some((identifier) => /^\d+$/.test(identifier) && /^0\d+/.test(identifier));
  if (!versionMatch || invalidNumericPrerelease) {
    throw new Error(`Invalid updater SemVer: ${String(manifest.version)}`);
  }
  if (
    manifest.pub_date !== undefined &&
    (!Number.isFinite(Date.parse(manifest.pub_date)) ||
      !/^\d{4}-\d{2}-\d{2}T/.test(manifest.pub_date))
  ) {
    throw new Error("Updater pub_date must be RFC 3339.");
  }
  if (!manifest.platforms || typeof manifest.platforms !== "object") {
    throw new Error("Updater manifest is missing platforms.");
  }
  if (manifest.notes !== undefined && typeof manifest.notes !== "string") {
    throw new Error("Updater notes must be a string.");
  }

  const keys = Object.keys(manifest.platforms);
  if (keys.length !== 1 || keys[0] !== PLATFORM) {
    throw new Error(`Updater platforms must contain only ${PLATFORM}.`);
  }

  const entry = manifest.platforms[PLATFORM];
  if (!entry || typeof entry !== "object") {
    throw new Error(`Missing updater platform ${PLATFORM}.`);
  }
  if (!isTauriMinisignSignature(entry.signature?.trim())) {
    throw new Error(`Invalid updater signature for ${PLATFORM}.`);
  }
  let url;
  try {
    url = new URL(entry.url);
  } catch {
    throw new Error(`Invalid updater URL for ${PLATFORM}.`);
  }
  if (url.protocol !== "https:") {
    throw new Error(`Updater URL for ${PLATFORM} must use HTTPS.`);
  }
  if (!url.pathname.endsWith("/Dakia-aarch64.app.tar.gz")) {
    throw new Error(`Updater URL architecture mismatch for ${PLATFORM}.`);
  }
  if (
    updaterPublicKey !== undefined &&
    minisignKeyId(entry.signature, "updater signature", "ED", 74, 4) !==
      minisignKeyId(updaterPublicKey, "updater public key", "Ed", 42, 2)
  ) {
    throw new Error(
      "Updater signature does not match the embedded public key.",
    );
  }

  return manifest;
}

export function buildManifest({
  version,
  pubDate,
  notes,
  aarch64Url,
  aarch64Signature,
  updaterPublicKey,
}) {
  return validateManifest(
    {
      version,
      pub_date: pubDate,
      notes,
      platforms: {
        "darwin-aarch64": {
          url: aarch64Url,
          signature: aarch64Signature.trim(),
        },
      },
    },
    updaterPublicKey,
  );
}

function parseArgs(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!key?.startsWith("--") || value === undefined) {
      throw new Error(`Invalid argument near ${key ?? "<end>"}.`);
    }
    options[key.slice(2)] = value;
  }
  return options;
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  const options = parseArgs(args);
  if (command === "validate") {
    const manifest = JSON.parse(await readFile(options.manifest, "utf8"));
    const tauriConfig = options["tauri-config"]
      ? JSON.parse(await readFile(options["tauri-config"], "utf8"))
      : undefined;
    validateManifest(manifest, tauriConfig?.plugins?.updater?.pubkey);
    process.stdout.write(`Valid updater manifest: ${options.manifest}\n`);
    return;
  }
  if (command !== "create") {
    throw new Error("Usage: updater-manifest.mjs <create|validate> [options]");
  }

  const notes = options["notes-file"]
    ? (await readFile(options["notes-file"], "utf8")).trim()
    : options.notes;
  const tauriConfig = options["tauri-config"]
    ? JSON.parse(await readFile(options["tauri-config"], "utf8"))
    : undefined;
  const manifest = buildManifest({
    version: options.version,
    pubDate: options["pub-date"],
    notes,
    aarch64Url: options["aarch64-url"],
    aarch64Signature: await readFile(options["aarch64-signature-file"], "utf8"),
    updaterPublicKey: tauriConfig?.plugins?.updater?.pubkey,
  });
  await writeFile(options.output, `${JSON.stringify(manifest, null, 2)}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : error}\n`);
    process.exitCode = 1;
  });
}
