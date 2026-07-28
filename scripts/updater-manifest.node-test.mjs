import assert from "node:assert/strict";
import test from "node:test";
import { buildManifest, validateManifest } from "./updater-manifest.mjs";

const keyId = Buffer.from("0123456789abcdef", "hex");
const signatureRecord = Buffer.concat([
  Buffer.from("ED"),
  keyId,
  Buffer.alloc(64, 1),
]).toString("base64");
const publicKeyRecord = Buffer.concat([
  Buffer.from("Ed"),
  keyId,
  Buffer.alloc(32, 2),
]).toString("base64");
const encodedSignature = Buffer.from(
  [
    "untrusted comment: signature from minisign secret key",
    signatureRecord,
    "trusted comment: timestamp:1753444800\tfile:Dakia.app.tar.gz",
    Buffer.alloc(64, 3).toString("base64"),
  ].join("\n"),
).toString("base64");
const matchingPublicKey = Buffer.from(
  ["untrusted comment: minisign public key", publicKeyRecord].join("\n"),
).toString("base64");

const input = {
  version: "0.2.7",
  pubDate: "2026-07-25T12:00:00Z",
  notes: "Safer updates",
  aarch64Url:
    "https://downloads.dakiamail.com/macos/v0.2.7/Dakia-aarch64.app.tar.gz",
  aarch64Signature: encodedSignature,
};

test("builds the Apple Silicon updater platform entry", () => {
  const manifest = buildManifest(input);
  assert.equal(
    manifest.platforms["darwin-aarch64"].signature,
    encodedSignature,
  );
  assert.deepEqual(Object.keys(manifest.platforms), ["darwin-aarch64"]);
});

test("rejects architecture swaps", () => {
  const manifest = buildManifest(input);
  manifest.platforms["darwin-aarch64"].url =
    "https://downloads.dakiamail.com/macos/v0.2.7/Dakia-x86_64.app.tar.gz";
  assert.throws(() => validateManifest(manifest), /architecture mismatch/);
});

test("rejects missing and empty signatures", () => {
  assert.throws(
    () => buildManifest({ ...input, aarch64Signature: " " }),
    /Invalid updater signature/,
  );
  assert.throws(
    () => buildManifest({ ...input, aarch64Signature: "not-a-signature" }),
    /Invalid updater signature/,
  );
});

test("rejects non-SemVer versions", () => {
  assert.throws(
    () => buildManifest({ ...input, version: "release-seven" }),
    /Invalid updater SemVer/,
  );
  assert.throws(
    () => buildManifest({ ...input, version: "1.0.0-01" }),
    /Invalid updater SemVer/,
  );
});

test("rejects an unsupported Intel platform entry", () => {
  const manifest = buildManifest(input);
  manifest.platforms["darwin-x86_64"] = {
    url: "https://downloads.dakiamail.com/macos/v0.2.7/Dakia-x86_64.app.tar.gz",
    signature: encodedSignature,
  };
  assert.throws(() => validateManifest(manifest), /only darwin-aarch64/);
});

test("rejects a signature from a different updater key", () => {
  const differentRecord = Buffer.from(publicKeyRecord, "base64");
  differentRecord[2] ^= 1;
  const differentPublicKey = Buffer.from(
    [
      "untrusted comment: minisign public key",
      differentRecord.toString("base64"),
    ].join("\n"),
  ).toString("base64");
  assert.throws(
    () => buildManifest({ ...input, updaterPublicKey: differentPublicKey }),
    /does not match the embedded public key/,
  );
});

test("accepts a signature with the embedded updater key identifier", () => {
  assert.doesNotThrow(() =>
    buildManifest({ ...input, updaterPublicKey: matchingPublicKey }),
  );
});

test("rejects truncated or mistyped minisign records", () => {
  const truncatedSignature = Buffer.from(encodedSignature, "base64")
    .toString("utf8")
    .replace(signatureRecord, signatureRecord.slice(0, -4));
  assert.throws(
    () =>
      buildManifest({
        ...input,
        aarch64Signature: Buffer.from(truncatedSignature).toString("base64"),
        updaterPublicKey: matchingPublicKey,
      }),
    /Invalid updater signature/,
  );

  const mistypedPublicRecord = Buffer.from(publicKeyRecord, "base64");
  mistypedPublicRecord[1] = "D".charCodeAt(0);
  const mistypedPublicKey = Buffer.from(
    [
      "untrusted comment: minisign public key",
      mistypedPublicRecord.toString("base64"),
    ].join("\n"),
  ).toString("base64");
  assert.throws(
    () => buildManifest({ ...input, updaterPublicKey: mistypedPublicKey }),
    /Invalid updater public key/,
  );
});
