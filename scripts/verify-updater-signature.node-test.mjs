import assert from "node:assert/strict";
import {
  createHash,
  generateKeyPairSync,
  sign,
} from "node:crypto";
import test from "node:test";

import { verifyUpdaterSignature } from "./verify-updater-signature.mjs";

function updaterFixture() {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const publicDer = publicKey.export({ format: "der", type: "spki" });
  const keyId = Buffer.from("0123456789abcdef", "hex");
  const archive = Buffer.from("signed Dakia updater archive fixture");
  const archiveSignature = sign(
    null,
    createHash("blake2b512").update(archive).digest(),
    privateKey,
  );
  const trustedComment = "timestamp:1785571200\tfile:Dakia-aarch64.app.tar.gz\tprehashed";
  const globalSignature = sign(
    null,
    Buffer.concat([archiveSignature, Buffer.from(trustedComment)]),
    privateKey,
  );
  const publicText = [
    "untrusted comment: minisign public key fixture",
    Buffer.concat([Buffer.from("Ed"), keyId, publicDer.subarray(-32)]).toString(
      "base64",
    ),
  ].join("\n");
  const signatureText = [
    "untrusted comment: signature fixture",
    Buffer.concat([Buffer.from("ED"), keyId, archiveSignature]).toString(
      "base64",
    ),
    `trusted comment: ${trustedComment}`,
    globalSignature.toString("base64"),
  ].join("\n");
  return {
    archive,
    publicKey: Buffer.from(publicText).toString("base64"),
    signature: Buffer.from(signatureText).toString("base64"),
  };
}

test("cryptographically verifies a Tauri minisign updater archive", () => {
  assert.doesNotThrow(() => verifyUpdaterSignature(updaterFixture()));
});

test("rejects tampered updater bytes and trusted comments", () => {
  const fixture = updaterFixture();
  assert.throws(
    () =>
      verifyUpdaterSignature({
        ...fixture,
        archive: Buffer.from("tampered updater archive"),
      }),
    /archive signature verification failed/,
  );

  const decoded = Buffer.from(fixture.signature, "base64")
    .toString("utf8")
    .replace("timestamp:1785571200", "timestamp:1785571201");
  assert.throws(
    () =>
      verifyUpdaterSignature({
        ...fixture,
        signature: Buffer.from(decoded).toString("base64"),
      }),
    /trusted-comment signature verification failed/,
  );
});
