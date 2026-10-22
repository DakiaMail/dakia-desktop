#!/usr/bin/env node

import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";

const rawlogRoot = process.argv[2];
const fixturePath = process.argv[3];
if (!rawlogRoot || !fixturePath) {
  console.error(
    "Usage: verify-dovecot-selective-rawlog.mjs <rawlog-directory> <fixture.eml>",
  );
  process.exit(64);
}

async function filesBelow(directory) {
  const files = [];
  for (const entry of await readdir(directory)) {
    const candidate = path.join(directory, entry);
    const metadata = await stat(candidate);
    if (metadata.isDirectory()) files.push(...(await filesBelow(candidate)));
    if (metadata.isFile()) files.push(candidate);
  }
  return files;
}

function fail(message) {
  throw new Error(`Dovecot selective MIME rawlog rejected: ${message}`);
}

const files = (await filesBelow(rawlogRoot)).sort();
if (files.length === 0) fail("Dovecot produced no post-login rawlog files");

const fixtureSha256 = createHash("sha256")
  .update(await readFile(fixturePath))
  .digest("hex");
if (
  fixtureSha256 !==
  "948d62f3a3f1519475c6b0773cc37c2542a54625004d993b028c05b25b84f580"
) {
  fail(`the governed fixture digest changed: ${fixtureSha256}`);
}

const traffic = (
  await Promise.all(files.map((file) => readFile(file, "utf8")))
).join("\n");
const commands = traffic
  .split(/\r?\n/u)
  .map((line) => line.trim())
  .filter((line) => /\bUID FETCH\b/iu.test(line));

const metadata = commands.find((command) =>
  /\bUID FETCH (\d+) \(FLAGS INTERNALDATE BODYSTRUCTURE BODY\.PEEK\[HEADER\.FIELDS /iu.test(
    command,
  ),
);
if (!metadata) fail("the production BODYSTRUCTURE and header fetch was not observed");
const uid = metadata.match(/\bUID FETCH (\d+) /iu)?.[1];
if (!uid) fail("the governed fixture UID could not be identified");

for (const section of ["1.MIME", "1.1.MIME", "1.2.MIME", "1.1", "1.2"]) {
  const expected = `UID FETCH ${uid} (BODY.PEEK[${section}])`;
  if (!commands.some((command) => command.includes(expected))) {
    fail(`missing production selective command ${expected}`);
  }
}

const fullRawFallback = `UID FETCH ${uid} (FLAGS INTERNALDATE BODY.PEEK[])`;
if (commands.some((command) => command.includes(fullRawFallback))) {
  fail(`unexpected full-message recovery command ${fullRawFallback}`);
}
if (/\bUID STORE\b|\bSTORE \d+/iu.test(traffic)) {
  fail("a mutating STORE command appeared in the read-only acceptance session");
}

const normalizedCommands = commands
  .filter((command) => command.includes(`UID FETCH ${uid} `))
  .map((command) => command.replace(/^\S+\s+/u, ""));
const commandDigest = createHash("sha256")
  .update(normalizedCommands.join("\n"))
  .digest("hex");

console.log(
  JSON.stringify(
    {
      schemaVersion: 1,
      server: "dovecot/dovecot:2.4.5",
      imageDigest: process.env.DAKIA_DOVECOT_IMAGE_DIGEST ?? null,
      fixtureSha256,
      fixtureUid: Number(uid),
      rawlogFileCount: files.length,
      selectiveCommandCount: normalizedCommands.length,
      productionMetadataFetchObserved: true,
      plainLeafFetchObserved: true,
      htmlLeafFetchObserved: true,
      fullRawFallbackObserved: false,
      mutatingStoreObserved: false,
      normalizedCommandSha256: commandDigest,
    },
    null,
    2,
  ),
);
