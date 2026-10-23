import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const root = new URL("..", import.meta.url).pathname;
const verifier = path.join(root, "scripts/verify-dovecot-selective-rawlog.mjs");
const fixture = path.join(
  root,
  "crates/dakia-core/testdata/mime/greenmail-2.1.14-mixed-alternative-leaf-mime-contradiction.eml",
);
const selectiveCommands = [
  "D0004 UID FETCH 1 (FLAGS INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (DATE FROM TO SUBJECT CONTENT-TYPE)])",
  "D0005 UID FETCH 1 (BODY.PEEK[1.MIME])",
  "D0006 UID FETCH 1 (BODY.PEEK[1.1.MIME])",
  "D0007 UID FETCH 1 (BODY.PEEK[1.2.MIME])",
  "D0008 UID FETCH 1 (BODY.PEEK[1.2])",
  "D0009 UID FETCH 1 (BODY.PEEK[1.1])",
];

async function verify(lines) {
  const rawlog = await mkdtemp(path.join(os.tmpdir(), "dakia-dovecot-rawlog-test-"));
  try {
    await writeFile(path.join(rawlog, "session.in"), `${lines.join("\r\n")}\r\n`);
    return await execFileAsync(process.execPath, [verifier, rawlog, fixture], {
      env: {
        ...process.env,
        DAKIA_DOVECOT_IMAGE_DIGEST: "fictional-test-digest",
      },
    });
  } finally {
    await rm(rawlog, { recursive: true, force: true });
  }
}

test("accepts the complete selective command set without raw fallback or mutation", async () => {
  const { stdout } = await verify(selectiveCommands);
  const evidence = JSON.parse(stdout);
  assert.equal(evidence.fixtureSha256, "948d62f3a3f1519475c6b0773cc37c2542a54625004d993b028c05b25b84f580");
  assert.equal(evidence.fixtureUid, 1);
  assert.equal(evidence.fullRawFallbackObserved, false);
  assert.equal(evidence.mutatingStoreObserved, false);
});

test("rejects the exact full-message recovery fetch", async () => {
  await assert.rejects(
    verify([
      ...selectiveCommands,
      "D0010 UID FETCH 1 (FLAGS INTERNALDATE BODY.PEEK[])",
    ]),
    /unexpected full-message recovery command/,
  );
});

test("rejects a mutating STORE in the read-only session", async () => {
  await assert.rejects(
    verify([...selectiveCommands, "D0010 UID STORE 1 +FLAGS.SILENT (\\Seen)"]),
    /mutating STORE command/,
  );
});
