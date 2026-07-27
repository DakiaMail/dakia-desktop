#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const SEMVER =
  /^(?:v)?(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const PLATFORMS = ["darwin-aarch64", "darwin-x86_64"];

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
  return (
    lines.length === 4 &&
    lines[0].startsWith("untrusted comment:") &&
    /^[A-Za-z0-9+/]+={0,2}$/.test(lines[1]) &&
    lines[2].startsWith("trusted comment:") &&
    /^[A-Za-z0-9+/]+={0,2}$/.test(lines[3])
  );
}

export function validateManifest(manifest) {
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

  const keys = Object.keys(manifest.platforms).sort();
  if (keys.join(",") !== [...PLATFORMS].sort().join(",")) {
    throw new Error(
      `Updater platforms must be exactly ${PLATFORMS.join(", ")}.`,
    );
  }

  for (const platform of PLATFORMS) {
    const entry = manifest.platforms[platform];
    const expectedArch = platform === "darwin-aarch64" ? "aarch64" : "x86_64";
    if (!entry || typeof entry !== "object") {
      throw new Error(`Missing updater platform ${platform}.`);
    }
    if (!isTauriMinisignSignature(entry.signature?.trim())) {
      throw new Error(`Invalid updater signature for ${platform}.`);
    }
    let url;
    try {
      url = new URL(entry.url);
    } catch {
      throw new Error(`Invalid updater URL for ${platform}.`);
    }
    if (url.protocol !== "https:") {
      throw new Error(`Updater URL for ${platform} must use HTTPS.`);
    }
    if (!url.pathname.endsWith(`/Dakia-${expectedArch}.app.tar.gz`)) {
      throw new Error(`Updater URL architecture mismatch for ${platform}.`);
    }
  }

  return manifest;
}

export function buildManifest({
  version,
  pubDate,
  notes,
  aarch64Url,
  aarch64Signature,
  x86_64Url,
  x86_64Signature,
}) {
  return validateManifest({
    version,
    pub_date: pubDate,
    notes,
    platforms: {
      "darwin-aarch64": {
        url: aarch64Url,
        signature: aarch64Signature.trim(),
      },
      "darwin-x86_64": {
        url: x86_64Url,
        signature: x86_64Signature.trim(),
      },
    },
  });
}

export function corruptManifestSignatures(manifest) {
  const corrupted = structuredClone(validateManifest(manifest));
  for (const platform of PLATFORMS) {
    const original = corrupted.platforms[platform].signature;
    const decoded = Buffer.from(original, "base64").toString("utf8");
    const lines = decoded.trimEnd().split("\n");
    const first = lines[1][0];
    lines[1] = `${first === "A" ? "B" : "A"}${lines[1].slice(1)}`;
    corrupted.platforms[platform].signature = Buffer.from(
      `${lines.join("\n")}\n`,
    ).toString("base64");
  }
  return validateManifest(corrupted);
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
    validateManifest(manifest);
    process.stdout.write(`Valid updater manifest: ${options.manifest}\n`);
    return;
  }
  if (command === "corrupt-signatures") {
    const manifest = JSON.parse(await readFile(options.manifest, "utf8"));
    const corrupted = corruptManifestSignatures(manifest);
    await writeFile(options.output, `${JSON.stringify(corrupted, null, 2)}\n`);
    return;
  }
  if (command !== "create") {
    throw new Error(
      "Usage: updater-manifest.mjs <create|validate|corrupt-signatures> [options]",
    );
  }

  const notes = options["notes-file"]
    ? (await readFile(options["notes-file"], "utf8")).trim()
    : options.notes;
  const manifest = buildManifest({
    version: options.version,
    pubDate: options["pub-date"],
    notes,
    aarch64Url: options["aarch64-url"],
    aarch64Signature: await readFile(options["aarch64-signature-file"], "utf8"),
    x86_64Url: options["x86-64-url"],
    x86_64Signature: await readFile(options["x86-64-signature-file"], "utf8"),
  });
  await writeFile(options.output, `${JSON.stringify(manifest, null, 2)}\n`);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : error}\n`);
    process.exitCode = 1;
  });
}
